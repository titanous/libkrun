#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use krun::{Builder, NitroBackendConfig, NitroSerialization, VmExit};
    use std::time::Duration;
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 2 {
        return Err("usage: nitro-public-capture source.eif".into());
    }
    let mut builder = Builder::new();
    builder.nitro_backend(NitroBackendConfig {
        image: args[1].clone().into(),
        cid: 709,
        watchdog: Duration::from_secs(15),
    });
    builder.nitro_lifecycle_lock(
        "/run/holodeck/nitro-lifecycle.lock".into(),
        NitroSerialization::StartAndTeardown,
    );
    builder.vm_config(1, 1024)?;
    builder.disable_implicit_console()?;
    builder.future_template();
    let context = builder.build()?;
    let handle = context.vm_handle();
    handle.arm_template_capture(Duration::from_secs(15))?;
    if !matches!(context.run()?, VmExit::Frozen { .. }) {
        return Err("expected frozen exit".into());
    }
    let template = handle.wait_template_capture()?;
    println!(
        "PUBLIC_CAPTURE_SEALED regions={} ram_mib={}",
        template.regions.len(),
        template.ram_mib
    );
    Ok(())
}
#[cfg(not(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
)))]
fn main() {
    eprintln!("requires arm64 Linux Nitro feature");
}
