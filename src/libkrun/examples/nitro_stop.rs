//! One-guest process used by the stop/kill lifecycle acceptance harness.
use krun::{Builder, NitroBackendConfig, NitroSerialization};
use std::io::Write;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        5,
        "usage: nitro_stop IMAGE LOCK handle|signal|poweroff CID"
    );
    let mut builder = Builder::new();
    builder.nitro_console_sink(Box::new(std::io::stdout()), 1024 * 1024, 256 * 1024);
    builder.nitro_backend(NitroBackendConfig {
        image: args[1].clone().into(),
        cid: args[4].parse()?,
        watchdog: std::time::Duration::from_secs(10),
    });
    builder.disable_implicit_console()?;
    builder.vm_config(1, 1024)?;
    builder.nitro_lifecycle_lock(args[2].clone().into(), NitroSerialization::StartAndTeardown);
    let context = builder.build()?;
    let handle = context.vm_handle();
    let _signal = handle.install_sigterm_handler()?;
    println!("STOP_TEST_READY pid={}", std::process::id());
    std::io::stdout().flush()?;
    let worker = if args[3] == "handle" {
        Some(std::thread::spawn(move || {
            std::thread::park_timeout(std::time::Duration::from_millis(500));
            handle.trigger_shutdown_event().unwrap();
        }))
    } else {
        None
    };
    let exit = context.run()?;
    if let Some(worker) = worker {
        worker.join().unwrap();
    }
    if args[3] != "poweroff" {
        assert_eq!(exit, krun::VmExit::Stopped);
    }
    println!("STOP_TEST_OK {exit:?}");
    Ok(())
}
