use nitro_sandbox::host::Sandbox;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = Sandbox::new(1024 * 1024 * 1024, 2)?;
    let region = sandbox.allocate_region(0, 2 * 1024 * 1024)?;
    // This mapping belongs only to this unstarted test guest.
    unsafe {
        std::ptr::write_bytes(
            region.mapping.address() as *mut u8,
            0xa5,
            region.mapping.len,
        );
    }
    let mut regions = [region];
    let events = unsafe { sandbox.add_device_with_regions(2, 1, 1 << 32, &[0; 64], &mut regions)? };
    assert!(regions[0].mapping.copy().iter().all(|b| *b == 0));
    drop(events);
    drop(sandbox);
    drop(regions);
    eprintln!("REGION_ZERO_OK");
    Ok(())
}
