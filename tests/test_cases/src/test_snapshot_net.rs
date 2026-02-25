//! Integration test for network connectivity after snapshot/restore.
//!
//! Tests AC6.4: Guest network connectivity works after snapshot/restore

use macros::{guest, host};

pub struct TestSnapshotNet;

const VSOCK_PORT: u32 = 5683;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::loopback_net::LoopbackFactory;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    use std::thread;

    impl Test for TestSnapshotNet {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("snap_net_control.sock");
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;

            // Add loopback network backend
            builder.add_net_device(
                krun::VirtioNetBackend::CustomAsyncFactory(
                    Box::new(LoopbackFactory::new()),
                ),
                [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee], // Guest MAC
                0, // features
            );

            builder.add_vsock_port(VSOCK_PORT, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();

            let vm_thread = thread::spawn(move || context.run());

            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Phase 1: Wait for guest to confirm networking works pre-snapshot
            let mut buf = vec![0u8; 6];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"NET_OK");

            // Take full snapshot
            handle.snapshot(&snap_dir)?;

            // Hot-restore the snapshot
            handle.restore_snapshot(&snap_dir)?;

            // Phase 2: Signal guest to verify networking post-restore
            stream.write_all(b"RESTORED").unwrap();

            // Wait for guest to confirm networking works post-restore
            let mut buf = vec![0u8; 20];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"NET_OK_AFTER_RESTORE");

            vm_thread.join().ok();
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use crate::net_helpers::{configure_eth0, test_ping};
    use nix::libc::VMADDR_CID_HOST;
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;


    impl Test for TestSnapshotNet {
        fn in_guest(self: Box<Self>) {
            // Configure network interface
            configure_eth0();

            let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
                .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Phase 1: Test networking pre-snapshot
            test_ping();

            // Signal host that networking works
            stream.write_all(b"NET_OK").unwrap();

            // Wait for RESTORED signal (after snapshot+restore)
            let mut buf = vec![0u8; 8];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"RESTORED");

            // Phase 2: Test networking post-snapshot/restore
            test_ping();

            // Signal host that networking still works after restore
            stream.write_all(b"NET_OK_AFTER_RESTORE").unwrap();

            println!("OK");
        }
    }
}
