//! Minimal separate-process cooperative freeze/clone proof launcher.
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: cooperative-console freeze|resume TEMPLATE CONTROL_GPA_HEX".into());
    }
    let control = u64::from_str_radix(args[3].trim_start_matches("0x"), 16)?;
    let mut builder = krun::Builder::new();
    let vcpus = std::env::var("CLONE_VCPUS")
        .unwrap_or_else(|_| "1".into())
        .parse()?;
    builder.vm_config(vcpus, 256)?;
    if let Some(disk) = std::env::var_os("CLONE_DISK") {
        builder.add_block_cfg(vmm::vmm_config::block::BlockDeviceConfig {
            block_id: "clone-data".into(),
            cache_type: devices::virtio::CacheType::Writeback,
            disk_type: devices::virtio::block::device::BlockDeviceType::Image {
                path: PathBuf::from(disk).to_string_lossy().into_owned(),
                format: devices::virtio::block::ImageType::Raw,
                sync_mode: devices::virtio::block::SyncMode::Full,
            },
            is_disk_read_only: false,
            direct_io: false,
        });
    }
    if let Some(socket) = std::env::var_os("CLONE_ECHO_SOCKET") {
        builder.add_vsock_port(7000, PathBuf::from(socket), false);
    }
    builder.set_kernel_cmdline(vec![
        "console=ttyS0",
        "earlycon=uart,io,0x3f8,115200",
        "rdinit=/init",
        "panic=-1",
        "reboot=k",
        "nokaslr",
        "initcall_debug",
        "pm_print_times",
    ]);
    if std::env::var_os("CLONE_HVC_CONSOLE").is_some() {
        builder.add_kernel_cmdline_args(&["console=hvc0"]);
    }
    if args[1] == "freeze" {
        builder.capture_cooperative_template(control, PathBuf::from(&args[2]));
    } else if args[1] == "resume" {
        builder.start_from_template_path(&args[2])?;
    } else {
        return Err("invalid mode".into());
    }
    let result = builder.build()?.run()?;
    eprintln!("COOPERATIVE HOST result={result:?}");
    match (args[1].as_str(), result) {
        ("freeze", krun::VmExit::Frozen { .. }) => Ok(()),
        ("resume", krun::VmExit::Shutdown { exit_code: 0 }) => Ok(()),
        _ => Err("unexpected VM exit".into()),
    }
}
