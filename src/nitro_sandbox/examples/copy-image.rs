#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: copy-image Image initramfs output.eif".into());
    }
    let image = nitro_sandbox::image::package(
        &std::fs::read(&args[1])?,
        Some(&std::fs::read(&args[2])?),
        b"console=ttyS0 nokaslr panic=-1 rdinit=/init",
        1024,
        1,
    )?;
    std::fs::write(&args[3], image)?;
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("Linux required");
}
