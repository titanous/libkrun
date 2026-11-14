//! Real KVM stop protocol acceptance. Run serialized under an outer timeout.
#![cfg(target_arch = "x86_64")]

fn stopped(mode: &str, cpus: u8, kernel: &str) -> krun::VmExit {
    let console = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../target/embed/stop-{}.log",
        std::process::id()
    ));
    let mut builder = krun::Builder::new();
    builder.vm_config(cpus, 512).unwrap();
    builder
        .external_kernel(
            std::env::var_os(kernel).expect("kernel path").into(),
            krun::KernelFormat::Elf,
            Some(
                std::env::var_os("EMBED_INITRAMFS")
                    .expect("initramfs path")
                    .into(),
            ),
        )
        .unwrap();
    builder.console_output(console.clone());
    builder.set_kernel_cmdline(vec![
        "console=hvc0",
        "panic=-1",
        "reboot=k",
        "rdinit=/init",
        mode,
    ]);
    let result = builder.build().unwrap().run().unwrap();
    let log = std::fs::read_to_string(&console).unwrap().replace('\r', "");
    assert!(
        log.contains("EMBED_USERSPACE_OK"),
        "guest did not reach fixture"
    );
    if mode.contains("console_burst") {
        let mut position = 0;
        for index in 0..4096 {
            let line =
                format!("CONSOLE_BURST_{index:04}_abcdefghijklmnopqrstuvwxyz_0123456789_END\n");
            position += log[position..]
                .find(&line)
                .unwrap_or_else(|| panic!("missing console line {index}: {mode}"))
                + line.len();
        }
    }
    std::fs::remove_file(console).unwrap();
    result
}

#[test]
#[ignore = "requires KVM and compiled STOP_FIXTURES"]
fn raw_terminal_mechanisms() {
    let root = std::path::PathBuf::from(std::env::var_os("STOP_FIXTURES").unwrap());
    for (fixture, expected) in [
        ("fault", krun::VmExit::Fault),
        ("reset", krun::VmExit::Fault),
        ("declared-reset", krun::VmExit::PowerOffRequested),
        ("cleared", krun::VmExit::Fault),
        ("invalid", krun::VmExit::Fault),
        ("sticky-panic", krun::VmExit::Panic),
    ] {
        let mut builder = krun::Builder::new();
        builder.vm_config(1, 128).unwrap();
        builder
            .external_kernel(root.join(fixture), krun::KernelFormat::Elf, None)
            .unwrap();
        assert_eq!(
            builder.build().unwrap().run().unwrap(),
            expected,
            "{fixture}"
        );
    }
}

