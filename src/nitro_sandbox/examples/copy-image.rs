#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: copy-image Image initramfs output.eif".into());
    }
    let cpus = std::env::var("COPY_CPUS")
        .unwrap_or_else(|_| "1".into())
        .parse()?;
    let memory = std::env::var("COPY_MEMORY_MIB")
        .unwrap_or_else(|_| "1024".into())
        .parse()?;
    let initramfs = if args[2] == "-" {
        None
    } else {
        Some(std::fs::read(&args[2])?)
    };
    let image = nitro_sandbox::image::package(
        &std::fs::read(&args[1])?,
        initramfs.as_deref(),
        b"console=ttyS0 nokaslr panic=-1 rdinit=/init",
        memory,
        cpus,
    )?;
    std::fs::write(&args[3], image)?;
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("Linux required");
}
