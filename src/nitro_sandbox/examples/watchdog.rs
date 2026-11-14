#[cfg(feature = "devices")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nitro_sandbox::runtime::{Exit, Runtime};
    let image = std::env::args().nth(1).ok_or("usage: watchdog SILENT_EIF")?;
    let start = std::time::Instant::now();
    let runtime = Runtime::new(&image, 1024 * 1024 * 1024, 2, 211, std::time::Duration::from_secs(3))?;
    assert_eq!(runtime.run()?, Exit::Watchdog);
    eprintln!("WATCHDOG_OK seconds={:.3}", start.elapsed().as_secs_f64());
    Ok(())
}
#[cfg(not(feature = "devices"))]
fn main() { panic!("requires --features devices"); }
