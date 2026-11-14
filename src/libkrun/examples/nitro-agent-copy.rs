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
            if value["type"] != "heartbeat" {
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
    fn builder(image: &str, cid: u64, socket: &Path) -> io::Result<Builder> {
        let mut builder = Builder::new();
        builder.nitro_backend(NitroBackendConfig {
            image: image.into(),
            cid,
            watchdog: Duration::from_secs(20),
        });
        builder.nitro_lifecycle_lock(
            "/run/holodeck/nitro-lifecycle.lock".into(),
            NitroSerialization::StartAndTeardown,
        );
        builder.vm_config(1, 1024).map_err(io::Error::other)?;
        builder
            .disable_implicit_console()
            .map_err(io::Error::other)?;
        builder.nitro_console_sink(
            Box::new(std::fs::File::create(format!("agent-console-{cid}.log"))?),
            1024 * 1024,
            1024 * 1024,
        );
        builder.add_vsock_port(7000, socket.into(), false);
        Ok(builder)
    }
    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args: Vec<_> = std::env::args().collect();
        if args.len() != 3 {
            return Err("usage: nitro-agent-copy agent-source.eif receiver.eif".into());
        }
        let socket =
            std::env::current_dir()?.join(format!("agent-copy-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let mut source = builder(&args[1], 711, &socket)?;
        source.future_template();
        eprintln!("AGENT_SOURCE_BUILD");
        let context = source.build()?;
        eprintln!("AGENT_SOURCE_RUN");
        let handle = context.vm_handle();
        let arm_handle = context.vm_handle();
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
            send(
                &mut connection,
                json!({"type":"list_processes","request":1}),
            )?;
            let processes = response(&mut connection, "processes")?;
            if processes["processes"]
                .as_array()
                .is_none_or(|p| !p.is_empty())
            {
                return Err(io::Error::other("source workload not quiescent"));
            }
            eprintln!("AGENT_SOURCE_QUIESCENT");
            arm_handle.arm_template_capture(Duration::from_secs(12))?;
            eprintln!("AGENT_SOURCE_ARMED");
            send(&mut connection, json!({"type":"freeze","request":2}))?;
            eprintln!("AGENT_FREEZE_SENT");
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
            Ok(hello)
        });
        if !matches!(context.run()?, VmExit::Frozen { .. }) {
            return Err("source not frozen".into());
        }
        let hello = source_thread
            .join()
            .map_err(|_| "source I/O thread panicked")??;
        let template = handle.wait_template_capture()?;
        eprintln!("AGENT_TEMPLATE_CAPTURED");
        let copies = std::env::var("AGENT_COPIES")
            .unwrap_or_else(|_| "1".into())
            .parse::<u64>()?;
        if !(1..=80).contains(&copies) {
            return Err("AGENT_COPIES must be1..80".into());
        }
        for copy in 0..copies {
            let started = Instant::now();
            let mut receiver = builder(&args[2], 712 + copy, &socket)?;
            let _readiness = receiver.start_from_template(
                TemplateSource::Descriptor(template.file.try_clone()?),
                Duration::from_secs(15),
                |_| Ok(()),
            )?;
            let context = receiver.build()?;
            let copy_listener = listener.try_clone()?;
            let hello = hello.clone();
            let receiver_thread = std::thread::spawn(move || -> io::Result<()> {
                let mut connection = accept(&copy_listener)?;
                let copied = response(&mut connection, "hello")?;
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
                send(&mut connection, json!({"type":"power_off","request":4}))?;
                response(&mut connection, "ack")?;
                Ok(())
            });
            let _exit = context.run()?;
            receiver_thread
                .join()
                .map_err(|_| "receiver I/O thread panicked")??;
            println!("AGENT_COPY_RECONNECTED copy={} cid={} total_us={} quiescent=1 armed_before_trigger=1 generation=1",copy,712+copy,started.elapsed().as_micros());
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
