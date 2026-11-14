use nitro_sandbox::host::Sandbox;
use std::time::Duration;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let image = std::env::args().nth(1).ok_or("usage: reuse EIF")?;
    let mut memory = None;
    let mut address = None;
    for i in 0..3 {
        let mut sandbox = Sandbox::with_mapping(1024 * 1024 * 1024, 2, memory.take())?;
        let current = sandbox.ram.address();
        if let Some(expected) = address { assert_eq!(expected, current); }
        address = Some(current);
        sandbox.load(&image)?;
        sandbox.start(210)?;
        if !sandbox.wait(Duration::from_secs(15))? { return Err("watchdog expired".into()); }
        memory = Some(sandbox.into_mapping());
        eprintln!("REUSE_OK lifecycle={i} address={current:#x}");
    }
    Ok(())
}
