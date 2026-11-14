#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
#[path = "nitro_copy_fs/mod.rs"]
mod nitro_copy_fs;
#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
mod probe {
    use krun::{Builder, NitroBackendConfig, NitroSerialization, TemplateSource, VmExit};
    use serde_json::{json, Value};
    use std::{
        io::{self, Read, Write},
        os::unix::net::{UnixListener, UnixStream},
        path::Path,
        time::{Duration, Instant},
    };
    fn send(stream: &mut UnixStream, value: Value) -> io::Result<()> {
        let bytes = serde_json::to_vec(&value)?;
        stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        stream.write_all(&bytes)
    }
    fn receive(stream: &mut UnixStream) -> io::Result<Value> {
        let mut header = [0; 4];
        stream.read_exact(&mut header)?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > 1024 * 1024 {
            return Err(io::Error::other("invalid agent frame"));
        }
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes)?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn response(stream: &mut UnixStream, kind: &str) -> io::Result<Value> {
        for _ in 0..32 {
            let value = receive(stream)?;
            if value["type"] == kind {
                return Ok(value);
            }
            if value["type"] != "heartbeat" && value["type"] != "exit" {
                return Err(io::Error::other(format!(
                    "unexpected agent response: {value}"
                )));
            }
        }
        Err(io::Error::other("agent response budget exhausted"))
    }
    fn accept(listener: &UnixListener) -> io::Result<UnixStream> {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    return Ok(stream);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::other("agent connection deadline"));
                    }
                    std::thread::yield_now();
                }
                Err(e) => return Err(e),
            }
        }
    }
    fn line(stream: &mut UnixStream) -> io::Result<String> {
        let mut bytes = Vec::new();
        for _ in 0..2048 {
            let mut byte = [0];
            stream.read_exact(&mut byte)?;
            if byte[0] == b'\n' {
                return String::from_utf8(bytes).map_err(io::Error::other);
            }
            bytes.push(byte[0]);
        }
        Err(io::Error::other("workload line too long"))
    }
    fn verify_workload(listener: &UnixListener, cid: u64) -> io::Result<String> {
        let mut stream = accept(listener)?;
        let report = line(&mut stream)?;
        let fields: std::collections::HashMap<_, _> = report
            .split_whitespace()
            .filter_map(|s| s.split_once('='))
            .collect();
        let number = |key: &str| -> io::Result<u64> {
            fields
                .get(key)
                .ok_or_else(|| io::Error::other("missing workload field"))?
                .parse()
                .map_err(io::Error::other)
        };
        let old = fields
            .get("old_result")
            .ok_or_else(|| io::Error::other("missing old stream result"))?
            .parse::<i64>()
            .map_err(io::Error::other)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos() as u64;
        if std::env::var_os("AGENT_TIMER_SECS").is_some()
            && (number("timers")? != 1
                || number("expirations")? != 1
                || number("counter")? != 42
                || number("timer_late_ns")? > 2_000_000_000
                || number("sleep_late_ns")? > 2_000_000_000)
        {
            return Err(io::Error::other(format!(
                "timer continuation failed: {report}"
            )));
        }
        if !report.starts_with("VERIFIED ")
            || fields.get("random") == fields.get("nonce")
            || number("bad")? != 0
            || (std::env::var_os("AGENT_DEVICES").is_some()
                && (number("device_bad")? != 0
                    || number("writes")? == 0
                    || number("traffic")? == 0))
            || number("cid")? != cid
            || old != -1
            || number("old_errno")? != libc::ENOTCONN as u64
            || number("monotonic_after")? < number("monotonic_before")?
            || now.abs_diff(number("realtime_after")?) > 2_000_000_000
        {
            return Err(io::Error::other(format!(
                "workload verification failed: {report}"
            )));
        }
        println!("WORKLOAD_COPY {report}");
        Ok(fields
            .get("random")
            .ok_or_else(|| io::Error::other("missing random"))?
            .to_string())
    }
    fn attach_devices(builder: &mut Builder, disk: &Path, fs: super::nitro_copy_fs::Tree) {
        builder.add_block_cfg(krun::BlockDeviceConfig {
            block_id: "data".into(),
            cache_type: devices::virtio::block::device::CacheType::Writeback,
            disk_type: krun::BlockDeviceType::Image {
                path: disk.to_string_lossy().into_owned(),
                format: krun::ImageType::Raw,
                sync_mode: krun::SyncMode::Full,
            },
            is_disk_read_only: false,
            direct_io: false,
        });
        builder.template_disk_identity("data", "acceptance-frozen-disk-v1");
        builder.add_virtiofs("copyfs", Box::new(fs), None);
    }
    fn builder(image: &str, cid: u64, socket: &Path) -> io::Result<Builder> {
        let mut builder = Builder::new();
        builder.nitro_backend(NitroBackendConfig {
            image: image.into(),
            cid,
            watchdog: Duration::from_secs(20),
        });
        // NITRO_SERIALIZATION=teardown serializes only teardowns (used to show a
        // retained resource without holding the host lock across a long wait).
        let serialization = match std::env::var("NITRO_SERIALIZATION").as_deref() {
            Ok("teardown") => NitroSerialization::Teardown,
            _ => NitroSerialization::StartAndTeardown,
        };
        builder.nitro_lifecycle_lock(
            nitro_sandbox::lifecycle::LIFECYCLE_LOCK_PATH.into(),
            serialization,
        );
        if let Some(path) = std::env::var_os("AGENT_DESCRIPTOR_HANDOFF") {
            let stream = UnixStream::connect(path)?;
            builder.nitro_descriptor_handoff(stream);
            eprintln!("AGENT_DESCRIPTOR_HANDOFF_CONFIGURED cid={cid}");
        }
        let cpus = std::env::var("COPY_CPUS")
            .unwrap_or_else(|_| "1".into())
            .parse::<u8>()
            .map_err(io::Error::other)?;
        let ram = std::env::var("COPY_MEMORY_MIB")
            .unwrap_or_else(|_| "1024".into())
            .parse::<u32>()
            .map_err(io::Error::other)?;
        if ![1, 4, 8].contains(&cpus) || ![1024, 4096].contains(&ram) {
            return Err(io::Error::other("unsupported acceptance shape"));
        }
        builder.vm_config(cpus, ram).map_err(io::Error::other)?;
        builder
            .disable_implicit_console()
            .map_err(io::Error::other)?;
        builder.nitro_console_sink(
            Box::new(std::fs::File::create(format!("agent-console-{cid}.log"))?),
            1024 * 1024,
            1024 * 1024,
        );
        builder.add_vsock_port(7000, socket.into(), false);
        if std::env::var_os("AGENT_WORKLOAD").is_some() {
            builder.add_vsock_port(7003, socket.with_extension("workload.sock"), false);
        }
        Ok(builder)
    }
    fn restore_child(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
        use std::os::fd::FromRawFd;
        let cid = args[3].parse::<u64>()?;
        if !(700..=899).contains(&cid) {
            return Err("CID outside lane range".into());
        }
        let directory = std::path::PathBuf::from(&args[5]);
        let socket = directory.join(format!("copy-{cid}.sock"));
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        // SAFETY: parent pre_exec duplicates only the sealed template to fd100.
        let file = unsafe { std::fs::File::from_raw_fd(100) };
        let workload_listener = if std::env::var_os("AGENT_WORKLOAD").is_some() {
            let l = UnixListener::bind(socket.with_extension("workload.sock"))?;
            l.set_nonblocking(true)?;
            Some(l)
        } else {
            None
        };
        let mut receiver = builder(&args[2], cid, &socket)?;
        if std::env::var_os("AGENT_DEVICES").is_some() {
            let disk = directory.join(format!("disk-{cid}"));
            std::fs::copy(directory.join("frozen-disk"), &disk)?;
            let fs = super::nitro_copy_fs::Tree::new(std::fs::read(directory.join("frozen-fs"))?);
            attach_devices(&mut receiver, &disk, fs);
        }
        let _readiness = receiver.start_from_template(
            TemplateSource::Descriptor(file),
            Duration::from_secs(20),
            |_| Ok(()),
        )?;
        let context = receiver.build()?;
        let boot_id = args[4].clone();
        let worker = std::thread::spawn(move || -> io::Result<()> {
            let mut connection = accept(&listener)?;
            let hello = response(&mut connection, "hello")?;
            if hello["boot_id"] != boot_id
                || hello["token"] != "clone-test-token"
                || hello["generation"] != 0
            {
                return Err(io::Error::other("copy identity mismatch"));
            }
            send(
                &mut connection,
                json!({"type":"welcome","version":3,"features":[]}),
            )?;
            send(
                &mut connection,
                json!({"type":"copied","request":3,"generation":1}),
            )?;
            response(&mut connection, "ack")?;
            let result = if let Some(listener) = workload_listener {
                verify_workload(&listener, cid)?
            } else {
                "ready".into()
            };
            std::fs::write(directory.join(format!("ready-{cid}")), result)?;
            let deadline = Instant::now() + Duration::from_secs(12);
            while !directory.join("release").exists() {
                if Instant::now() >= deadline {
                    return Err(io::Error::other("concurrent release deadline"));
                }
                std::thread::yield_now();
            }
            send(&mut connection, json!({"type":"power_off","request":4}))?;
            response(&mut connection, "ack")?;
            Ok(())
        });
        let _exit = context.run()?;
        worker.join().map_err(|_| "copy worker panicked")??;
        std::fs::remove_file(socket)?;
        Ok(())
    }
    fn wait_until(what: &str, limit: Duration, done: impl Fn() -> bool) -> io::Result<()> {
        let deadline = Instant::now() + limit;
        while !done() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, what.to_string()));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
    /// Guest resources this process itself holds: the Nitro device handle, the
    /// VM descriptor (an anonymous inode other than the event/poll/timer kinds),
    /// the guest info page and the lock file descriptor, and mappings of the
    /// Nitro device (the guest's pool memory) or the info page. Global counters
    /// are shared with other lanes; these are not.
    fn own_nitro_resources() -> io::Result<(usize, usize, Vec<String>)> {
        let mut targets = Vec::new();
        for entry in std::fs::read_dir("/proc/self/fd")? {
            let Ok(target) = std::fs::read_link(entry?.path()) else {
                continue;
            };
            let target = target.to_string_lossy().into_owned();
            let ordinary_anon = [
                "[eventfd]",
                "[eventpoll]",
                "[timerfd]",
                "[signalfd]",
                "[pidfd]",
            ]
            .iter()
            .any(|kind| target.ends_with(kind));
            if target == "/dev/nitro_enclaves"
                || target.contains("libkrun-nitro-info")
                || target == nitro_sandbox::lifecycle::LIFECYCLE_LOCK_PATH
                || (target.starts_with("anon_inode:") && !ordinary_anon)
            {
                targets.push(target);
            }
        }
        let mappings = std::fs::read_to_string("/proc/self/maps")?
            .lines()
            .filter(|line| {
                line.contains("/dev/nitro_enclaves") || line.contains("libkrun-nitro-info")
            })
            .count();
        Ok((targets.len(), mappings, targets))
    }
    /// Try the host-wide lock without waiting, from a separate open file
    /// description (so a holder in this same process also counts), and release
    /// it at once. Counts how many of `samples` attempts found it free.
    fn lock_free_samples(samples: u32) -> io::Result<u32> {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(nitro_sandbox::lifecycle::LIFECYCLE_LOCK_PATH)?;
        let mut free = 0;
        for _ in 0..samples {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                free += 1;
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(free)
    }
    fn pool_counters() -> String {
        let read = |name: &str| {
            std::fs::read_to_string(format!("/sys/class/misc/nitro_enclaves/{name}"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "?".into())
        };
        format!(
            "slots={} pool_in_use={}",
            read("pcie_slots_in_use"),
            read("mem_pool/in_use")
        )
    }
    /// A parked receiver that is built but never run (PARK_MODE=unrun), whose
    /// handle is dropped while unrun (unrun-drop), or whose context is dropped
    /// unrun (unrun-ctxdrop). It must not hold the host lock after build, and
    /// must give back its VM descriptor, device handle and pool memory within
    /// its park wait (or at once when dropped), with the typed reason from
    /// `run`.
    fn unrun_child(
        receiver: Builder,
        mode: &str,
        wait: Duration,
        cid: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let before = own_nitro_resources()?;
        println!(
            "UNRUN_BEFORE cid={cid} own={:?} {}",
            (before.0, before.1),
            pool_counters()
        );
        let built = Instant::now();
        let mut context = receiver.build()?;
        let build_ms = built.elapsed().as_millis();
        let assignment = context
            .take_template_assignment()
            .ok_or("parked context has no assignment handle")?;
        let after_build = own_nitro_resources()?;
        let lock_free = lock_free_samples(10)?;
        println!(
            "UNRUN_BUILT cid={cid} build_ms={build_ms} own={:?} targets={:?} lock_free={lock_free}/10 {}",
            (after_build.0, after_build.1),
            after_build.2,
            pool_counters()
        );
        let mut failures = Vec::new();
        if after_build.0 == 0 || after_build.1 == 0 {
            failures.push("no Nitro resources observed after build".to_string());
        }
        // Another lane may hold the lock for one start or teardown at a time;
        // a lock held by this unrun receiver is held in every sample.
        if lock_free == 0 {
            failures.push("host lock held throughout after build".to_string());
        }
        let expected = match mode {
            "unrun" => Some((io::ErrorKind::TimedOut, krun::ParkedReceiverError::Expired)),
            "unrun-drop" => Some((
                io::ErrorKind::ConnectionAborted,
                krun::ParkedReceiverError::Abandoned,
            )),
            "unrun-ctxdrop" => None,
            _ => return Err(format!("unknown PARK_MODE {mode}").into()),
        };
        let observe_at = match mode {
            "unrun" => wait + Duration::from_secs(6),
            _ => Duration::from_secs(3),
        };
        let mut context = Some(context);
        let mut assignment = Some(assignment);
        match mode {
            "unrun-drop" => drop(assignment.take()),
            "unrun-ctxdrop" => {
                let dropped = Instant::now();
                drop(context.take());
                println!(
                    "UNRUN_CONTEXT_DROPPED cid={cid} drop_ms={}",
                    dropped.elapsed().as_millis()
                );
            }
            _ => {}
        }
        while built.elapsed() < observe_at {
            std::thread::sleep(Duration::from_millis(100));
        }
        let later = own_nitro_resources()?;
        println!(
            "UNRUN_OBSERVED cid={cid} after_ms={} own={:?} targets={:?} {}",
            built.elapsed().as_millis(),
            (later.0, later.1),
            later.2,
            pool_counters()
        );
        if (later.0, later.1) != (before.0, before.1) {
            failures.push(format!(
                "resources still held {} ms after build: {:?}",
                built.elapsed().as_millis(),
                (later.0, later.1)
            ));
        }
        if let Some(context) = context.take() {
            let ran = Instant::now();
            let result = context.run();
            let run_ms = ran.elapsed().as_millis();
            match (&result, expected) {
                (Err(krun::StartError::Nitro(error)), Some((kind, reason)))
                    if error.kind() == kind
                        && error
                            .get_ref()
                            .and_then(|e| e.downcast_ref::<krun::ParkedReceiverError>())
                            == Some(&reason) =>
                {
                    println!("UNRUN_RUN_RESULT cid={cid} typed={reason:?} run_ms={run_ms}");
                }
                _ => failures.push(format!("run returned {result:?} after {run_ms} ms")),
            }
        }
        drop(assignment);
        let end = own_nitro_resources()?;
        println!(
            "UNRUN_END cid={cid} own={:?} {}",
            (end.0, end.1),
            pool_counters()
        );
        if failures.is_empty() {
            println!("UNRUN_PASS cid={cid} mode={mode}");
            Ok(())
        } else {
            println!("UNRUN_FAIL cid={cid} mode={mode} {failures:?}");
            Err(failures.join("; ").into())
        }
    }
    /// Open the template the parent offered in `directory` (a fresh descriptor).
    fn offered_template(directory: &Path) -> io::Result<std::fs::File> {
        let offer = directory.join("template-ready");
        wait_until("template offer", Duration::from_secs(90), || offer.exists())?;
        let offer = std::fs::read_to_string(offer)?;
        let fields: Vec<_> = offer.lines().collect();
        let [pid, fd, _] = fields[..] else {
            return Err(io::Error::other("malformed template offer"));
        };
        std::fs::File::open(format!("/proc/{pid}/fd/{fd}"))
    }
    fn typed<T: std::error::Error + Copy + 'static>(error: &io::Error) -> Option<T> {
        error.get_ref().and_then(|e| e.downcast_ref::<T>()).copied()
    }
    /// PARK_MODE=mismatch MISMATCH_KIND=cpu|memory|devices|kernel: a parked
    /// receiver whose shape, device list or kernel differs from the offered
    /// template. CPU, memory and device differences must be refused by
    /// `assign` with their `RestoreMismatch` while the receiver stays parked;
    /// a different kernel is refused after READY with `KernelIdentity` and
    /// ends the receiver. In every case no host write reaches receiver memory
    /// (`receiver_writes` stays 0) and the receiver's descriptors and pool
    /// mapping are released. Late assignments after the end must be refused
    /// with `Closed`: after a stop request (memory case) and after the
    /// mismatch error (kernel case).
    fn mismatch_child(
        receiver: Builder,
        cid: u64,
        directory: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let kind = std::env::var("MISMATCH_KIND")?;
        let expected = match kind.as_str() {
            "cpu" => krun::RestoreMismatch::CpuCount,
            "memory" => krun::RestoreMismatch::MemorySize,
            "devices" => krun::RestoreMismatch::DeviceList,
            "kernel" => krun::RestoreMismatch::KernelIdentity,
            _ => return Err(format!("unknown MISMATCH_KIND {kind}").into()),
        };
        let before = own_nitro_resources()?;
        let mut context = receiver.build()?;
        let handle = context.vm_handle();
        let assignment = context
            .take_template_assignment()
            .ok_or("parked context has no assignment handle")?;
        wait_until("receiver READY", Duration::from_secs(15), || {
            assignment.receiver_ready()
        })?;
        std::fs::write(directory.join(format!("parked-{cid}")), b"parked")?;
        let parked = own_nitro_resources()?;
        let template = offered_template(directory)?;
        let mut failures = Vec::new();
        let writes_before = vmm::nitro_restore::receiver_writes();
        let assigned = assignment.assign(TemplateSource::Descriptor(template));
        let mut assignment = Some(assignment);
        let late_mode;
        let run_result = match (&assigned, kind.as_str()) {
            (Ok(()), "kernel") => {
                late_mode = "errored";
                let result = context.run();
                match &result {
                    Err(krun::StartError::Nitro(error))
                        if typed::<krun::RestoreMismatch>(error) == Some(expected) =>
                    {
                        println!("MISMATCH_REFUSED cid={cid} kind={kind} where=runtime typed={expected:?}");
                    }
                    other => failures.push(format!("kernel mismatch run ended {other:?}")),
                }
                format!("{result:?}")
            }
            (Err(error), _)
                if kind != "kernel" && typed::<krun::RestoreMismatch>(error) == Some(expected) =>
            {
                let still_parked = assignment.as_ref().is_some_and(|a| a.receiver_ready())
                    && own_nitro_resources()?.1 == parked.1;
                println!(
                    "MISMATCH_REFUSED cid={cid} kind={kind} where=assign typed={expected:?} still_parked={still_parked}"
                );
                if !still_parked {
                    failures.push("receiver did not stay parked after the refusal".into());
                }
                if kind == "cpu" {
                    // End it by dropping the only handle: typed Abandoned.
                    late_mode = "";
                    drop(assignment.take());
                    let result = context.run();
                    match &result {
                        Err(krun::StartError::Nitro(error))
                            if typed::<krun::ParkedReceiverError>(error)
                                == Some(krun::ParkedReceiverError::Abandoned) => {}
                        other => failures.push(format!("abandoned after refusal ended {other:?}")),
                    }
                    format!("{result:?}")
                } else {
                    // End it by a stop request, then try a late assignment.
                    late_mode = "stopped";
                    handle.trigger_shutdown_event()?;
                    let result = context.run();
                    if !matches!(result, Ok(VmExit::Stopped)) {
                        failures.push(format!("stop after refusal ended {result:?}"));
                    }
                    format!("{result:?}")
                }
            }
            other => {
                late_mode = "";
                failures.push(format!("assign returned {:?}", other.0));
                let _ = handle.trigger_shutdown_event();
                format!("{:?}", context.run())
            }
        };
        let writes = vmm::nitro_restore::receiver_writes() - writes_before;
        if writes != 0 {
            failures.push(format!(
                "{writes} receiver writes after a refused assignment"
            ));
        }
        if let (false, Some(assignment)) = (late_mode.is_empty(), assignment.as_ref()) {
            let late = assignment.assign(TemplateSource::Descriptor(offered_template(directory)?));
            match &late {
                Err(error)
                    if typed::<krun::ParkedReceiverError>(error)
                        == Some(krun::ParkedReceiverError::Closed) =>
                {
                    println!("LATE_ASSIGN_REFUSED cid={cid} after={late_mode} typed=Closed");
                }
                other => failures.push(format!("late assign after {late_mode} returned {other:?}")),
            }
        }
        drop(assignment);
        let after = own_nitro_resources()?;
        println!(
            "MISMATCH_RESULT cid={cid} kind={kind} run={run_result} receiver_writes={writes} own_before={:?} own_parked={:?} own_after={:?}",
            (before.0, before.1),
            (parked.0, parked.1),
            (after.0, after.1)
        );
        if (after.0, after.1) != (before.0, before.1) || parked.1 == 0 {
            failures.push("receiver resources not released".into());
        }
        if failures.is_empty() {
            println!("MISMATCH_PASS cid={cid} kind={kind}");
            Ok(())
        } else {
            println!("MISMATCH_FAIL cid={cid} kind={kind} {failures:?}");
            Err(failures.join("; ").into())
        }
    }
    /// One receiver parked before any template exists. PARK_MODE=assign (default)
    /// waits for the parent's template, expire never assigns, drop releases the
    /// handle unassigned. PARK_WAIT_SECS bounds the park (default 60).
    fn parked_child(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::FileExt;
        let cid = args[3].parse::<u64>()?;
        if !(700..=899).contains(&cid) {
            return Err("CID outside lane range".into());
        }
        let directory = std::path::PathBuf::from(&args[4]);
        let socket = directory.join(format!("copy-{cid}.sock"));
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let workload_listener = if std::env::var_os("AGENT_WORKLOAD").is_some() {
            let l = UnixListener::bind(socket.with_extension("workload.sock"))?;
            l.set_nonblocking(true)?;
            Some(l)
        } else {
            None
        };
        let mode = std::env::var("PARK_MODE").unwrap_or_else(|_| "assign".into());
        let wait = Duration::from_secs(
            std::env::var("PARK_WAIT_SECS")
                .unwrap_or_else(|_| "60".into())
                .parse()?,
        );
        let devices = std::env::var_os("AGENT_DEVICES").is_some();
        let disk = directory.join(format!("disk-{cid}"));
        let fs = super::nitro_copy_fs::Tree::new(vec![0; 4096]);
        let mut receiver = builder(&args[2], cid, &socket)?;
        if devices {
            std::fs::File::create(&disk)?.set_len(16 * 1024 * 1024)?;
            attach_devices(&mut receiver, &disk, fs.clone());
        }
        receiver.park_for_template(wait);
        if mode.starts_with("unrun") {
            let result = unrun_child(receiver, &mode, wait, cid);
            let _ = std::fs::remove_file(&socket);
            return result;
        }
        if mode == "mismatch" {
            let result = mismatch_child(receiver, cid, &directory);
            let _ = std::fs::remove_file(&socket);
            return result;
        }
        let mut context = receiver.build()?;
        let assignment = context
            .take_template_assignment()
            .ok_or("parked context has no assignment handle")?;
        let parked_marker = directory.join(format!("parked-{cid}"));
        if mode == "expire" || mode == "drop" {
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            let drop_it = mode == "drop";
            let holder = std::thread::spawn(move || -> io::Result<()> {
                wait_until("receiver READY", Duration::from_secs(15), || {
                    assignment.receiver_ready()
                })?;
                std::fs::write(&parked_marker, b"parked")?;
                if drop_it {
                    std::thread::sleep(Duration::from_secs(1));
                    drop(assignment);
                } else {
                    let _ = done_rx.recv_timeout(wait + Duration::from_secs(30));
                    drop(assignment);
                }
                Ok(())
            });
            let ended = Instant::now();
            let result = context.run();
            let _ = done_tx.send(());
            holder.join().map_err(|_| "holder panicked")??;
            let (kind, expected) = if drop_it {
                (
                    io::ErrorKind::ConnectionAborted,
                    krun::ParkedReceiverError::Abandoned,
                )
            } else {
                (io::ErrorKind::TimedOut, krun::ParkedReceiverError::Expired)
            };
            match result {
                Err(krun::StartError::Nitro(error))
                    if error.kind() == kind
                        && error
                            .get_ref()
                            .and_then(|e| e.downcast_ref::<krun::ParkedReceiverError>())
                            == Some(&expected) =>
                {
                    println!(
                        "PARKED_ENDED cid={cid} mode={mode} typed={expected:?} run_ms={}",
                        ended.elapsed().as_millis()
                    );
                }
                other => return Err(format!("parked {mode} ended unexpectedly: {other:?}").into()),
            }
            std::fs::remove_file(&socket)?;
            return Ok(());
        }
        let late_directory = directory.clone();
        let worker = std::thread::spawn(move || -> io::Result<krun::TemplateAssignment> {
            wait_until("receiver READY", Duration::from_secs(15), || {
                assignment.receiver_ready()
            })?;
            std::fs::write(&parked_marker, b"parked")?;
            let offer = directory.join("template-ready");
            wait_until("template offer", Duration::from_secs(90), || offer.exists())?;
            let offer = std::fs::read_to_string(offer)?;
            let fields: Vec<_> = offer.lines().collect();
            let [pid, fd, boot_id] = fields[..] else {
                return Err(io::Error::other("malformed template offer"));
            };
            if devices {
                let bytes = std::fs::read(directory.join("frozen-disk"))?;
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&disk)?
                    .write_all_at(&bytes, 0)?;
                *fs.0.lock().unwrap() = std::fs::read(directory.join("frozen-fs"))?;
            }
            let file = std::fs::File::open(format!("/proc/{pid}/fd/{fd}"))?;
            let assigned = Instant::now();
            assignment.assign(TemplateSource::Descriptor(file))?;
            let mut connection = accept(&listener)?;
            let hello = response(&mut connection, "hello")?;
            println!(
                "PARKED_ASSIGN_TO_HELLO cid={cid} us={}",
                assigned.elapsed().as_micros()
            );
            if hello["boot_id"] != boot_id
                || hello["token"] != "clone-test-token"
                || hello["generation"] != 0
            {
                return Err(io::Error::other("copy identity mismatch"));
            }
            send(
                &mut connection,
                json!({"type":"welcome","version":3,"features":[]}),
            )?;
            send(
                &mut connection,
                json!({"type":"copied","request":3,"generation":1}),
            )?;
            response(&mut connection, "ack")?;
            let result = if let Some(listener) = workload_listener {
                verify_workload(&listener, cid)?
            } else {
                "ready".into()
            };
            std::fs::write(directory.join(format!("ready-{cid}")), result)?;
            let release = directory.join("release");
            wait_until("parked release", Duration::from_secs(30), || {
                release.exists()
            })?;
            send(&mut connection, json!({"type":"power_off","request":4}))?;
            response(&mut connection, "ack")?;
            Ok(assignment)
        });
        let run = context.run();
        let assignment = worker.join().map_err(|_| "parked worker panicked")??;
        let exit = run?;
        // Positive control for the refusal tests: a real import writes.
        println!(
            "IMPORT_RECEIVER_WRITES cid={cid} writes={} exit={exit:?}",
            vmm::nitro_restore::receiver_writes()
        );
        if vmm::nitro_restore::receiver_writes() == 0 {
            return Err("an accepted import recorded no receiver writes".into());
        }
        if std::env::var_os("LATE_ASSIGN").is_some() {
            // The copy has shut down: a late assignment must be refused, never Ok.
            let late = assignment.assign(TemplateSource::Descriptor(offered_template(
                &late_directory,
            )?));
            match &late {
                Err(error)
                    if typed::<krun::ParkedReceiverError>(error)
                        == Some(krun::ParkedReceiverError::Closed) =>
                {
                    println!("LATE_ASSIGN_REFUSED cid={cid} after=shutdown typed=Closed");
                }
                other => {
                    return Err(format!("late assign after shutdown returned {other:?}").into())
                }
            }
        }
        std::fs::remove_file(socket)?;
        Ok(())
    }
    fn spawn_parked(receiver: &str, cid: u64, directory: &Path) -> io::Result<std::process::Child> {
        std::process::Command::new(std::env::current_exe()?)
            .args([
                "--parked-child",
                receiver,
                &cid.to_string(),
                directory
                    .to_str()
                    .ok_or_else(|| io::Error::other("nonutf8"))?,
            ])
            .spawn()
    }
    fn wait_parked(
        directory: &Path,
        cids: std::ops::Range<u64>,
        children: &mut [std::process::Child],
    ) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !cids
            .clone()
            .all(|cid| directory.join(format!("parked-{cid}")).exists())
        {
            if Instant::now() >= deadline
                || children
                    .iter_mut()
                    .any(|child| child.try_wait().ok().flatten().is_some())
            {
                for child in children.iter_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err(io::Error::other("parked receivers not READY in time"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args: Vec<_> = std::env::args().collect();
        if args.len() == 6 && args[1] == "--restore-child" {
            return restore_child(&args);
        }
        if args.len() == 5 && args[1] == "--parked-child" {
            return parked_child(&args);
        }
        if args.len() != 3 {
            return Err("usage: nitro-agent-copy agent-source.eif receiver.eif".into());
        }
        let socket =
            std::env::current_dir()?.join(format!("agent-copy-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let workload_listener = if std::env::var_os("AGENT_WORKLOAD").is_some() {
            let l = UnixListener::bind(socket.with_extension("workload.sock"))?;
            l.set_nonblocking(true)?;
            Some(l)
        } else {
            None
        };
        let source_workload = workload_listener
            .as_ref()
            .map(UnixListener::try_clone)
            .transpose()?;
        let full_devices = std::env::var_os("AGENT_DEVICES").is_some();
        // AGENT_PARKED=N parks receivers CIDs 760.. before the source exists. With
        // the 4-guest limit, N=4 parks 3 before the source and the 4th after the
        // source closes but before any template is assigned.
        let parked = std::env::var("AGENT_PARKED")
            .ok()
            .map(|n| n.parse::<u64>())
            .transpose()?
            .unwrap_or(0);
        if !(0..=4).contains(&parked) {
            return Err("AGENT_PARKED must be0..4".into());
        }
        let park_directory =
            std::env::current_dir()?.join(format!("agent-parked-{}", std::process::id()));
        let early = if std::env::var_os("AGENT_PARKED_AFTER_SOURCE").is_some() {
            0
        } else {
            parked.min(3)
        };
        let mut parked_children = Vec::new();
        if parked > 0 {
            std::fs::create_dir(&park_directory)?;
            for cid in 760..760 + early {
                parked_children.push(spawn_parked(&args[2], cid, &park_directory)?);
            }
            wait_parked(&park_directory, 760..760 + early, &mut parked_children)?;
            eprintln!("AGENT_PARKED_BEFORE_SOURCE count={early}");
        }
        let source_disk = socket.with_extension("source-disk");
        let source_fs = super::nitro_copy_fs::Tree::new(vec![b'A'; 4096]);
        let source_cid = std::env::var("AGENT_SOURCE_CID")
            .unwrap_or_else(|_| "711".into())
            .parse::<u64>()?;
        if !(700..=899).contains(&source_cid) {
            return Err("source CID outside lane range".into());
        }
        let mut source = builder(&args[1], source_cid, &socket)?;
        if full_devices {
            std::fs::File::create(&source_disk)?.set_len(16 * 1024 * 1024)?;
            attach_devices(&mut source, &source_disk, source_fs.clone());
        }
        source.future_template();
        // RSS_SAMPLES=<file>: record this VMM process's resident memory every
        // 100 ms from source build until the template is captured.
        let rss_epoch = Instant::now();
        let mut rss_sampler = match std::env::var("RSS_SAMPLES") {
            Ok(path) => {
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let stopped = stop.clone();
                let thread = std::thread::spawn(move || -> io::Result<()> {
                    let mut out = std::fs::File::create(path)?;
                    let started = rss_epoch;
                    while !stopped.load(std::sync::atomic::Ordering::SeqCst) {
                        let status = std::fs::read_to_string("/proc/self/status")?;
                        let field = |name: &str| {
                            status
                                .lines()
                                .find(|l| l.starts_with(name))
                                .and_then(|l| l.split_whitespace().nth(1))
                                .unwrap_or("?")
                                .to_string()
                        };
                        writeln!(
                            out,
                            "{} {} {}",
                            started.elapsed().as_millis(),
                            field("VmRSS:"),
                            field("RssAnon:")
                        )?;
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Ok(())
                });
                Some((stop, thread))
            }
            Err(_) => None,
        };
        eprintln!("AGENT_SOURCE_BUILD");
        let context = source.build()?;
        eprintln!("AGENT_SOURCE_RUN");
        let handle = context.vm_handle();
        let arm_handle = context.vm_handle();
        let (freeze_tx, freeze_rx) = std::sync::mpsc::sync_channel(1);
        let source_listener = listener.try_clone()?;
        let source_thread = std::thread::spawn(move || -> io::Result<Value> {
            let mut connection = accept(&source_listener)?;
            let hello = response(&mut connection, "hello")?;
            eprintln!("AGENT_SOURCE_HELLO");
            if hello["version"] != 3 || hello["generation"] != 0 {
                return Err(io::Error::other("invalid initial agent identity"));
            }
            send(
                &mut connection,
                json!({"type":"welcome","version":3,"features":[],"token":"clone-test-token"}),
            )?;
            let mut workload_connection = None;
            if let Some(listener) = source_workload {
                send(
                    &mut connection,
                    json!({"type":"exec","request":9,"process":1,"command":{"argv":if let Ok(seconds)=std::env::var("AGENT_TIMER_SECS") {vec!["/clone-workload".to_string(),"devices".into(),"timers".into(),seconds]}else if let Ok(seconds)=std::env::var("AGENT_CONSOLE_INJECT_SECS") {vec!["/clone-workload".into(),"devices".into(),"inject".into(),seconds]}else if std::env::var_os("AGENT_CONSOLE_INJECT").is_some() {vec!["/clone-workload".into(),"devices".into(),"inject".into()]}else if full_devices {vec!["/clone-workload".into(),"devices".into()]}else{vec!["/clone-workload".to_string()]}}}),
                )?;
                response(&mut connection, "ack")?;
                let mut stream = accept(&listener)?;
                let ready = line(&mut stream)?;
                if !ready.starts_with("READY ") {
                    return Err(io::Error::other("workload not ready"));
                }
                eprintln!("WORKLOAD_SOURCE {ready}");
                workload_connection = Some(stream);
                if let Ok(seconds) = std::env::var("AGENT_CONSOLE_INJECT_SECS") {
                    // The guest floods its console and kernel log from a thread
                    // for this long; wait it out (plus margin) before quiescing.
                    let seconds: u64 = seconds.parse().map_err(io::Error::other)?;
                    eprintln!(
                        "AGENT_FLOOD_STARTED secs={seconds} at_ms={}",
                        rss_epoch.elapsed().as_millis()
                    );
                    std::thread::sleep(Duration::from_secs(seconds + 3));
                    eprintln!(
                        "AGENT_FLOOD_WAITED at_ms={}",
                        rss_epoch.elapsed().as_millis()
                    );
                }
            }
            send(
                &mut connection,
                json!({"type":"list_processes","request":1}),
            )?;
            let processes = response(&mut connection, "processes")?;
            if processes["processes"]
                .as_array()
                .is_none_or(|p| p.len() != usize::from(workload_connection.is_some()))
            {
                return Err(io::Error::other("source workload not quiescent"));
            }
            eprintln!("AGENT_SOURCE_QUIESCENT");
            if std::env::var_os("AGENT_RECEIVER_FAIL").is_some() {
                // A copy's receiver fails to start (corrupt image) while the source
                // runs; the source must stay usable and still be capturable.
                let directory =
                    std::env::current_dir()?.join(format!("agent-recvfail-{}", std::process::id()));
                std::fs::create_dir_all(&directory)?;
                let bad = directory.join("corrupt-receiver.eif");
                std::fs::write(&bad, vec![0u8; 4096])?;
                let log = std::fs::File::create(directory.join("receiver-failure.log"))?;
                let mut child = std::process::Command::new(std::env::current_exe()?)
                    .args([
                        "--parked-child",
                        bad.to_str().ok_or_else(|| io::Error::other("nonutf8"))?,
                        "765",
                        directory
                            .to_str()
                            .ok_or_else(|| io::Error::other("nonutf8"))?,
                    ])
                    .env("PARK_MODE", "expire")
                    .env("PARK_WAIT_SECS", "5")
                    .stdout(std::process::Stdio::null())
                    .stderr(log)
                    .spawn()?;
                let deadline = Instant::now() + Duration::from_secs(40);
                let status = loop {
                    if let Some(status) = child.try_wait()? {
                        break status;
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(io::Error::other("failing receiver did not exit"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                };
                let reason = std::fs::read_to_string(directory.join("receiver-failure.log"))?;
                std::fs::remove_dir_all(&directory)?;
                if status.success() {
                    return Err(io::Error::other("corrupt receiver unexpectedly started"));
                }
                send(
                    &mut connection,
                    json!({"type":"list_processes","request":5}),
                )?;
                response(&mut connection, "processes")?;
                eprintln!(
                    "AGENT_SOURCE_USABLE_AFTER_RECEIVER_FAILURE receiver_exit={status} reason={}",
                    reason.lines().last().unwrap_or("").trim()
                );
            }
            if let Ok(seconds) = std::env::var("AGENT_SOURCE_AGE_SECS") {
                let seconds: u64 = seconds.parse().map_err(io::Error::other)?;
                if !(1800..=3600).contains(&seconds) {
                    return Err(io::Error::other("source age must be1800..3600seconds"));
                }
                let age_started = Instant::now();
                while age_started.elapsed() < Duration::from_secs(seconds) {
                    let value = receive(&mut connection)?;
                    if value["type"] != "heartbeat" {
                        return Err(io::Error::other("unexpected warm-source message"));
                    }
                }
                eprintln!(
                    "AGENT_LONG_UPTIME_SOURCE elapsed_ms={}",
                    age_started.elapsed().as_millis()
                );
            }
            let expired = std::env::var_os("AGENT_EXPIRED_RECOVERY").is_some();
            let unarmed = std::env::var_os("AGENT_UNARMED_RECOVERY").is_some() || expired;
            if expired {
                arm_handle.arm_template_capture(Duration::from_millis(20))?;
                let error = match arm_handle.wait_template_capture() {
                    Err(error) => error,
                    Ok(_) => {
                        return Err(io::Error::other("capture succeeded without guest trigger"))
                    }
                };
                if error.kind() != io::ErrorKind::TimedOut {
                    return Err(error);
                }
                eprintln!("AGENT_CAPTURE_DEADLINE_EXPIRED typed_timeout=1");
            }
            if !unarmed {
                arm_handle.arm_template_capture(Duration::from_secs(12))?;
                eprintln!("AGENT_SOURCE_ARMED");
            }
            freeze_tx.send(Instant::now()).map_err(io::Error::other)?;
            send(&mut connection, json!({"type":"freeze","request":2}))?;
            eprintln!("AGENT_FREEZE_SENT");
            if unarmed {
                drop(connection);
                let mut recovered = accept(&source_listener)?;
                let recovered_hello = response(&mut recovered, "hello")?;
                if recovered_hello["boot_id"] != hello["boot_id"] {
                    return Err(io::Error::other("recovery changed source identity"));
                }
                send(
                    &mut recovered,
                    json!({"type":"welcome","version":3,"features":[]}),
                )?;
                send(&mut recovered, json!({"type":"list_processes","request":3}))?;
                response(&mut recovered, "processes")?;
                if std::env::var_os("AGENT_FREEZE_TWICE").is_some() {
                    send(&mut recovered, json!({"type":"freeze","request":6}))?;
                    drop(recovered);
                    recovered = accept(&source_listener)?;
                    let again = response(&mut recovered, "hello")?;
                    if again["boot_id"] != hello["boot_id"] {
                        return Err(io::Error::other("second freeze changed identity"));
                    }
                    send(
                        &mut recovered,
                        json!({"type":"welcome","version":3,"features":[]}),
                    )?;
                    send(&mut recovered, json!({"type":"list_processes","request":7}))?;
                    response(&mut recovered, "processes")?;
                    eprintln!("AGENT_TWO_FREEZES_RECOVERED running=1");
                }
                send(&mut recovered, json!({"type":"power_off","request":4}))?;
                response(&mut recovered, "ack")?;
                eprintln!("AGENT_UNARMED_SOURCE_RECOVERED running=1");
                return Ok(hello);
            }
            // Keep the source session alive until capture closes its backend.
            // A returned error/ack means the guest refused or recovered in place.
            loop {
                match receive(&mut connection) {
                    Ok(value) if value["type"] == "heartbeat" => continue,
                    Ok(value) => {
                        eprintln!("AGENT_FREEZE_RETURNED {value}");
                        return Err(io::Error::other(format!("freeze returned: {value}")));
                    }
                    Err(error)
                        if error.kind() == io::ErrorKind::UnexpectedEof
                            || error.kind() == io::ErrorKind::ConnectionReset =>
                    {
                        break
                    }
                    Err(error) => return Err(error),
                }
            }
            drop(workload_connection);
            Ok(hello)
        });
        let source_exit = context.run()?;
        if std::env::var_os("AGENT_UNARMED_RECOVERY").is_some()
            || std::env::var_os("AGENT_EXPIRED_RECOVERY").is_some()
        {
            source_thread
                .join()
                .map_err(|_| "source recovery thread panicked")??;
            // A refused capture must leave no template: the wait reports an error
            // and no sealed template descriptor exists in this process.
            let waited = handle.wait_template_capture();
            let template_fds = std::fs::read_dir("/proc/self/fd")?
                .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
                .filter(|target| target.to_string_lossy().contains("guest-template"))
                .count();
            if waited.is_ok() || template_fds != 0 {
                return Err("refused capture left a template behind".into());
            }
            println!(
                "AGENT_REFUSAL_NO_TEMPLATE wait_error=1 template_fds={template_fds} error={}",
                waited.err().map(|e| e.to_string()).unwrap_or_default()
            );
            println!("AGENT_UNARMED_RECOVERY_PASS exit={source_exit:?}");
            return Ok(());
        }
        if !matches!(source_exit, VmExit::Frozen { .. }) {
            return Err("source not frozen".into());
        }
        let hello = source_thread
            .join()
            .map_err(|_| "source I/O thread panicked")??;
        let template = handle.wait_template_capture()?;
        // What the host captured from boot discovery: compare across runs with
        // and without a console flood.
        println!(
            "AGENT_TEMPLATE_MANIFEST control=0x{:x} regions={:?} vcpus={} ram_mib={}",
            template.control_page_address,
            template
                .regions
                .iter()
                .map(|&(gpa, len, _)| (format!("0x{gpa:x}"), len))
                .collect::<Vec<_>>(),
            template.vcpus,
            template.ram_mib
        );
        if let Some((stop, sampler)) = rss_sampler.take() {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = sampler.join();
        }
        let frozen_disk = if full_devices {
            Some(std::fs::read(&source_disk)?)
        } else {
            None
        };
        let frozen_fs = source_fs.snapshot();
        eprintln!(
            "AGENT_TEMPLATE_CAPTURED freeze_request_to_template_us={}",
            freeze_rx
                .recv_timeout(Duration::from_secs(1))?
                .elapsed()
                .as_micros()
        );
        if parked > 0 {
            use std::os::fd::AsRawFd;
            let directory = &park_directory;
            for cid in 760 + early..760 + parked {
                parked_children.push(spawn_parked(&args[2], cid, directory)?);
            }
            wait_parked(directory, 760..760 + parked, &mut parked_children)?;
            if let Some(disk) = frozen_disk.as_ref() {
                std::fs::write(directory.join("frozen-disk"), disk)?;
                std::fs::write(directory.join("frozen-fs"), &frozen_fs)?;
            }
            let offer = format!(
                "{}\n{}\n{}",
                std::process::id(),
                template.file.as_raw_fd(),
                hello["boot_id"].as_str().ok_or("missing boot_id")?
            );
            std::fs::write(directory.join("template-offer.tmp"), offer)?;
            std::fs::rename(
                directory.join("template-offer.tmp"),
                directory.join("template-ready"),
            )?;
            eprintln!("AGENT_TEMPLATE_OFFERED parked={parked}");
            let ready = wait_until("parked copies ready", Duration::from_secs(40), || {
                (760..760 + parked).all(|cid| directory.join(format!("ready-{cid}")).exists())
            });
            if ready.is_err()
                || parked_children
                    .iter_mut()
                    .any(|child| child.try_wait().ok().flatten().is_some())
            {
                for child in &mut parked_children {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err("parked copies did not all become ready".into());
            }
            if std::env::var_os("AGENT_WORKLOAD").is_some() {
                let values = (760..760 + parked)
                    .map(|cid| std::fs::read_to_string(directory.join(format!("ready-{cid}"))))
                    .collect::<io::Result<std::collections::HashSet<_>>>()?;
                if values.len() as u64 != parked {
                    return Err("duplicate parked random values".into());
                }
            }
            println!("AGENT_PARKED_ALIVE count={parked}");
            std::fs::write(directory.join("release"), b"release")?;
            for child in &mut parked_children {
                if !child.wait()?.success() {
                    return Err("parked copy failed".into());
                }
            }
            if std::env::var_os("AGENT_MISMATCH").is_some() {
                // One mismatching parked receiver at a time, template still offered.
                let kernel_eif = std::env::var("AGENT_MISMATCH_KERNEL_EIF")
                    .map_err(|_| "AGENT_MISMATCH needs AGENT_MISMATCH_KERNEL_EIF")?;
                let base_mib = std::env::var("COPY_MEMORY_MIB").unwrap_or_else(|_| "1024".into());
                let base_cpus = std::env::var("COPY_CPUS").unwrap_or_else(|_| "1".into());
                if base_mib != "1024" || base_cpus != "1" || !full_devices {
                    return Err("AGENT_MISMATCH expects a 1 GiB, 1 CPU source with devices".into());
                }
                // (kind, CID, receiver EIF, environment overrides, without devices)
                type Case<'a> = (&'a str, u64, &'a str, &'a [(&'a str, &'a str)], bool);
                let cases: [Case; 4] = [
                    (
                        "cpu",
                        793,
                        "takeover-receiver-1024-4.eif",
                        &[("COPY_CPUS", "4")],
                        false,
                    ),
                    (
                        "memory",
                        794,
                        "takeover-receiver-4096-1.eif",
                        &[("COPY_MEMORY_MIB", "4096")],
                        false,
                    ),
                    ("devices", 795, &args[2], &[], true),
                    ("kernel", 796, &kernel_eif, &[], false),
                ];
                for (kind, cid, eif, envs, without_devices) in cases {
                    let mut command = std::process::Command::new(std::env::current_exe()?);
                    command
                        .args(["--parked-child", eif, &cid.to_string()])
                        .arg(directory)
                        .env("PARK_MODE", "mismatch")
                        .env("MISMATCH_KIND", kind)
                        .env("PARK_WAIT_SECS", "60");
                    for (key, value) in envs {
                        command.env(key, value);
                    }
                    if without_devices {
                        command.env_remove("AGENT_DEVICES");
                    }
                    let mut child = command.spawn()?;
                    let deadline = Instant::now() + Duration::from_secs(90);
                    let status = loop {
                        if let Some(status) = child.try_wait()? {
                            break status;
                        }
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(format!("mismatch case {kind} timed out").into());
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    };
                    if !status.success() {
                        return Err(format!("mismatch case {kind} failed: {status:?}").into());
                    }
                    println!("AGENT_MISMATCH_CASE_PASS kind={kind} cid={cid}");
                }
                println!("AGENT_MISMATCH_COMPLETED count=4");
            }
            drop(template);
            std::fs::remove_dir_all(directory)?;
            std::fs::remove_file(socket)?;
            println!("AGENT_PARKED_COMPLETED count={parked}");
            return Ok(());
        }
        if std::env::var_os("AGENT_CONCURRENT").is_some() {
            use std::os::{fd::AsRawFd, unix::process::CommandExt};
            let directory =
                std::env::current_dir()?.join(format!("agent-concurrent-{}", std::process::id()));
            std::fs::create_dir(&directory)?;
            if let Some(disk) = frozen_disk.as_ref() {
                std::fs::write(directory.join("frozen-disk"), disk)?;
                std::fs::write(directory.join("frozen-fs"), &frozen_fs)?;
            }
            let mut children = Vec::new();
            for cid in 740..744 {
                let mut command = std::process::Command::new(std::env::current_exe()?);
                command.args([
                    "--restore-child",
                    &args[2],
                    &cid.to_string(),
                    hello["boot_id"].as_str().ok_or("missing boot_id")?,
                    directory.to_str().ok_or("nonutf8 directory")?,
                ]);
                let fd = template.file.as_raw_fd();
                // SAFETY: pre_exec only duplicates a live sealed descriptor; no allocation or locks.
                unsafe {
                    command.pre_exec(move || {
                        if libc::dup2(fd, 100) < 0 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                children.push(command.spawn()?);
            }
            drop(template);
            eprintln!("AGENT_PARENT_TEMPLATE_DROPPED copies=4");
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if (740..744).all(|cid| directory.join(format!("ready-{cid}")).exists()) {
                    break;
                }
                if Instant::now() >= deadline {
                    for child in &mut children {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    return Err("four-copy ready deadline".into());
                }
                std::thread::yield_now();
            }
            if children
                .iter_mut()
                .any(|child| child.try_wait().ok().flatten().is_some())
            {
                return Err("copy exited before four-alive barrier".into());
            }
            if std::env::var_os("AGENT_WORKLOAD").is_some() {
                let values = (740..744)
                    .map(|cid| std::fs::read_to_string(directory.join(format!("ready-{cid}"))))
                    .collect::<io::Result<std::collections::HashSet<_>>>()?;
                if values.len() != 4 {
                    return Err("duplicate concurrent random values".into());
                }
            }
            println!("AGENT_FOUR_ALIVE count=4");
            std::fs::write(directory.join("release"), b"release")?;
            for child in &mut children {
                if !child.wait()?.success() {
                    return Err("concurrent copy failed".into());
                }
            }
            std::fs::remove_dir_all(directory)?;
            std::fs::remove_file(socket)?;
            println!("AGENT_FOUR_COMPLETED count=4");
            return Ok(());
        }
        let copies = std::env::var("AGENT_COPIES")
            .unwrap_or_else(|_| "1".into())
            .parse::<u64>()?;
        if !(1..=80).contains(&copies) {
            return Err("AGENT_COPIES must be1..80".into());
        }
        let mut random_values = std::collections::HashSet::new();
        for copy in 0..copies {
            let started = Instant::now();
            let copy_socket = socket.with_extension(format!("copy-{copy}.sock"));
            let copy_listener = UnixListener::bind(&copy_socket)?;
            copy_listener.set_nonblocking(true)?;
            let copy_workload = if workload_listener.is_some() {
                let l = UnixListener::bind(copy_socket.with_extension("workload.sock"))?;
                l.set_nonblocking(true)?;
                Some(l)
            } else {
                None
            };
            let copy_disk = copy_socket.with_extension("disk");
            let mut receiver = builder(&args[2], 712 + copy, &copy_socket)?;
            if let Some(bytes) = &frozen_disk {
                std::fs::write(&copy_disk, bytes)?;
                attach_devices(
                    &mut receiver,
                    &copy_disk,
                    super::nitro_copy_fs::Tree::new(frozen_fs.clone()),
                );
            }
            let _readiness = receiver.start_from_template(
                TemplateSource::Descriptor(template.file.try_clone()?),
                Duration::from_secs(15),
                |_| Ok(()),
            )?;
            let context = receiver.build()?;
            if std::env::var_os("AGENT_COPY_FREEZE").is_some() {
                let error = context
                    .vm_handle()
                    .arm_template_capture(Duration::from_secs(1))
                    .unwrap_err();
                if !error
                    .get_ref()
                    .is_some_and(|e| e.is::<krun::RestoredCopyCaptureUnsupported>())
                {
                    return Err(format!("wrong copy capture refusal: {error}").into());
                }
                eprintln!("AGENT_COPY_CAPTURE_REFUSED typed=RestoredCopyCaptureUnsupported");
            }
            let hello = hello.clone();
            let (random_tx, random_rx) = std::sync::mpsc::sync_channel(1);
            let receiver_thread = std::thread::spawn(move || -> io::Result<()> {
                let result = (|| -> io::Result<()> {
                    eprintln!("COPY_IO_WAIT copy={copy}");
                    let mut connection = accept(&copy_listener)?;
                    let copied = response(&mut connection, "hello")?;
                    eprintln!("COPY_IO_HELLO copy={copy}");
                    // No prestart: from choosing the template (backend files and
                    // builder, fresh receiver boot, import, release) to agent Hello.
                    println!(
                        "AGENT_COPY_CHOSEN_TO_HELLO copy={copy} us={}",
                        started.elapsed().as_micros()
                    );
                    if std::env::var_os("NITRO_COPY_TIMINGS").is_some() {
                        eprintln!(
                            "NITRO_AGENT_HELLO_TIMING host_ns={}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_err(io::Error::other)?
                                .as_nanos()
                        );
                    }
                    if copied["boot_id"] != hello["boot_id"]
                        || copied["generation"] != 0
                        || copied["token"] != "clone-test-token"
                    {
                        return Err(io::Error::other("resumed agent identity mismatch"));
                    }
                    send(
                        &mut connection,
                        json!({"type":"welcome","version":3,"features":[]}),
                    )?;
                    send(
                        &mut connection,
                        json!({"type":"copied","request":3,"generation":1}),
                    )?;
                    response(&mut connection, "ack")?;
                    eprintln!("COPY_IO_GENERATION_ACK copy={copy}");
                    if std::env::var_os("AGENT_COPY_FREEZE").is_some() {
                        send(&mut connection, json!({"type":"freeze","request":8}))?;
                        let refused = response(&mut connection, "error")?;
                        send(
                            &mut connection,
                            json!({"type":"list_processes","request":9}),
                        )?;
                        response(&mut connection, "processes")?;
                        eprintln!("AGENT_COPY_KERNEL_FREEZE_REFUSED running=1 response={refused}");
                    }
                    if let Some(listener) = copy_workload {
                        eprintln!("COPY_IO_WORKLOAD_WAIT copy={copy}");
                        random_tx
                            .send(verify_workload(&listener, 712 + copy)?)
                            .map_err(io::Error::other)?;
                    }
                    println!(
                        "AGENT_COPY_READY copy={} create_to_agent_ready_us={}",
                        copy,
                        started.elapsed().as_micros()
                    );
                    send(&mut connection, json!({"type":"power_off","request":4}))?;
                    response(&mut connection, "ack")?;
                    Ok(())
                })();
                if let Err(error) = &result {
                    eprintln!("COPY_IO_ERROR copy={copy} error={error}");
                }
                result
            });
            let _exit = context.run()?;
            receiver_thread
                .join()
                .map_err(|_| "receiver I/O thread panicked")??;
            if workload_listener.is_some()
                && !random_values.insert(random_rx.recv_timeout(Duration::from_secs(1))?)
            {
                return Err("duplicate copied random value".into());
            }
            std::fs::remove_file(&copy_socket)?;
            if workload_listener.is_some() {
                std::fs::remove_file(copy_socket.with_extension("workload.sock"))?;
            }
            println!("AGENT_COPY_RECONNECTED copy={} cid={} total_us={} quiescent=1 armed_before_trigger=1 generation=1",copy,712+copy,started.elapsed().as_micros());
        }
        if workload_listener.is_some() {
            std::fs::remove_file(socket.with_extension("workload.sock"))?;
        }
        std::fs::remove_file(socket)?;
        Ok(())
    }
}
#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    probe::run()
}
#[cfg(not(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
)))]
fn main() {
    eprintln!("requires arm64 Linux Nitro feature");
}
