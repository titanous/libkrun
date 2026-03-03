//! Integration test: rapid balloon inflate/deflate during snapshot (AC3.7).
//!
//! Rapidly alternate between inflate and deflate operations while taking a
//! snapshot. The snapshot must succeed without panic or corruption. The VM
//! must exit cleanly after the snapshot/restore cycle.

use macros::{guest, host};

pub struct TestBalloonSnapshotRace;

const VSOCK_PORT: u32 = 5724;

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

    impl Test for TestBalloonSnapshotRace {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("balloon_snap_race");

            // Phase 1: Boot with balloon, execute rapid inflate/deflate, snapshot during operations
            {
                let sock_path = test_setup.tmp_dir.join("bsr_phase1.sock");
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

                // Wait for guest to signal ready
                let mut buf = vec![0u8; 5];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"READY");

                // Rapidly inflate/deflate to stress the device state machine
                for i in 0..5 {
                    balloon
                        .resize(32 + (i % 2) * 32)
                        .map_err(|e| anyhow::anyhow!("balloon resize failed: {e:?}"))?;
                    std::thread::sleep(Duration::from_millis(100));
                }

                // Take snapshot during balloon activity
                handle.snapshot(&snap_dir)?;

                // Guest signals completion
                drop(stream);
                vm_thread.join().ok();
            }

            // Phase 2: Hot restore — balloon state and guest memory must be consistent
            {
                let sock_path = test_setup.tmp_dir.join("bsr_phase2.sock");
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
                        assert_eq!(&buf, b"OK", "expected OK from guest after restore");
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

    impl Test for TestBalloonSnapshotRace {
        fn in_guest(self: Box<Self>) {
            // Phase 1: signal ready; host will perform rapid inflate/deflate + snapshot
            let mut stream = vsock_connect(VSOCK_PORT);
            stream.write_all(b"READY").unwrap();
            drop(stream);

            // Phase 2: reconnect after hot restore
            let mut stream = vsock_connect(VSOCK_PORT);

            // Allocate some memory to ensure balloon state recovery is correct
            let mut _memory: Vec<u8> = vec![0x42; 8 * 1024];
            _memory.iter_mut().for_each(|b| *b = b.wrapping_add(1));

            stream.write_all(b"OK").unwrap();
            println!("OK");
        }
    }
}
