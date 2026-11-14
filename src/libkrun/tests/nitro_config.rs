#![cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
use krun::{Builder, NitroBackendConfig, StartError};
fn rejected(mut builder: Builder, expected: &str) {
    builder.nitro_backend(NitroBackendConfig {
        image: "/does-not-exist.eif".into(),
        cid: 200,
        watchdog: std::time::Duration::from_secs(3),
    });
    match builder.build() {
        Err(StartError::Nitro(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("unsupported on this backend"));
            assert!(error.to_string().contains(expected));
        }
        _ => panic!("must reject configuration before opening Nitro"),
    }
}
#[test]
fn rejects_unsupported_memory_geometry_before_allocating_vm() {
    let mut builder = Builder::new();
    builder.disable_implicit_console().unwrap();
    builder.vm_config(1, 65).unwrap();
    rejected(builder, "memory must");
}
#[test]
#[cfg(feature = "net")]
fn rejects_ignored_mac_before_allocating_vm() {
    let mut builder = Builder::new();
    builder.net_mac([2, 0, 0, 0, 0, 1]);
    rejected(builder, "EIF");
}
#[test]
fn rejects_eif_incompatible_exec_before_allocating_vm() {
    let mut builder = Builder::new();
    builder.exec_path("/should-not-be-ignored".into());
    rejected(builder, "EIF");
}
#[test]
fn nitro_console_paths_follow_registered_device_order() {
    let mut builder = Builder::new();
    builder.nitro_backend(NitroBackendConfig {
        image: "/unused".into(),
        cid: 200,
        watchdog: std::time::Duration::from_secs(3),
    });
    builder.disable_implicit_console().unwrap();
    let paths =
        builder.add_virtio_console_with_ports(vec![devices::virtio::PortDescription::output_pipe(
            "test",
            devices::virtio::port_io::output_to_log_as_err(),
        )]);
    assert_eq!(paths, ["/dev/vport1p0"]); // implicit vsock precedes console
}
#[test]
fn rejects_backend_selection_after_console_paths_returned() {
    let mut builder = Builder::new();
    builder.disable_implicit_console().unwrap();
    builder.add_virtio_console();
    rejected(builder, "before adding consoles");
}
#[test]
fn rejects_even_empty_commandline_override() {
    let mut builder = Builder::new();
    builder.set_kernel_cmdline(vec![]);
    rejected(builder, "command line");
}
#[test]
#[cfg(target_arch = "x86_64")]
fn rejects_non_arm64_external_format_before_allocating_vm() {
    let mut builder = Builder::new();
    builder.disable_implicit_console().unwrap();
    builder
        .external_kernel(
            "/does-not-exist.kernel".into(),
            krun::KernelFormat::Elf,
            None,
        )
        .unwrap();
    rejected(builder, "arm64 Raw Image");
}
