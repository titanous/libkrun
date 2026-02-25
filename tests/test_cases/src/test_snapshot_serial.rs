//! Integration test for guest state preservation across snapshot/restore.
//!
//! Tests AC6.1: Guest state (memory and static variables) survives snapshot/restore

use macros::{guest, host};

pub struct TestSnapshotSerial;

const VSOCK_PORT: u32 = 5680;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    use std::thread;

    impl Test for TestSnapshotSerial {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("snap_serial_control.sock");
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

            // Wait for guest to signal READY
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"READY");

            // Take full snapshot
            handle.snapshot(&snap_dir)?;

            // Hot-restore the snapshot (resets VM state back to the snapshot point)
            handle.restore_snapshot(&snap_dir)?;

            // Signal guest to verify state
            stream.write_all(b"CHECK").unwrap();

            // Guest verifies static variables survived and prints "OK", then exits
            vm_thread.join().ok();
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

    const SCRATCH_VALUE: u8 = 0x42;

    impl Test for TestSnapshotSerial {
        fn in_guest(self: Box<Self>) {
            // Use static variables to track state across snapshot/restore
            static WRITTEN: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            static SCRATCH_VALUE_SAVED: std::sync::atomic::AtomicU8 =
                std::sync::atomic::AtomicU8::new(0);

            let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
                .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Save a test value to static variables that should survive snapshot/restore
            WRITTEN.store(true, std::sync::atomic::Ordering::SeqCst);
            SCRATCH_VALUE_SAVED.store(SCRATCH_VALUE, std::sync::atomic::Ordering::SeqCst);

            // Signal host we are ready
            stream.write_all(b"READY").unwrap();

            // Wait for host to signal CHECK (after snapshot+restore)
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"CHECK");

            // After restore, verify that static variables survived
            assert!(
                WRITTEN.load(std::sync::atomic::Ordering::SeqCst),
                "write flag not set after restore"
            );

            // Also verify the saved value persisted
            let saved_val = SCRATCH_VALUE_SAVED.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                saved_val, SCRATCH_VALUE,
                "saved value was {saved_val:#x} after restore, expected {SCRATCH_VALUE:#x}"
            );

            println!("OK");
        }
    }
}
