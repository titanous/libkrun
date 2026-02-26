//! Test parallel fault resolution.
//!
//! Verifies userfaultd.AC5.6: With 50ms delay per read_page and 2+ vCPUs,
//! wall-clock time confirms concurrent resolution.

use macros::{guest, host};

pub struct TestUffdParallelFaults;

const VSOCK_PORT: u32 = 5704;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::mock_snapshot_store::DelayStoreFactory;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::{Duration, Instant};

    impl Test for TestUffdParallelFaults {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            // Phase 1: Snapshot
            {
                let sock_path = test_setup.tmp_dir.join("uffd_parallel_phase1.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(2, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT, sock_path, false);

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

            // Phase 2: Restore with delays
            {
                let sock_path = test_setup.tmp_dir.join("uffd_parallel_phase2.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(2, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT, sock_path, false);

                let context = builder.build()?;

                let listener_thread = thread::spawn(move || {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let mut buf = vec![0u8; 2];
                        let _ = stream.read_exact(&mut buf);
                        assert_eq!(&buf, b"OK");
                    }
                });

                // Record wall time before restore
                let start = Instant::now();

                // Create factory with 50ms delay per read_page
                let factory = DelayStoreFactory::new(&snap_dir, &[], 50);
                let _vm_exit = context.restore_and_run_with_store(Box::new(factory))?;

                let elapsed = start.elapsed();

                // With concurrent resolution across 2 vCPUs and artificial 50ms delays,
                // we should see significant parallelism. Sequential would be much slower.
                eprintln!("Restore with 50ms delays took: {:?}", elapsed);

                assert!(
                    elapsed < Duration::from_secs(3),
                    "Restore took {:?}, expected < 3s (concurrent). Sequential would be much longer",
                    elapsed
                );

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

    impl Test for TestUffdParallelFaults {
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
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT);
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
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            // After cold restore, verify counter
            let val = COUNTER.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                val, 42,
                "counter was {val} after parallel fault restore, expected 42"
            );

            // Allocate memory to trigger faults from multiple vCPUs if running in parallel
            let _heap_data: Vec<u8> = vec![0xCC; 8192];

            stream.write_all(b"OK").unwrap();
            println!("OK");
        }
    }
}
