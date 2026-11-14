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
            addr.as_unix_addr().is_none()
                && addr
                    .as_sockaddr_in()
                    .is_none_or(|addr| addr.port() != 18081)
        }
    }
    let allowed = TcpListener::bind("127.0.0.1:18080").unwrap();
    let denied = TcpListener::bind("127.0.0.1:18081").unwrap();
    allowed.set_nonblocking(true).unwrap();
    denied.set_nonblocking(true).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut builder = krun::Builder::new();
    builder
        .internet_socket_forwarding(true)
        .outbound_policy(Arc::new(Policy(calls.clone())));
    let denied_path =
        std::env::temp_dir().join(format!("embed-denied-{}.sock", std::process::id()));
    let denied_unix = UnixListener::bind(&denied_path).unwrap();
    denied_unix.set_nonblocking(true).unwrap();
    builder.add_vsock_port(7001, denied_path.clone(), false);
    assert!(run(builder, "embed_policy").contains("EMBED_POLICY_OK"));
    assert!(calls.load(Ordering::Relaxed) >= 3);
    assert!(matches!(denied_unix.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    std::fs::remove_file(denied_path).unwrap();
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
#[cfg(target_arch = "x86_64")]
#[test]
#[ignore = "requires KVM and external artifacts"]
fn configured_rule_survives_guest_device_reset() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    let tcp = std::net::TcpListener::bind("127.0.0.1:18080").unwrap();
    tcp.set_nonblocking(true).unwrap();
    for allowed in [false, true] {
        let path =
            std::env::temp_dir().join(format!("embed-reset-{}-{allowed}.sock", std::process::id()));
        let unix = UnixListener::bind(&path).unwrap();
        unix.set_nonblocking(true).unwrap();
        let mut builder = krun::Builder::new();
        builder
            .internet_socket_forwarding(false)
            .add_vsock_port(7000, path.clone(), false);
        struct AllowedPath(std::path::PathBuf);
        impl krun::OutboundPolicy for AllowedPath {
            fn allow(&self, op: krun::SocketOperation, addr: &krun::SocketAddress) -> bool {
                addr.as_unix_addr().is_none()
                    || (op == krun::SocketOperation::UnixConnect
                        && addr.as_unix_addr().and_then(|addr| addr.path())
                            == Some(self.0.as_path()))
            }
        }
        if allowed {
            builder.outbound_policy(Arc::new(AllowedPath(path.clone())));
        } else {
            builder.outbound_policy(Arc::new(krun::AllowlistPolicy::new([])));
        }
        let peer = allowed.then(|| {
            std::thread::spawn(move || {
                for _ in 0..2 {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    let mut stream = loop {
                        match unix.accept() {
                            Ok((stream, _)) => break stream,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(
                                    std::time::Instant::now() < deadline,
                                    "mapped connection timeout"
                                );
                                std::thread::yield_now();
                            }
                            Err(e) => panic!("{e}"),
                        }
                    };
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                        .unwrap();
                    let mut data = [0; 4];
                    stream.read_exact(&mut data).unwrap();
                    assert_eq!(&data, b"ping");
                    stream.write_all(b"pong").unwrap();
                    let mut ack = [0];
                    stream.read_exact(&mut ack).unwrap();
                    assert_eq!(ack, [1]);
                }
            })
        });
        let log = run(
            builder,
            if allowed {
                "embed_reset_allow"
            } else {
                "embed_reset_deny"
            },
        );
        assert!(
            log.contains("EMBED_RESET_BEFORE_OK")
                && log.contains("EMBED_VSOCK_RESET_OK")
                && log.contains("EMBED_RESET_AFTER_OK")
        );
        if let Some(peer) = peer {
            peer.join().unwrap();
        }
        assert!(matches!(tcp.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
        std::fs::remove_file(path).unwrap();
    }
}

/// Backend conformance: a configured deny rule must be enforced, never ignored.
/// Run this same public-API case when adding a backend; unsupported configuration
/// must fail build explicitly rather than boot with an unrestricted device.
#[cfg(target_arch = "x86_64")]
#[test]
#[ignore = "requires KVM and external artifacts"]
fn backend_must_enforce_configured_rule() {
    let listener = std::net::TcpListener::bind("127.0.0.1:18080").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut builder = krun::Builder::new();
    builder
        .internet_socket_forwarding(true)
        .outbound_policy(std::sync::Arc::new(krun::AllowlistPolicy::new([])));
    assert!(run(builder, "embed_deny_all").contains("EMBED_DENY_ALL_OK"));
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}

#[cfg(target_arch = "x86_64")]
#[test]
#[ignore = "requires KVM and external artifacts"]
fn udp_policy_delivery_and_refusal() {
    use std::sync::{Arc, Mutex};
    struct Policy(Arc<Mutex<Vec<(krun::SocketOperation, String)>>>);
    impl krun::OutboundPolicy for Policy {
        fn allow(&self, op: krun::SocketOperation, addr: &krun::SocketAddress) -> bool {
            self.0.lock().unwrap().push((op, addr.to_string()));
            addr.as_sockaddr_in()
                .is_none_or(|addr| addr.port() != 18081)
        }
    }
    let allowed = std::net::UdpSocket::bind("127.0.0.1:18082").unwrap();
    let denied = std::net::UdpSocket::bind("127.0.0.1:18081").unwrap();
    let ipv6 = std::net::UdpSocket::bind("[::1]:18083").unwrap();
    ipv6.set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    allowed
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    denied.set_nonblocking(true).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut builder = krun::Builder::new();
    builder
        .internet_socket_forwarding(true)
        .outbound_policy(Arc::new(Policy(calls.clone())));
    let refused_source = denied.try_clone().unwrap();
    let peer = std::thread::spawn(move || {
        let mut data = [0; 32];
        let (len, source) = allowed.recv_from(&mut data).unwrap();
        assert_eq!(&data[..len], b"allowed");
        refused_source.send_to(b"forbidden", source).unwrap();
        allowed.send_to(b"reply", source).unwrap();
    });
    assert!(run(builder, "embed_udp").contains("EMBED_UDP_OK"));
    peer.join().unwrap();
    let mut data = [0; 32];
    let mut ipv6_builder = krun::Builder::new();
    ipv6_builder
        .internet_socket_forwarding(true)
        .outbound_policy(std::sync::Arc::new(krun::AllowlistPolicy::new([
            (
                krun::SocketOperation::Bind,
                "[::]:0".parse::<std::net::SocketAddr>().unwrap().into(),
            ),
            (
                krun::SocketOperation::UdpSend,
                "[::1]:18083"
                    .parse::<std::net::SocketAddr>()
                    .unwrap()
                    .into(),
            ),
        ])));
    assert!(run(ipv6_builder, "embed_dgram_families").contains("EMBED_DGRAM_FAMILIES_OK"));
    let (len, _) = ipv6.recv_from(&mut data).unwrap();
    assert_eq!(&data[..len], b"ipv6");
    assert!(
        matches!(denied.recv_from(&mut data), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    );
    let calls = calls.lock().unwrap();
    assert!(calls.iter().any(
        |(op, addr)| *op == krun::SocketOperation::ReceiveDatagram && addr == "127.0.0.1:18082"
    ));
    assert!(calls
        .iter()
        .any(|(op, addr)| *op == krun::SocketOperation::UdpSend && addr == "127.0.0.1:18081"));
    assert!(calls
        .iter()
        .any(|(op, addr)| *op == krun::SocketOperation::UdpConnect && addr == "127.0.0.1:18081"));
}

#[cfg(all(target_arch = "x86_64", feature = "blk"))]
#[path = "embed_backends.rs"]
mod backends;

#[cfg(all(target_arch = "x86_64", feature = "blk"))]
#[test]
#[ignore = "requires KVM and external artifacts"]
fn filesystem_initialization_failure_is_reported() {
    let mut builder = krun::Builder::new();
    builder.add_virtiofs("embed_bad", Box::new(backends::FailingTree), None);
    assert!(run(builder, "embed_fs_error").contains("EMBED_FS_ERROR_OK"));
}

#[cfg(target_arch = "x86_64")]
fn run(mut builder: krun::Builder, mode: &str) -> String {
    let console = std::env::temp_dir().join(format!("embed-{}-{mode}.log", std::process::id()));
    let ram = match mode {
        "embed_4g" => 4096,
        "embed_8g" => 8192,
        _ => 512,
    };
    builder.vm_config(1, ram).unwrap();
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
    run(krun::Builder::new(), "embed_4g");
    run(krun::Builder::new(), "embed_8g");
}

#[cfg(target_arch = "x86_64")]
#[test]
#[ignore = "requires KVM and external artifacts"]
fn oversized_changed_initramfs_returns_error() {
    let file = utils::tempfile::TempFile::new().unwrap();
    let mut builder = krun::Builder::new();
    builder.vm_config(1, 128).unwrap();
    builder
        .external_kernel(
            std::env::var_os("EMBED_KERNEL").unwrap().into(),
            krun::KernelFormat::Elf,
            Some(file.as_path().to_owned()),
        )
        .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(file.as_path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    assert!(builder.build().is_err());
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
