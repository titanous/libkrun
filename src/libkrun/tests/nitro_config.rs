#![cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
use krun::{Builder, NitroBackendConfig, StartError};
#[test]
fn rejects_eif_incompatible_exec_before_allocating_vm() {
    let mut builder = Builder::new();
    builder.exec_path("/should-not-be-ignored".into());
    builder.nitro_backend(NitroBackendConfig {
        image: "/does-not-exist.eif".into(),
        cid: 200,
        watchdog: std::time::Duration::from_secs(3),
    });
    match builder.build() {
        Err(StartError::Nitro(error)) => assert_eq!(error.kind(), std::io::ErrorKind::Unsupported),
        _ => panic!("must reject configuration before opening Nitro"),
    }
}
