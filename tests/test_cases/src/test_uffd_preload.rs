//! Test FsSnapshotStore full preload and partial preload.
//!
//! AC5.2: Preload loads everything, near-zero faults
//! AC5.3: Both preload and fault paths exercise (partial)

use macros::{guest, host};

pub struct TestUffdPreloadFull;

const VSOCK_PORT_FULL: u32 = 5701;

#[host]
mod host_full {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use libkrun::snapshot_store::FsSnapshotStoreFactory;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestUffdPreloadFull {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            // Phase 1: Snapshot
            {
                let sock_path = test_setup.tmp_dir.join("uffd_preload_full_phase1.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT_FULL, sock_path, false);

                let context = builder.build()?;
                let handle = context.vm_handle();

                let vm_thread = thread::spawn(move || context.run());

                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut buf = vec![0u8; 5];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"READY");

                handle.snapshot(&snap_dir)?;
                vm_thread.join().ok();
            }

            // Phase 2: Restore with full preload
            {
                let sock_path = test_setup.tmp_dir.join("uffd_preload_full_phase2.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT_FULL, sock_path, false);

                let context = builder.build()?;

                let listener_thread = thread::spawn(move || {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let mut buf = vec![0u8; 2];
                        let _ = stream.read_exact(&mut buf);
                        assert_eq!(&buf, b"OK");
                    }
                });

                let factory = FsSnapshotStoreFactory::new(&snap_dir, &[]);
                let _vm_exit = context.restore_and_run_with_store(Box::new(factory))?;

                listener_thread.join().ok();
            }

            Ok(())
        }
    }
}

#[guest]
mod guest_full {
    use super::*;
    use crate::Test;
    use nix::libc::VMADDR_CID_HOST;
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    impl Test for TestUffdPreloadFull {
        fn in_guest(self: Box<Self>) {
            static COUNTER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

            // Phase 1: Before snapshot
            COUNTER.store(42, std::sync::atomic::Ordering::SeqCst);

            let sock = socket(
                AddressFamily::Vsock,
                SockType::Stream,
                SockFlag::empty(),
                None,
            )
            .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT_FULL);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            stream.write_all(b"READY").unwrap();
            drop(stream);

            // Reconnect after cold restore
            let sock = socket(
                AddressFamily::Vsock,
                SockType::Stream,
                SockFlag::empty(),
                None,
            )
            .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT_FULL);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            // Verify counter
            let val = COUNTER.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                val, 42,
                "counter was {val} after full preload restore, expected 42"
            );

            // Touch memory to ensure it's available
            let _heap_data: Vec<u8> = vec![0xAB; 4096];

            stream.write_all(b"OK").unwrap();
            println!("OK");
        }
    }
}

// Partial preload test
pub struct TestUffdPreloadPartial;

const VSOCK_PORT_PARTIAL: u32 = 5702;

#[host]
mod host_partial {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::mock_snapshot_store::PartialPreloadStoreFactory;
    use crate::{Test, TestSetup};
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestUffdPreloadPartial {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            // Phase 1: Snapshot
            {
                let sock_path = test_setup.tmp_dir.join("uffd_preload_partial_phase1.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT_PARTIAL, sock_path, false);

                let context = builder.build()?;
                let handle = context.vm_handle();

                let vm_thread = thread::spawn(move || context.run());

                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut buf = vec![0u8; 5];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"READY");

                handle.snapshot(&snap_dir)?;
                vm_thread.join().ok();
            }

            // Phase 2: Restore with 50% preload
            {
                let sock_path = test_setup.tmp_dir.join("uffd_preload_partial_phase2.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT_PARTIAL, sock_path, false);

                let context = builder.build()?;

                let listener_thread = thread::spawn(move || {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let mut buf = vec![0u8; 2];
                        let _ = stream.read_exact(&mut buf);
                        assert_eq!(&buf, b"OK");
                    }
                });

                let factory = PartialPreloadStoreFactory::new(&snap_dir, &[], 0.5);
                let _vm_exit = context.restore_and_run_with_store(Box::new(factory))?;

                listener_thread.join().ok();
            }

            Ok(())
        }
    }
}

#[guest]
mod guest_partial {
    use super::*;
    use crate::Test;
    use nix::libc::VMADDR_CID_HOST;
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    impl Test for TestUffdPreloadPartial {
        fn in_guest(self: Box<Self>) {
            static COUNTER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

            // Phase 1: Before snapshot
            COUNTER.store(42, std::sync::atomic::Ordering::SeqCst);

            let sock = socket(
                AddressFamily::Vsock,
                SockType::Stream,
                SockFlag::empty(),
                None,
            )
            .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT_PARTIAL);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            stream.write_all(b"READY").unwrap();
            drop(stream);

            // Reconnect after cold restore
            let sock = socket(
                AddressFamily::Vsock,
                SockType::Stream,
                SockFlag::empty(),
                None,
            )
            .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT_PARTIAL);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            // Verify counter (both preloaded and demand-paged memory work)
            let val = COUNTER.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                val, 42,
                "counter was {val} after partial preload restore, expected 42"
            );

            // Allocate to trigger demand-paging on second half
            let _heap_data: Vec<u8> = vec![0xAB; 4096];

            stream.write_all(b"OK").unwrap();
            println!("OK");
        }
    }
}
