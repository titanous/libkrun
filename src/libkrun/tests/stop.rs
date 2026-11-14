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
