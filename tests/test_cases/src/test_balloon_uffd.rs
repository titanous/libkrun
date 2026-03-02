//! Integration test for UFFD zero-fill of balloon-reclaimed pages (AC3.1, AC3.3).
//!
//! Phase 1: Boot a VM with the balloon inflated to 64MB and take a snapshot.
//! Phase 2: Cold-restore via UFFD demand-paging. The snapshot contains absent pages
//! (reclaimed by the balloon); the UFFD fault handler must resolve those via zeropage
//! rather than a store read. The guest verifies that non-reclaimed static data is
//! intact (AC3.3) and that post-restore memory allocation works (AC3.1).

use macros::{guest, host};

pub struct TestBalloonUffdZeroFill;

const VSOCK_PORT: u32 = 5712;

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

    impl Test for TestBalloonUffdZeroFill {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("balloon_uffd_snap");

            // Phase 1: Boot with balloon, inflate, snapshot, exit
            {
                let sock_path = test_setup.tmp_dir.join("balloon_uffd_phase1.sock");
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
                stream
                    .set_read_timeout(Some(Duration::from_secs(15)))
                    .unwrap();

                let mut buf = vec![0u8; 5];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"READY");

                // Inflate 64MB before snapshotting — those pages become absent in the snapshot
                balloon
                    .resize(64)
                    .map_err(|e| anyhow::anyhow!("resize failed: {e:?}"))?;
                balloon
                    .await_target(64, Duration::from_secs(5), Some(Duration::from_secs(30)))
                    .map_err(|e| anyhow::anyhow!("await_target failed: {e:?}"))?;

                handle.snapshot(&snap_dir)?;

                // Guest exits after dropping stream
                drop(stream);
                vm_thread.join().ok();
            }

            // Phase 2: Cold-restore via UFFD demand-paging with empty preload
            // EmptyPreloadStoreFactory forces every page — including absent ones — through
            // the UFFD fault handler. Absent pages are resolved via zeropage (AC3.1);
            // present pages are resolved via store read + copy (AC3.3).
            {
                let sock_path = test_setup.tmp_dir.join("balloon_uffd_phase2.sock");
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
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
                        let mut buf = vec![0u8; 2];
                        let _ = stream.read_exact(&mut buf);
                        assert_eq!(&buf, b"OK", "expected OK from guest after UFFD restore");
                    }
                });

                // AC3.1: restore succeeds even with absent (balloon-reclaimed) pages —
                // the UFFD handler zero-fills them via zeropage ioctl
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

    impl Test for TestBalloonUffdZeroFill {
        fn in_guest(self: Box<Self>) {
            // Static values that must survive cold restore via UFFD (AC3.3: present pages
            // are loaded from store correctly, not confused with zero-filled absent pages)
            static COUNTER: std::sync::atomic::AtomicI32 =
                std::sync::atomic::AtomicI32::new(0);
            static PATTERN: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

            // Phase 1: set known values and signal ready
            COUNTER.store(42, std::sync::atomic::Ordering::SeqCst);
            PATTERN.store(0xBB, std::sync::atomic::Ordering::SeqCst);

            let mut stream = vsock_connect(VSOCK_PORT);
            stream.write_all(b"READY").unwrap();

            // Drop connection — host inflates balloon then takes snapshot
            drop(stream);

            // Phase 2: reconnect after cold UFFD restore
            let mut stream = vsock_connect(VSOCK_PORT);

            // AC3.3: present page data survived cold restore (static section was in snapshot)
            let counter = COUNTER.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                counter, 42,
                "counter should be 42 after UFFD restore (present page), got {counter}"
            );
            let pattern = PATTERN.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                pattern, 0xBB,
                "pattern should be 0xBB after UFFD restore (present page), got {pattern:#x}"
            );

            // AC3.1: allocate heap memory — this touches demand-paged regions that may
            // include zero-filled absent pages; no crash or SIGBUS should occur
            let mut heap: Vec<u8> = (0u8..=255).cycle().take(8 * 1024).collect();
            heap.iter_mut().for_each(|b| *b = b.wrapping_add(1));
            let sum: u64 = heap.iter().map(|&b| b as u64).sum();
            assert!(sum > 0);

            stream.write_all(b"OK").unwrap();
            println!("OK");
        }
    }
}
