#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nitro_sandbox::{
        host::Sandbox,
        lifecycle::{LifecycleLock, Serialization},
    };
    use std::time::{Duration, Instant};
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: receiver-ready receiver-Image output.eif shared-lifecycle-lock".into());
    }
    let image = nitro_sandbox::image::package(&std::fs::read(&args[1])?, None, b"", 1024, 1)?;
    std::fs::write(&args[2], image)?;
    let mut sandbox = Sandbox::with_lifecycle_lock(
        1024 * 1024 * 1024,
        1,
        None,
        Some(LifecycleLock {
            path: args[3].clone().into(),
            serialization: Serialization::StartAndTeardown,
        }),
    )?;
    sandbox.load(&args[2])?;
    let start = Instant::now();
    sandbox.start(700)?;
    let ready = (sandbox.ram.address() + 0x30002000) as *const u64;
    while start.elapsed() < Duration::from_secs(5) {
        // SAFETY: this aligned control word lies in the sandbox's live RAM mapping.
        // The parked receiver publishes it; no host writes occur during this probe.
        if unsafe { ready.read_volatile() } == 0x5944414552564352 {
            std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
            println!("RECEIVER_READY start_us={}", start.elapsed().as_micros());
            return Ok(());
        }
        std::thread::yield_now();
    }
    Err("receiver READY deadline expired".into())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("receiver-ready requires Linux");
    std::process::exit(2);
}
