#![cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
use krun::{Builder, NitroBackendConfig, StartError};
fn rejected(mut builder: Builder, expected: &str) {
    builder.nitro_backend(NitroBackendConfig {
        image: "/does-not-exist.eif".into(), cid: 200,
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
fn rejects_eif_incompatible_exec_before_allocating_vm() {
    let mut builder = Builder::new();
    builder.exec_path("/should-not-be-ignored".into());
    rejected(builder, "EIF");
}
#[test]
fn rejects_even_empty_commandline_override() {
    let mut builder = Builder::new();
    builder.set_kernel_cmdline(vec![]);
    rejected(builder, "command line");
}
#[test]
fn rejects_separate_external_kernel_before_allocating_vm() {
    let mut builder = Builder::new();
    let format = if cfg!(target_arch = "x86_64") { krun::KernelFormat::Elf } else { krun::KernelFormat::Raw };
    builder.external_kernel("/does-not-exist.kernel".into(), format, None).unwrap();
    rejected(builder, "separate external_kernel");
}
