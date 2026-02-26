//! Test incremental chain via demand-paging.
//!
//! Verifies userfaultd.AC5.4: Base + 2 incrementals via demand-paging.
//! Latest dirty pages win.

use macros::{guest, host};

pub struct TestUffdIncrementalChain;

const VSOCK_PORT: u32 = 5703;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use libkrun::snapshot_store::FsSnapshotStoreFactory;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestUffdIncrementalChain {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("uffd_incr_control.sock");
            let full_snap_dir = test_setup.tmp_dir.join("snap_base");
            let incr_snap_dir_1 = test_setup.tmp_dir.join("snap_incr_1");
            let incr_snap_dir_2 = test_setup.tmp_dir.join("snap_incr_2");

            let listener = UnixListener::bind(&sock_path).unwrap();

            // Phase 1-3: Build VM, take snapshots through incremental chain
            {
                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT, sock_path.clone(), false);

                let context = builder.build()?;
                let handle = context.vm_handle();

                let vm_thread = thread::spawn(move || context.run());

                // Phase 1: Wait for READY (counter = 100)
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut buf = vec![0u8; 5];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"READY");

                // Take full snapshot
                handle.snapshot(&full_snap_dir)?;

                // Phase 2: Enable dirty tracking, mutate to 200
                handle.enable_dirty_tracking()?;
                stream.write_all(b"MUT1").unwrap();

                let mut buf = vec![0u8; 7];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"WRITTEN");

                // Take incremental snapshot 1
                handle.incremental_snapshot(&incr_snap_dir_1)?;

                // Phase 3: Mutate to 300
                stream.write_all(b"MUT2").unwrap();

                let mut buf = vec![0u8; 7];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"WRITTEN");

                // Take incremental snapshot 2
                handle.incremental_snapshot(&incr_snap_dir_2)?;

                // Signal guest to exit
                drop(stream);
                vm_thread.join().ok();
            }

            // Phase 4: Cold restore with incremental chain
            {
                let sock_path2 = test_setup.tmp_dir.join("uffd_incr_restore.sock");
                let listener2 = UnixListener::bind(&sock_path2).unwrap();

                let mut builder2 = krun::Builder::new();
                builder2.vm_config(1, 256)?;
                setup_fs_builder(&mut builder2, &test_setup)?;
                builder2.add_vsock_port(VSOCK_PORT, sock_path2, false);

                let context2 = builder2.build()?;

                let listener_thread = thread::spawn(move || {
                    if let Ok((mut stream, _)) = listener2.accept() {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let mut buf = vec![0u8; 2];
                        let _ = stream.read_exact(&mut buf);
                        assert_eq!(&buf, b"OK");
                    }
                });

                // Create factory with base and incrementals
                let factory = FsSnapshotStoreFactory::new(&full_snap_dir, &[&incr_snap_dir_1, &incr_snap_dir_2]);
                let _vm_exit = context2.restore_and_run_with_store(Box::new(factory))?;

                listener_thread.join().ok();
            }

            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use nix::libc::VMADDR_CID_HOST;
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    impl Test for TestUffdIncrementalChain {
        fn in_guest(self: Box<Self>) {
            static COUNTER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

            let sock = socket(
                AddressFamily::Vsock,
                SockType::Stream,
                SockFlag::empty(),
                None,
            )
            .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            // Phase 1: Set counter to 100, signal READY
            COUNTER.store(100, std::sync::atomic::Ordering::SeqCst);
            stream.write_all(b"READY").unwrap();

            // Phase 2: Wait for MUT1, set counter to 200, signal WRITTEN
            let mut buf = vec![0u8; 4];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"MUT1");
            COUNTER.store(200, std::sync::atomic::Ordering::SeqCst);
            stream.write_all(b"WRITTEN").unwrap();

            // Phase 3: Wait for MUT2, set counter to 300, signal WRITTEN
            let mut buf = vec![0u8; 4];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"MUT2");
            COUNTER.store(300, std::sync::atomic::Ordering::SeqCst);
            stream.write_all(b"WRITTEN").unwrap();

            // Drop connection (cold restore happens here)
            drop(stream);

            // Reconnect after cold restore
            let sock = socket(
                AddressFamily::Vsock,
                SockType::Stream,
                SockFlag::empty(),
                None,
            )
            .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            // After cold restore from incremental chain, counter should be 300
            let val = COUNTER.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(val, 300, "counter was {val} after incremental chain restore, expected 300 (latest dirty page)");

            stream.write_all(b"OK").unwrap();
            println!("OK");
        }
    }
}
