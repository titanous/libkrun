//! Minimal separate-process cooperative freeze/clone proof launcher.
use std::path::PathBuf;
mod cooperative_fs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: cooperative-console freeze|resume TEMPLATE CONTROL_GPA_HEX".into());
    }
    let control = u64::from_str_radix(args[3].trim_start_matches("0x"), 16)?;
    let mut builder = krun::Builder::new();
    builder.omit_balloon_device();
    if let Some(mode) = std::env::var_os("CLONE_FS") {
        let dax = if mode == "dax" { Some(64 << 20) } else { None };
        builder.add_virtiofs("clone_ro", Box::new(cooperative_fs::Tree::new(true)), dax);
        builder.add_virtiofs("clone_rw", Box::new(cooperative_fs::Tree::new(false)), dax);
    }
    if let Some(kernel) = std::env::var_os("CLONE_KERNEL") {
        let initramfs = std::env::var_os("CLONE_INITRAMFS").map(PathBuf::from);
        builder.external_kernel(PathBuf::from(kernel), krun::KernelFormat::Elf, initramfs)?;
    }
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
        let path = PathBuf::from(socket);
        struct EchoPolicy(PathBuf);
        impl krun::OutboundPolicy for EchoPolicy {
            fn allow(&self, op: krun::SocketOperation, addr: &krun::SocketAddress) -> bool {
                addr.as_unix_addr().is_none()
                    || (op == krun::SocketOperation::UnixConnect
                        && addr.as_unix_addr().and_then(|address| address.path())
                            == Some(self.0.as_path()))
            }
        }
        builder.internet_socket_forwarding(false);
        if std::env::var_os("CLONE_DENY").is_some() {
            builder.outbound_policy(std::sync::Arc::new(krun::AllowlistPolicy::new([])));
        } else {
            builder.outbound_policy(std::sync::Arc::new(EchoPolicy(path.clone())));
        }
        builder.add_vsock_port(7000, path, false);
    }
    builder.set_kernel_cmdline(vec![
        "console=ttyS0",
        "earlycon=uart,io,0x3f8,115200",
        "rdinit=/init",
        "panic=-1",
        "reboot=k",
    ]);
    if std::env::var_os("CLONE_FS").is_some_and(|mode| mode == "dax") {
        builder.add_kernel_cmdline_args(&["clone_dax"]);
    }
    if std::env::var_os("CLONE_HVC_CONSOLE").is_some() {
        builder.add_kernel_cmdline_args(&["console=hvc0"]);
    }
    if args[1] == "host-freeze" {
        builder.future_template();
    } else if args[1] == "freeze" || args[1] == "refuse" || args[1] == "unsolicited" {
        builder.capture_cooperative_template(control, PathBuf::from(&args[2]));
    } else if args[1] == "resume" {
        builder.start_from_template_path(&args[2])?;
    } else if args[1] == "resume-fd" {
        use std::os::fd::FromRawFd;
        let fd: i32 = args[2].parse()?;
        // SAFETY: test parent passes ownership of this inherited descriptor.
        builder.start_from_template_fd(unsafe { std::fs::File::from_raw_fd(fd) })?;
    } else {
        return Err("invalid mode".into());
    }
    let context = builder.build()?;
    if args[1] == "freeze" || args[1] == "refuse" {
        context
            .vm_handle()
            .arm_template_capture(std::time::Duration::from_secs(30))?;
    }
    let capture = if args[1] == "host-freeze" {
        let handle = context.vm_handle();
        let path = PathBuf::from(&args[2]);
        Some(std::thread::spawn(move || -> std::io::Result<()> {
            let template = handle.freeze_template(std::time::Duration::from_secs(30))?;
            vmm::template::Template::open(template.file.try_clone()?)?;
            template.write_to_path(path)?;
            eprintln!("HOSTREQUEST SEALED TEMPLATE received");
            Ok(())
        }))
    } else {
        None
    };
    let result = context.run()?;
    if let Some(capture) = capture {
        capture.join().expect("capture thread")?;
    }
    eprintln!("COOPERATIVE HOST result={result:?}");
    match (args[1].as_str(), result) {
        ("freeze" | "host-freeze", krun::VmExit::Frozen { .. }) => Ok(()),
        (
            "resume" | "resume-fd" | "refuse" | "unsolicited",
            krun::VmExit::Shutdown { exit_code: 0 },
        ) => Ok(()),
        _ => Err("unexpected VM exit".into()),
    }
}
