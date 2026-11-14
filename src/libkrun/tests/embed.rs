//! Real KVM tests; build embed-init.c as static /init in EMBED_INITRAMFS.
//! Set EMBED_KERNEL to ELF vmlinux. Run with an outer timeout and one test thread.
#[cfg(target_arch = "x86_64")]
#[test]
#[ignore = "requires KVM and external artifacts"]
fn outbound_policy_and_disabled_forwarding() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::net::UnixListener;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Policy(Arc<AtomicUsize>);
    impl krun::OutboundPolicy for Policy {
        fn allow(&self, _: krun::SocketOperation, addr: &krun::SocketAddress) -> bool {
            self.0.fetch_add(1, Ordering::Relaxed);
            addr.as_sockaddr_in()
                .is_none_or(|addr| addr.port() != 18081)
        }
    }
    let allowed = TcpListener::bind("127.0.0.1:18080").unwrap();
    let denied = TcpListener::bind("127.0.0.1:18081").unwrap();
    allowed.set_nonblocking(true).unwrap();
    denied.set_nonblocking(true).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut builder = krun::Builder::new();
    builder.outbound_policy(Arc::new(Policy(calls.clone())));
    assert!(run(builder, "embed_policy").contains("EMBED_POLICY_OK"));
    assert!(calls.load(Ordering::Relaxed) >= 2);
    assert!(allowed.accept().is_ok());
    assert!(matches!(denied.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
    let socket = std::env::temp_dir().join(format!("embed-vsock-{}.sock", std::process::id()));
    let unix = UnixListener::bind(&socket).unwrap();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = unix.accept().unwrap();
        let timeout = Some(std::time::Duration::from_secs(5));
        stream.set_read_timeout(timeout).unwrap();
        stream.set_write_timeout(timeout).unwrap();
        let mut data = [0; 4];
        stream.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"ping");
        stream.write_all(b"pong").unwrap();
        let mut ack = [0; 1];
        stream.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [1]);
    });
    let mut builder = krun::Builder::new();
    builder
        .internet_socket_forwarding(false)
        .add_vsock_port(7000, socket.clone(), false);
    assert!(run(builder, "embed_disabled").contains("EMBED_DISABLED_OK"));
    peer.join().unwrap();
    std::fs::remove_file(socket).unwrap();
    assert!(matches!(allowed.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
}
#[cfg(all(target_arch = "x86_64", feature = "blk"))]
#[path = "embed_backends.rs"]
mod backends;

#[cfg(target_arch = "x86_64")]
fn run(mut builder: krun::Builder, mode: &str) -> String {
    let console = std::env::temp_dir().join(format!("embed-{}-{mode}.log", std::process::id()));
    builder.vm_config(1, 512).unwrap();
    builder
        .external_kernel(
            std::env::var_os("EMBED_KERNEL")
                .expect("EMBED_KERNEL")
                .into(),
            krun::KernelFormat::Elf,
            Some(
                std::env::var_os("EMBED_INITRAMFS")
                    .expect("EMBED_INITRAMFS")
                    .into(),
            ),
        )
        .unwrap();
    builder.console_output(console.clone());
    builder.set_kernel_cmdline(vec![
        "console=hvc0",
        "nokaslr",
        "panic=-1",
        "reboot=k",
        "rdinit=/init",
        mode,
    ]);
    let result = builder.build().expect("build external guest").run();
    assert!(
        matches!(result, Ok(krun::VmExit::Shutdown { .. })),
        "{result:?}"
    );
    let log = std::fs::read_to_string(&console).unwrap();
    std::fs::remove_file(console).unwrap();
    assert!(
        log.contains("EMBED_USERSPACE_OK")
            && log.contains("EMBED_ALL_OK")
            && !log.contains("EMBED_FAIL"),
        "{log}"
    );
    log
}

#[cfg(target_arch = "x86_64")]
#[test]
#[ignore = "requires KVM and external artifacts"]
fn external_initramfs_boot() {
    run(krun::Builder::new(), "embed_boot");
}

#[cfg(all(target_arch = "x86_64", feature = "blk"))]
#[test]
#[ignore = "requires KVM and external artifacts"]
fn caller_disk_and_filesystem() {
    let disk = std::sync::Arc::new(backends::MemoryDisk(std::sync::Mutex::new(vec![
        0;
        1024 * 1024
    ])));
    let mut builder = krun::Builder::new();
    builder.add_block_backend("memory".into(), disk.clone(), false);
    builder.add_virtiofs("embed_tree", Box::new(backends::ReadOnlyTree), None);
    let log = run(builder, "embed_devices");
    assert!(
        log.contains("EMBED_DISK_OK") && log.contains("EMBED_FS_OK"),
        "{log}"
    );
    assert_eq!(&disk.0.lock().unwrap()[4096..4608], &[0x5a; 512]);
}
