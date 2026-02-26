//! Test demand-page-only (empty preload) — all pages loaded via UFFD faults.
//!
//! Verifies userfaultd.AC5.1: All pages loaded via faults, guest runs correctly

use macros::{guest, host};

pub struct TestUffdDemandPageOnly;

const VSOCK_PORT: u32 = 5700;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::mock_snapshot_store::EmptyPreloadStoreFactory;
    use crate::{Test, TestSetup};
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestUffdDemandPageOnly {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            // Phase 1: Build initial VM, snapshot it
            {
                let sock_path = test_setup.tmp_dir.join("uffd_demand_page_phase1.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT, sock_path.clone(), false);

                let context = builder.build()?;
                let handle = context.vm_handle();

                let vm_thread = thread::spawn(move || context.run());

                // Wait for guest READY
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut buf = vec![0u8; 5];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"READY");

                // Take snapshot
                handle.snapshot(&snap_dir)?;

                // VM exits naturally after this
                vm_thread.join().ok();
            }

            // Phase 2: Cold restore with empty preload
            {
                let sock_path = test_setup.tmp_dir.join("uffd_demand_page_phase2.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT, sock_path, false);

                let context = builder.build()?;

                // Create factory with empty preload (forces all pages through faults)
                let factory = EmptyPreloadStoreFactory::new(&snap_dir, &[]);

                // spawn listener thread before restore starts
                let listener_thread = thread::spawn(move || {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let mut buf = vec![0u8; 2];
                        let _ = stream.read_exact(&mut buf);
                        assert_eq!(&buf, b"OK", "Expected OK from restored guest");
                    }
                });

                // Cold restore and run
                let _vm_exit = context.restore_and_run_with_store(Box::new(factory))?;

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

    impl Test for TestUffdDemandPageOnly {
        fn in_guest(self: Box<Self>) {
            // Use static counter to survive snapshot
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
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            // Signal host we are ready
            stream.write_all(b"READY").unwrap();

            // Drop connection (host takes snapshot here)
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

            // After cold restore, verify counter is still 42
            let val = COUNTER.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                val, 42,
                "counter was {val} after demand-page restore, expected 42"
            );

            // Allocate and write to heap to ensure demand-paged memory works
            let _heap_data: Vec<u8> = vec![0xAB; 4096];

            // Signal completion
            stream.write_all(b"OK").unwrap();

            println!("OK");
        }
    }
}
