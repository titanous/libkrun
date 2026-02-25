//! Integration test for serial scratch register snapshot/restore.
//!
//! Tests AC6.1: Guest reads serial scratch register value that was written before snapshot

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

            // Signal guest to verify
            stream.write_all(b"CHECK").unwrap();

            // Guest verifies and prints "OK", then exits
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

    // COM1 scratch register: port 0x3F8 + 7 = 0x3FF
    const SERIAL_PORT_BASE: u16 = 0x3F8;
    const SCRATCH_REG_OFFSET: u16 = 7;
    const SCRATCH_PORT: u16 = SERIAL_PORT_BASE + SCRATCH_REG_OFFSET;
    const SCRATCH_VALUE: u8 = 0x42;

    // SAFETY: Port I/O is only used in guest context (single-threaded)
    unsafe fn outb(port: u16, val: u8) {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") val,
            options(nostack, nomem)
        );
    }

    // SAFETY: Port I/O is only used in guest context (single-threaded)
    unsafe fn inb(port: u16) -> u8 {
        let val: u8;
        core::arch::asm!(
            "in al, dx",
            out("al") val,
            in("dx") port,
            options(nostack, nomem)
        );
        val
    }

    impl Test for TestSnapshotSerial {
        fn in_guest(self: Box<Self>) {
            // Use static variables to track serial port I/O across snapshot/restore
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

            // Write known value to COM1 scratch register
            unsafe {
                outb(SCRATCH_PORT, SCRATCH_VALUE);
            }
            // Save the value we wrote to static variables so they survive across snapshot/restore
            WRITTEN.store(true, std::sync::atomic::Ordering::SeqCst);
            SCRATCH_VALUE_SAVED.store(SCRATCH_VALUE, std::sync::atomic::Ordering::SeqCst);

            // Signal host we have written the scratch register
            stream.write_all(b"READY").unwrap();

            // Wait for host to signal CHECK (after snapshot+restore)
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"CHECK");

            // After restore, verify that we previously wrote to the register
            assert!(
                WRITTEN.load(std::sync::atomic::Ordering::SeqCst),
                "scratch register write flag not set after restore"
            );

            // Also verify the saved value persisted
            let saved_val = SCRATCH_VALUE_SAVED.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                saved_val, SCRATCH_VALUE,
                "scratch register saved value was {saved_val:#x} after restore, expected {SCRATCH_VALUE:#x}"
            );

            // Attempt to read scratch register back if possible
            let val = unsafe { inb(SCRATCH_PORT) };
            assert_eq!(
                val, SCRATCH_VALUE,
                "scratch register was {val:#x} after restore, expected {SCRATCH_VALUE:#x}"
            );

            println!("OK");
        }
    }
}
