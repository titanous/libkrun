use macros::{guest, host};

pub struct TestSnapshotRestore;

const VSOCK_PORT: u32 = 5678;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestSnapshotRestore {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("snap_control.sock");
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_vsock_port(VSOCK_PORT, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();

            // Spawn VM in background — it runs until the guest exits
            let vm_thread = thread::spawn(move || context.run());

            // Wait for guest to signal READY (guest has set counter to 42)
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"READY");

            // Take full snapshot
            handle.snapshot(&snap_dir)?;

            // Hot-restore the snapshot (resets VM state back to the snapshot point)
            handle.restore_snapshot(&snap_dir)?;

            // Signal guest to verify counter is still 42
            stream.write_all(b"CHECK").unwrap();

            // Guest verifies counter == 42, prints "OK", then exits
            vm_thread.join().ok();
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::vsock_helpers::vsock_connect;
    use crate::Test;
    use std::io::{Read, Write};

    impl Test for TestSnapshotRestore {
        fn in_guest(self: Box<Self>) {
            // Use a static variable to survive snapshot/restore
            static COUNTER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

            // Set counter to 42 — this value must be preserved after restore
            COUNTER.store(42, std::sync::atomic::Ordering::SeqCst);

            let mut stream = vsock_connect(VSOCK_PORT);

            // Signal host we are ready
            stream.write_all(b"READY").unwrap();

            // Wait for host to signal CHECK (after snapshot+restore)
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"CHECK");

            // After restore, counter must still be 42
            let val = COUNTER.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(val, 42, "counter was {val} after restore, expected 42");

            println!("OK");
        }
    }
}

pub struct TestSnapshotRestoreIncremental;

const VSOCK_PORT_INCR: u32 = 5679;

#[host]
mod host_incr {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestSnapshotRestoreIncremental {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("snap_incr_control.sock");
            let full_snap_dir = test_setup.tmp_dir.join("full_snapshot");
            let incr_snap_dir = test_setup.tmp_dir.join("incr_snapshot");

            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_vsock_port(VSOCK_PORT_INCR, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();

            let vm_thread = thread::spawn(move || context.run());

            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            // Phase 1: guest writes initial data, signals READY
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"READY");

            // Take full snapshot and enable dirty tracking
            handle.snapshot(&full_snap_dir)?;
            handle.enable_dirty_tracking()?;

            // Signal guest to write to a specific memory region
            stream.write_all(b"WRITE").unwrap();

            // Phase 2: guest writes known pattern, signals WRITTEN
            let mut buf = vec![0u8; 7];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"WRITTEN");

            // Take incremental snapshot (only dirty pages)
            handle.incremental_snapshot(&incr_snap_dir)?;

            // Restore from incremental snapshot
            handle.restore_incremental_snapshot(&incr_snap_dir)?;

            // Signal guest to verify: data written after full snapshot must still be present
            stream.write_all(b"VERIFY").unwrap();

            // Guest verifies and prints "OK"
            vm_thread.join().ok();
            Ok(())
        }
    }
}

#[guest]
mod guest_incr {
    use super::*;
    use crate::vsock_helpers::vsock_connect;
    use crate::Test;
    use std::io::{Read, Write};

    // A fixed-size buffer in data segment — this memory will be tracked as dirty.
    // SAFETY: guest code is single-threaded, so no concurrent access is possible.
    static mut TEST_REGION: [u8; 64] = [0u8; 64];
    const EXPECTED_PATTERN: u8 = 0xAB;

    impl Test for TestSnapshotRestoreIncremental {
        fn in_guest(self: Box<Self>) {
            let mut stream = vsock_connect(VSOCK_PORT_INCR);

            // Phase 1: signal ready with initial region state (zeros)
            stream.write_all(b"READY").unwrap();

            // Wait for WRITE signal
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"WRITE");

            // Write known pattern into the region
            #[allow(static_mut_refs)]
            unsafe {
                TEST_REGION.fill(EXPECTED_PATTERN);
            }

            stream.write_all(b"WRITTEN").unwrap();

            // Wait for VERIFY signal (after incremental restore)
            let mut buf = vec![0u8; 6];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"VERIFY");

            // After incremental restore, the written region should still have our pattern
            #[allow(static_mut_refs)]
            let pattern = unsafe { TEST_REGION[0] };
            assert_eq!(
                pattern, EXPECTED_PATTERN,
                "region pattern was {pattern:#x} after incremental restore"
            );

            println!("OK");
        }
    }
}
