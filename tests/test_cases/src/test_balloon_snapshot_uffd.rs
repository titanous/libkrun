//! Integration test: balloon inflate → snapshot → UFFD cold restore (AC3.4).
//!
//! Phase 1: Boot VM with balloon, set known static values, inflate 64MB,
//!          snapshot, exit.
//! Phase 2: Cold UFFD restore with empty preload. Balloon-reclaimed pages
//!          (absent in snapshot) are zero-filled by UFFD fault handler.
//!          Non-reclaimed static data is restored from store.

use macros::{guest, host};

pub struct TestBalloonSnapshotUffd;

const VSOCK_PORT: u32 = 5721;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::mock_snapshot_store::EmptyPreloadStoreFactory;
    use crate::{Test, TestSetup};
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestBalloonSnapshotUffd {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("balloon_snap_uffd");

            // Phase 1: Boot with balloon, inflate, snapshot, exit
            {
                let sock_path = test_setup.tmp_dir.join("bsu_phase1.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT, sock_path, false);
                builder.enable_balloon();

                let context = builder.build()?;
                let handle = context.vm_handle();
                let balloon = handle
                    .balloon()
                    .expect("balloon() should return Some after enable_balloon()");

                let vm_thread = thread::spawn(move || context.run());

                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();

                // Wait for guest to signal ready with known static values set
                let mut buf = vec![0u8; 5];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"READY");

                // Inflate 64MB — those pages become absent in the snapshot
                balloon
                    .resize(64)
                    .map_err(|e| anyhow::anyhow!("balloon resize failed: {e:?}"))?;
                balloon
                    .await_target(64, Duration::from_secs(5), Some(Duration::from_secs(30)))
                    .map_err(|e| anyhow::anyhow!("balloon await_target failed: {e:?}"))?;

                handle.snapshot(&snap_dir)?;

                // Guest exits after dropping connection
                drop(stream);
                vm_thread.join().ok();
            }

            // Phase 2: Cold UFFD restore with empty preload
            // Absent pages (balloon-reclaimed) are zero-filled; present pages are
            // loaded from store. Guest verifies static data and heap allocation.
            {
                let sock_path = test_setup.tmp_dir.join("bsu_phase2.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT, sock_path, false);
                builder.enable_balloon();

                let context = builder.build()?;

                let factory =
                    EmptyPreloadStoreFactory::new(&snap_dir, &[] as &[&std::path::Path]);

                let listener_thread = thread::spawn(move || {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
                        let mut buf = vec![0u8; 2];
                        let _ = stream.read_exact(&mut buf);
                        assert_eq!(&buf, b"OK", "expected OK from guest after UFFD restore");
                    }
                });

                context.restore_and_run_with_store(Box::new(factory))?;

                listener_thread.join().ok();
            }

            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::vsock_helpers::vsock_connect;
    use crate::Test;
    use std::io::Write;

    impl Test for TestBalloonSnapshotUffd {
        fn in_guest(self: Box<Self>) {
            // Static values that must survive UFFD cold restore (present-page path)
            static COUNTER: std::sync::atomic::AtomicI32 =
                std::sync::atomic::AtomicI32::new(0);
            static PATTERN: std::sync::atomic::AtomicU8 =
                std::sync::atomic::AtomicU8::new(0);

            // Phase 1: set values and signal ready
            COUNTER.store(99, std::sync::atomic::Ordering::SeqCst);
            PATTERN.store(0xCC, std::sync::atomic::Ordering::SeqCst);

            let mut stream = vsock_connect(VSOCK_PORT);
            stream.write_all(b"READY").unwrap();

            // Drop connection — host inflates balloon, then takes snapshot
            drop(stream);

            // Phase 2: reconnect after cold UFFD restore
            let mut stream = vsock_connect(VSOCK_PORT);

            // Verify present-page static data was restored from store
            let counter = COUNTER.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                counter, 99,
                "counter should be 99 after UFFD restore, got {counter}"
            );
            let pattern = PATTERN.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                pattern, 0xCC,
                "pattern should be 0xCC after UFFD restore, got {pattern:#x}"
            );

            // Allocate heap — this may touch zero-filled absent pages (no SIGBUS)
            let mut heap: Vec<u8> = (0u8..=255).cycle().take(16 * 1024).collect();
            heap.iter_mut().for_each(|b| *b = b.wrapping_add(1));
            let sum: u64 = heap.iter().map(|&b| b as u64).sum();
            assert!(sum > 0);

            stream.write_all(b"OK").unwrap();
            println!("OK");
        }
    }
}
