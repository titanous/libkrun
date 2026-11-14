#[cfg(feature = "devices")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nitro_sandbox::host::Sandbox;
    use vm_memory::{GuestAddress, GuestMemory, Permissions};
    let image = std::env::args().nth(1).ok_or("usage: split_ram EIF")?;
    let mut sandbox = Sandbox::new(2 * 1024 * 1024 * 1024, 2)?;
    let memory = unsafe { sandbox.guest_memory()? };
    assert!(memory.check_range(GuestAddress(0x40000000), 4096, Permissions::ReadWrite));
    assert!(!memory.check_range(GuestAddress(0x80000000), 4096, Permissions::ReadWrite));
    assert!(memory.check_range(GuestAddress(0x400000000), 4096, Permissions::ReadWrite));
    sandbox.load(&image)?;
    sandbox.start(214)?;
    assert!(sandbox.wait(std::time::Duration::from_secs(15))?);
    drop(memory);
    eprintln!("SPLIT_RAM_OK");
    Ok(())
}
#[cfg(not(feature = "devices"))]
fn main() {
    panic!("requires --features devices");
}
