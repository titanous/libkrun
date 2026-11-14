#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use krun::{Builder, NitroBackendConfig, NitroSerialization, VmExit};
    use std::time::Duration;
    let args: Vec<_> = std::env::args().collect();
    if !(2..=3).contains(&args.len()) {
        return Err("usage: nitro-public-capture source.eif [receiver.eif]".into());
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
    if args.len() == 3 {
        let mut receiver = Builder::new();
        receiver.nitro_backend(NitroBackendConfig {
            image: args[2].clone().into(),
            cid: 710,
            watchdog: Duration::from_secs(15),
        });
        receiver.nitro_lifecycle_lock(
            "/run/holodeck/nitro-lifecycle.lock".into(),
            NitroSerialization::StartAndTeardown,
        );
        receiver.vm_config(1, 1024)?;
        receiver.disable_implicit_console()?;
        receiver.nitro_console_sink(
            Box::new(std::fs::File::create("public-restore-console.log")?),
            1024 * 1024,
            1024 * 1024,
        );
        let _readiness = receiver.start_from_template(
            krun::TemplateSource::Descriptor(template.file.try_clone()?),
            Duration::from_secs(15),
            |_| Ok(()),
        )?;
        let _exit = receiver.build()?.run()?;
        if !std::fs::read_to_string("public-restore-console.log")?
            .contains("COPY_VERIFY ok=1 bad=0")
        {
            return Err("public restore verification missing".into());
        }
        println!("PUBLIC_TEMPLATE_RESTORE_VERIFIED");
    }
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
