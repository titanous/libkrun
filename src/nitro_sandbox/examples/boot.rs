use nitro_sandbox::host::Sandbox;
use std::time::Duration;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let image = std::env::args().nth(1).ok_or("usage: boot IMAGE [CID]")?;
    let cid = std::env::args().nth(2).unwrap_or_else(|| "200".into()).parse()?;
    let mut sandbox = Sandbox::new(1024 * 1024 * 1024, 2)?;
    sandbox.load(&image)?;
    sandbox.start(cid)?;
    eprintln!("STARTED cid={cid}");
    if !sandbox.wait(Duration::from_secs(30))? { return Err("guest watchdog expired".into()); }
    eprintln!("POWER_OFF fd_hup=true");
    Ok(())
}
