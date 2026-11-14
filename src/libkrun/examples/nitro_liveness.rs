//! Real-guest liveness/panic and bounded-console acceptance.
use krun::{Builder, NitroBackendConfig, VmExit};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        4,
        "usage: nitro_liveness IMAGE SOCKET panic|heartbeat|never|flood"
    );
    let mut builder = Builder::new();
    builder.nitro_backend(NitroBackendConfig {
        image: args[1].clone().into(),
        cid: 212,
        watchdog: std::time::Duration::from_secs(5),
    });
    builder.disable_implicit_console()?;
    builder.vm_config(1, 1024)?;
    let captured = Arc::new(Mutex::new(Vec::new()));
    builder.nitro_console_sink(Box::new(Sink(captured.clone())), 4096, 2048);
    if args[3] != "panic" {
        builder.nitro_heartbeat(
            7999,
            args[2].clone().into(),
            std::time::Duration::from_secs(1),
        );
    }
    let context = builder.build()?;
    let start = std::time::Instant::now();
    let exit = context.run()?;
    let elapsed = start.elapsed().as_secs_f64();
    if args[3] == "panic" {
        assert_eq!(exit, VmExit::GuestPanic);
    } else {
        assert_eq!(exit, VmExit::LivenessExpired);
    }
    if args[3] == "heartbeat" {
        assert!(
            elapsed >= 2.5,
            "heartbeats did not extend deadline: {elapsed}"
        );
    }
    assert!(elapsed < 6.0, "unbounded exit: {elapsed}");
    assert!(captured.lock().unwrap().len() <= 4096);
    println!(
        "LIVENESS_OK mode={} exit={exit:?} seconds={elapsed:.3} console_bytes={}",
        args[3],
        captured.lock().unwrap().len()
    );
    Ok(())
}
