#[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use devices::virtio::block::{BlockDeviceType, ImageType, SyncMode};
    use devices::virtio::{CacheType, PortDescription};
    use krun::{Builder, NitroBackendConfig, VmExit};
    use vmm::vmm_config::block::BlockDeviceConfig;
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: nitro_smoke EIF DISK UNIX_SOCKET".into());
    }
    let mut builder = Builder::new();
    builder.nitro_console_sink(Box::new(std::io::stdout()), 1024 * 1024, 256 * 1024);
    let memory_mib = std::env::var("NITRO_MEMORY_MIB")
        .unwrap_or_else(|_| "1024".into())
        .parse()?;
    let cid = std::env::var("NITRO_CID")
        .unwrap_or_else(|_| "200".into())
        .parse()?;
    let expected_watchdog = std::env::var_os("NITRO_EXPECT_WATCHDOG").is_some();
    builder.nitro_backend(NitroBackendConfig {
        image: args[1].clone().into(),
        cid,
        watchdog: std::time::Duration::from_secs(if expected_watchdog { 3 } else { 15 }),
    });
    builder.vm_config(2, memory_mib)?;
    builder.disable_implicit_console()?;
    builder.add_block_cfg(BlockDeviceConfig {
        block_id: "disk".into(),
        cache_type: CacheType::Writeback,
        disk_type: BlockDeviceType::Image {
            path: args[2].clone(),
            format: ImageType::Raw,
            sync_mode: SyncMode::Full,
        },
        is_disk_read_only: false,
        direct_io: false,
    });
    builder.add_vsock_port(7000, args[3].clone().into(), false);
    if std::env::var_os("NITRO_DENY_ALL").is_some() {
        builder.internet_socket_forwarding(true);
        builder.outbound_policy(std::sync::Arc::new(krun::AllowlistPolicy::new([])));
    }
    let console_path =
        std::env::var("NITRO_CONSOLE_LOG").unwrap_or_else(|_| "virtio-console.log".into());
    let output = devices::virtio::port_io::output_file(std::fs::File::create(console_path)?)?;
    let paths =
        builder.add_virtio_console_with_ports(vec![PortDescription::output_pipe("smoke", output)]);
    assert_eq!(paths, ["/dev/vport2p0"]);
    match builder.build()?.run()? {
        VmExit::Error { message }
            if expected_watchdog && message == "Nitro guest watchdog expired" =>
        {
            eprintln!("REBOOT_WATCHDOG_OK");
            Ok(())
        }
        VmExit::Shutdown { .. } if !expected_watchdog => Ok(()),
        other => Err(format!("unexpected exit: {other:?}").into()),
    }
}
#[cfg(not(all(target_os = "linux", feature = "nitro-sandbox")))]
fn main() {
    eprintln!("requires Linux and --features nitro-sandbox");
    std::process::exit(2);
}
