//! Real KVM stop-reason protocol tests. Run serialized with an outer timeout.
#![cfg(target_arch = "x86_64")]

fn stopped(mode: &str, cpus: u8) -> krun::VmExit {
    stopped_with_kernel(mode, cpus, "EMBED_KERNEL")
}

fn stopped_with_kernel(mode: &str, cpus: u8, kernel_variable: &str) -> krun::VmExit {
    let console = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../target/embed/stop-{}-{mode}.log",
        std::process::id()
    ));
    let mut builder = krun::Builder::new();
    builder.vm_config(cpus, 512).unwrap();
    builder
        .external_kernel(
            std::env::var_os(kernel_variable)
                .expect("EMBED_KERNEL")
                .into(),
            krun::KernelFormat::Elf,
            Some(
                std::env::var_os("EMBED_INITRAMFS")
                    .expect("EMBED_INITRAMFS")
                    .into(),
            ),
        )
        .unwrap();
    builder.console_output(console.clone());
    builder.set_kernel_cmdline(vec![
        "console=hvc0",
        "nokaslr",
        "panic=-1",
        "reboot=k",
        "rdinit=/init",
        mode,
    ]);
    let result = builder
        .build()
        .expect("build stop guest")
        .run()
        .expect("run stop guest");
    let log = std::fs::read_to_string(&console).unwrap();
    if mode.contains("console_burst") {
        let normalized = log.replace('\r', "");
        let mut position = 0;
        for index in 0..4096 {
            let line =
                format!("CONSOLE_BURST_{index:04}_abcdefghijklmnopqrstuvwxyz_0123456789_END\n");
            position += normalized[position..]
                .find(&line)
                .unwrap_or_else(|| panic!("missing console line {index} for {mode}"))
                + line.len();
        }
    }
    let marker = match mode.split_whitespace().next().unwrap() {
        "embed_stop_poweroff" => "HOLODECK GUEST STOP poweroff",
        "embed_stop_reboot" => "HOLODECK GUEST STOP reboot",
        "embed_stop_halt" => "HOLODECK GUEST STOP halt",
        "embed_stop_panic" => "HOLODECK GUEST STOP panic",
        _ => unreachable!(),
    };
    assert!(log.contains(marker), "{log}");
    std::fs::remove_file(console).unwrap();
    result
}

#[test]
#[ignore = "requires KVM, LEGACY_KERNEL and external initramfs"]
fn legacy_kernel_stops() {
    assert_eq!(
        stopped_with_kernel("embed_stop_poweroff", 1, "LEGACY_KERNEL"),
        krun::VmExit::Fault
    );
    for mode in ["embed_stop_reboot", "embed_stop_halt", "embed_stop_panic"] {
        assert_eq!(
            stopped_with_kernel(mode, 1, "LEGACY_KERNEL"),
            krun::VmExit::Shutdown { exit_code: 0 }
        );
    }
}

#[test]
#[ignore = "requires KVM and external stop kernel/initramfs"]
fn poweroff() {
    assert_eq!(
        stopped("embed_stop_poweroff", 1),
        krun::VmExit::PowerOffRequested
    );
}
#[test]
#[ignore = "requires KVM and external stop kernel/initramfs"]
fn reboot() {
    assert_eq!(
        stopped("embed_stop_reboot", 1),
        krun::VmExit::RebootRequested
    );
}
#[test]
#[ignore = "requires KVM and external stop kernel/initramfs"]
fn halt() {
    assert_eq!(stopped("embed_stop_halt", 1), krun::VmExit::Halted);
}
#[test]
#[ignore = "requires KVM and external stop kernel/initramfs"]
fn halt_smp() {
    assert_eq!(stopped("embed_stop_halt", 4), krun::VmExit::Halted);
}
#[test]
#[ignore = "requires KVM and external stop kernel/initramfs"]
fn panic() {
    assert_eq!(stopped("embed_stop_panic", 1), krun::VmExit::Panic);
}
#[test]
#[ignore = "requires KVM and external stop kernel/initramfs"]
fn reason_precedes_different_terminal_mechanism() {
    assert_eq!(raw_stop("mismatch"), krun::VmExit::PowerOffRequested);
}

#[test]
#[ignore = "requires KVM and external stop kernel; 200 guests"]
fn console_output_before_stop_50_times() {
    for _ in 0..50 {
        for (mode, expected) in [
            (
                "embed_stop_poweroff console_burst",
                krun::VmExit::PowerOffRequested,
            ),
            (
                "embed_stop_reboot console_burst",
                krun::VmExit::RebootRequested,
            ),
            ("embed_stop_halt console_burst", krun::VmExit::Halted),
            ("embed_stop_panic console_burst", krun::VmExit::Panic),
        ] {
            assert_eq!(stopped(mode, 1), expected);
        }
    }
}

fn raw_stop(case: &str) -> krun::VmExit {
    let root = std::path::PathBuf::from(std::env::var_os("STOP_FIXTURES").expect("STOP_FIXTURES"));
    let mut builder = krun::Builder::new();
    builder.vm_config(1, 128).unwrap();
    builder
        .external_kernel(root.join(case), krun::KernelFormat::Elf, None)
        .unwrap();
    builder.build().unwrap().run().unwrap()
}

#[test]
#[ignore = "requires KVM and ring-0 stop fixtures"]
fn triple_fault_without_reason() {
    assert_eq!(raw_stop("fault"), krun::VmExit::Fault);
}
#[test]
#[ignore = "requires KVM and ring-0 stop fixtures"]
fn halt_without_reason() {
    assert_eq!(raw_stop("halt"), krun::VmExit::Fault);
}
