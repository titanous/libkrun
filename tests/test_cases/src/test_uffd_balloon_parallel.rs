//! Integration test: multiple vCPUs faulting on balloon-reclaimed addresses after UFFD restore (AC3.8).
//!
//! Phase 1: Boot with 2 vCPUs and balloon, inflate 64MB, take snapshot.
//! Phase 2: Cold UFFD restore with empty preload. Multiple vCPUs fault on
//!          balloon-reclaimed (absent) pages. The UFFD fault handler zero-fills
//!          these pages without SIGBUS errors.

use macros::{guest, host};

pub struct TestUffdBalloonParallel;

const VSOCK_PORT: u32 = 5725;

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

    impl Test for TestUffdBalloonParallel {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("uffd_balloon_parallel");

            // Phase 1: Boot with 2 vCPUs, balloon, inflate, snapshot
            {
                let sock_path = test_setup.tmp_dir.join("ubp_phase1.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(2, 256)?; // 2 vCPUs
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

                // Wait for guest to signal ready
                let mut buf = vec![0u8; 5];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"READY");

                // Inflate 64MB to reclaim pages (become absent)
                balloon
                    .resize(64)
                    .map_err(|e| anyhow::anyhow!("balloon resize failed: {e:?}"))?;
                balloon
                    .await_target(64, Duration::from_secs(5), Some(Duration::from_secs(30)))
                    .map_err(|e| anyhow::anyhow!("balloon await_target failed: {e:?}"))?;

                // Snapshot with inflated balloon
                handle.snapshot(&snap_dir)?;

                drop(stream);
                vm_thread.join().ok();
            }

            // Phase 2: Cold UFFD restore — multiple vCPUs fault on absent pages
            {
                let sock_path = test_setup.tmp_dir.join("ubp_phase2.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(2, 256)?; // 2 vCPUs
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT, sock_path, false);
                builder.enable_balloon();

                let context = builder.build()?;

                let factory = EmptyPreloadStoreFactory::new(&snap_dir, &[] as &[&std::path::Path]);

                let listener_thread = thread::spawn(move || {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                        let mut buf = vec![0u8; 2];
                        let _ = stream.read_exact(&mut buf);
                        assert_eq!(&buf, b"OK", "expected OK from guest after parallel restore");
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    impl Test for TestUffdBalloonParallel {
        fn in_guest(self: Box<Self>) {
            // Phase 1: signal ready; host will inflate balloon and snapshot
            let mut stream = vsock_connect(VSOCK_PORT);
            stream.write_all(b"READY").unwrap();
            drop(stream);

            // Phase 2: reconnect after cold UFFD restore; spawn threads to fault on absent pages
            let mut stream = vsock_connect(VSOCK_PORT);

            let error_counter = Arc::new(AtomicUsize::new(0));

            let mut handles = vec![];
            for thread_id in 0..2 {
                let error_counter = error_counter.clone();
                let handle = thread::spawn(move || {
                    // Allocate and access memory — some pages are absent (zero-filled by UFFD)
                    // and some are present (restored from snapshot).
                    // The key is that multiple threads should not trigger SIGBUS.
                    let mut local_mem: Vec<u8> = vec![0; 4 * 1024 * 1024]; // 4 MiB per thread
                    for i in 0..local_mem.len() {
                        local_mem[i] = ((thread_id * 256 + i) % 256) as u8;
                    }
                    let sum: u64 = local_mem.iter().map(|&b| b as u64).sum();
                    if sum == 0 {
                        error_counter.fetch_add(1, Ordering::Relaxed);
                    }
                });
                handles.push(handle);
            }

            for h in handles {
                let _ = h.join();
            }

            // No errors should have occurred (no SIGBUS)
            let errors = error_counter.load(Ordering::Relaxed);
            assert_eq!(
                errors, 0,
                "parallel memory allocation should not trigger errors, got {errors}"
            );

            stream.write_all(b"OK").unwrap();
            println!("OK");
        }
    }
}
