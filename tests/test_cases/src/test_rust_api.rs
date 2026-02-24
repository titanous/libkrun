//! Test suite for the Rust API: Builder/Context/VmHandle lifecycle.
//!
//! Tests:
//! - AC7.1: Builder validation rejects 0 vCPUs before VM starts
//! - AC7.2: device_info() reflects configured vCPU count and RAM size
//! - AC7.3: pause() and resume() work on a running VM
//! - AC7.4: trigger_shutdown_event() returns Err on Linux (unsupported)

use macros::{guest, host};

pub struct TestRustApiZeroVcpu;
pub struct TestRustApiDeviceInfo;
pub struct TestRustApiPauseResume;
pub struct TestRustApiShutdown;

const VSOCK_PORT_API: u32 = 5680;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    use std::thread;

    impl Test for TestRustApiZeroVcpu {
        fn start_vm(self: Box<Self>, _test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();
            // AC7.1: vm_config(0, 256) should return Err, not start a VM
            let result = builder.vm_config(0, 256);
            assert!(
                result.is_err(),
                "Expected error for 0 vCPUs, but vm_config() succeeded"
            );
            println!("OK");
            Ok(())
        }
    }

    impl Test for TestRustApiDeviceInfo {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();
            builder.vm_config(2, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;

            // AC7.2: device_info() should reflect configured values
            let info = context.device_info();
            assert_eq!(info.vcpu_count, 2, "vcpu_count mismatch");

            // RAM may not be exactly 512 MiB due to alignment; allow ±10%
            let ram = info.ram_mib;
            assert!(
                ram >= 460 && ram <= 565,
                "ram_mib {ram} not within 10% of 512"
            );

            // Don't run the VM — just check device_info
            println!("OK");
            Ok(())
        }
    }

    impl Test for TestRustApiPauseResume {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("api_control.sock");
            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 256)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_vsock_port(VSOCK_PORT_API, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();

            let vm_thread = thread::spawn(move || context.run());

            // AC7.3: Wait for guest READY signal
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"READY");

            // Pause and resume
            handle.pause()?;
            std::thread::sleep(Duration::from_millis(100));
            handle.resume()?;

            // Signal guest to print OK
            stream.write_all(b"CONT!").unwrap();

            vm_thread.join().ok();
            Ok(())
        }
    }

    impl Test for TestRustApiShutdown {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();
            builder.vm_config(1, 256)?;
            setup_fs_builder(&mut builder, &test_setup)?;

            let context = builder.build()?;
            let handle = context.vm_handle();

            let _vm_thread = thread::spawn(move || context.run());

            // AC7.4: On Linux x86, trigger_shutdown_event() is unsupported
            // On aarch64/macOS, it causes the VM to exit cleanly
            let result = handle.trigger_shutdown_event();
            #[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
            {
                // On Linux: verify the function returns a meaningful Err (not panic)
                assert!(
                    result.is_err(),
                    "Expected Err on non-aarch64-mac, got Ok"
                );
                // Verify the error message indicates unsupported (not a crash/panic)
                let msg = result.unwrap_err().to_string();
                assert!(
                    msg.contains("unavailable") || msg.contains("Unsupported") || msg.contains("not supported"),
                    "Unexpected error: {msg}"
                );
                println!("OK");
            }
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                // On aarch64/macOS: should succeed and VM exits cleanly
                result?;
                println!("OK");
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
    use std::time::Duration;

    impl Test for TestRustApiZeroVcpu {
        fn in_guest(self: Box<Self>) {
            // Never runs — host test doesn't start a VM
        }
    }

    impl Test for TestRustApiDeviceInfo {
        fn in_guest(self: Box<Self>) {
            // Never runs — host test doesn't start a VM
        }
    }

    impl Test for TestRustApiPauseResume {
        fn in_guest(self: Box<Self>) {
            let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
                .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT_API);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = std::os::unix::net::UnixStream::from(sock);
            stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(15))).unwrap();

            // AC7.3: Send READY signal
            stream.write_all(b"READY").unwrap();

            // Wait for CONT! signal — host pauses and resumes between READY and CONT!
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"CONT!");

            println!("OK");
        }
    }

    impl Test for TestRustApiShutdown {
        fn in_guest(self: Box<Self>) {
            // On aarch64/macOS: would exit when shutdown event fires
            // On Linux: host prints OK directly (never reaches guest)
        }
    }
}