#[test]
#[ignore = "requires KVM, EMBED_KERNEL and FLOOD_INITRAMFS; outer timeout required"]
fn eight_cpu_console_flood_with_unread_pipe_returns() {
    use std::os::fd::AsRawFd;
    let (reader, writer) = std::io::pipe().unwrap();
    let mut builder = krun::Builder::new();
    builder.vm_config(8, 512).unwrap();
    builder
        .external_kernel(
            std::env::var_os("EMBED_KERNEL").unwrap().into(),
            krun::KernelFormat::Elf,
            Some(std::env::var_os("FLOOD_INITRAMFS").unwrap().into()),
        )
        .unwrap();
    builder.console_output(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/embed/flood-primary.log"),
    );
    let console = builder.add_virtio_console();
    builder
        .add_port_console_fd(&console, -1, writer.as_raw_fd(), 80, 24)
        .unwrap();
    builder.set_kernel_cmdline(vec!["console=hvc0", "panic=-1", "reboot=k", "rdinit=/init"]);
    let started = std::time::Instant::now();
    assert_eq!(
        builder.build().unwrap().run().unwrap(),
        krun::VmExit::PowerOffRequested
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    drop((reader, writer));
}

#[test]
#[ignore = "requires KVM, EMBED_KERNEL and FLOOD_INITRAMFS; outer timeout required"]
fn console_pressure_stop_and_counter_are_bounded() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};
    let full_path = std::env::var_os("CONSOLE_FULL_FILE").map(std::path::PathBuf::from);
    for mode in 0..if full_path.is_some() { 4 } else { 3 } {
        let slow = mode == 1;
        let failing_file = (mode >= 2).then(|| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(if mode == 2 {
                    std::path::Path::new("/dev/full")
                } else {
                    full_path.as_ref().unwrap().as_path()
                })
                .unwrap()
        });
        let (mut reader, writer) = std::io::pipe().unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let reader_done = done.clone();
        let reader_thread = std::thread::spawn(move || {
            if !slow {
                while !reader_done.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            } else {
                let mut byte = [0];
                while !reader_done.load(Ordering::Acquire) {
                    if reader.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        });
        let mut builder = krun::Builder::new();
        builder.vm_config(8, 512).unwrap();
        builder
            .external_kernel(
                std::env::var_os("EMBED_KERNEL").unwrap().into(),
                krun::KernelFormat::Elf,
                Some(std::env::var_os("FLOOD_INITRAMFS").unwrap().into()),
            )
            .unwrap();
        let primary = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../target/embed/pressure-{mode}.log"));
        builder.disable_implicit_console().unwrap();
        let timer_output = std::fs::File::create(&primary).unwrap();
        let console = builder.add_virtio_console();
        builder
            .add_port_console_fd(&console, -1, timer_output.as_raw_fd(), 80, 24)
            .unwrap();
        let output_fd = failing_file
            .as_ref()
            .map_or(writer.as_raw_fd(), |file| file.as_raw_fd());
        builder
            .add_port_console_fd(&console, -1, output_fd, 80, 24)
            .unwrap();
        builder.set_kernel_cmdline(vec![
            "console=hvc0",
            "panic=-1",
            "reboot=k",
            "rdinit=/init",
            "console_pressure",
        ]);
        let context = builder.build().unwrap();
        let handle = context.vm_handle();
        let (sent, received) = std::sync::mpsc::channel();
        let runner = std::thread::spawn(move || {
            sent.send(context.run()).unwrap();
        });
        std::thread::sleep(Duration::from_secs(3));
        let guest_ticks: u64 = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|task| {
                let stat = std::fs::read_to_string(task.path().join("stat")).ok()?;
                let fields: Vec<_> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
                // Linux /proc task stat field 43: guest CPU ticks.
                fields.get(40)?.parse::<u64>().ok()
            })
            .sum();
        let began = Instant::now();
        let mut maximum_read = Duration::ZERO;
        let outcome = loop {
            let read_began = Instant::now();
            let _ = handle.console_dropped_bytes();
            maximum_read = maximum_read.max(read_began.elapsed());
            if let Ok(outcome) = received.try_recv() {
                break outcome;
            }
            assert!(
                began.elapsed() < Duration::from_secs(2),
                "stop stuck with slow={slow}"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        runner.join().unwrap();
        assert_eq!(outcome.as_ref().unwrap(), &krun::VmExit::PowerOffRequested);
        done.store(true, Ordering::Release);
        drop(writer);
        reader_thread.join().unwrap();
        let ticks = std::fs::read(primary)
            .unwrap()
            .windows(4)
            .filter(|bytes| *bytes == b"TICK")
            .count();
        eprintln!("pressure mode={mode} slow={slow} stop={:?} max_counter_read={maximum_read:?} ticks={ticks} guest_cpu_ticks={guest_ticks} dropped={} outcome={outcome:?}", began.elapsed(), handle.console_dropped_bytes());
        assert!(maximum_read < Duration::from_millis(50));
        assert!(ticks >= 15, "guest timer stalled: {ticks}");
    }
}

fn cases() -> [(&'static str, krun::VmExit); 4] {
    [
        ("embed_stop_poweroff", krun::VmExit::PowerOffRequested),
        ("embed_stop_reboot", krun::VmExit::RebootRequested),
        ("embed_stop_halt", krun::VmExit::Halted),
        ("embed_stop_panic", krun::VmExit::Panic),
    ]
}

#[test]
#[ignore = "requires KVM and external maintained kernel/initramfs"]
fn declared_stops() {
    for (mode, expected) in cases() {
        assert_eq!(stopped(mode, 1, "EMBED_KERNEL"), expected);
    }
    assert_eq!(
        stopped("embed_stop_halt", 4, "EMBED_KERNEL"),
        krun::VmExit::Halted
    );
}

#[test]
#[ignore = "requires KVM and published legacy kernel/initramfs"]
fn legacy_stops_are_faults() {
    for (mode, _) in cases() {
        assert_eq!(stopped(mode, 1, "LEGACY_KERNEL"), krun::VmExit::Fault);
    }
}

#[test]
#[ignore = "requires KVM; 200 guests and 819200 numbered console lines"]
fn console_output_before_stop_50_times() {
    for _ in 0..50 {
        for (mode, expected) in cases() {
            assert_eq!(
                stopped(&format!("{mode} console_burst"), 1, "EMBED_KERNEL"),
                expected
            );
        }
    }
}
