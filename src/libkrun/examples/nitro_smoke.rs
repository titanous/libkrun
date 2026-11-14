#[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use krun::{Builder, NitroBackendConfig, VmExit};
    use devices::virtio::{CacheType, PortDescription};
    use devices::virtio::block::{BlockDeviceType, ImageType, SyncMode};
    use vmm::vmm_config::block::BlockDeviceConfig;
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 { return Err("usage: nitro_smoke EIF DISK UNIX_SOCKET".into()); }
    let mut builder = Builder::new();
    builder.vm_config(2, 1024)?;
    builder.disable_implicit_console()?;
    builder.add_virtio_console_with_ports(vec![PortDescription::output_pipe("smoke", devices::virtio::port_io::stdout()?)]);
    builder.add_block_cfg(BlockDeviceConfig {
        block_id: "disk".into(), cache_type: CacheType::Writeback,
        disk_type: BlockDeviceType::Image { path: args[2].clone(), format: ImageType::Raw, sync_mode: SyncMode::Full },
        is_disk_read_only: false, direct_io: false,
    });
    builder.add_vsock_port(7000, args[3].clone().into(), false);
    let cid = std::env::var("NITRO_CID").unwrap_or_else(|_| "200".into()).parse()?;
    builder.nitro_backend(NitroBackendConfig { image: args[1].clone().into(), cid, watchdog: std::time::Duration::from_secs(15) });
    match builder.build()?.run()? {
        VmExit::Shutdown { .. } => Ok(()),
        other => Err(format!("unexpected exit: {other:?}").into()),
    }
}
#[cfg(not(all(target_os = "linux", feature = "nitro-sandbox")))]
fn main() { eprintln!("requires Linux and --features nitro-sandbox"); std::process::exit(2); }
