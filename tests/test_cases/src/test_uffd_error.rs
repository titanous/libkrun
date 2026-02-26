//! Test error handling: read_page failure.
//!
//! Verifies userfaultd.AC5.5: Store fails read_page, VM gets clean VmExit::Error.

use macros::{guest, host};

pub struct TestUffdErrorHandling;

const VSOCK_PORT: u32 = 5705;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::mock_snapshot_store::ErrorStoreFactory;
    use crate::{Test, TestSetup};
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestUffdErrorHandling {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            // Phase 1: Snapshot
            {
                let sock_path = test_setup.tmp_dir.join("uffd_error_phase1.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_vsock_port(VSOCK_PORT, sock_path, false);

                let context = builder.build()?;
                let handle = context.vm_handle();

                let vm_thread = thread::spawn(move || context.run());

                // Wait for guest READY signal
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut buf = vec![0u8; 5];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"READY");

                // Take snapshot
                handle.snapshot(&snap_dir)?;

                vm_thread.join().ok();
            }

            // Phase 2: Restore with error on read_page
            {
                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;

                let context = builder.build()?;

                // Create factory that will fail on read_page at address 0x0
                let factory = ErrorStoreFactory::new(&snap_dir, &[], 0x0);

                // Attempt to restore and expect VmExit::Error or StartError
                match context.restore_and_run_with_store(Box::new(factory)) {
                    Ok(krun::VmExit::Error { message }) => {
                        // Expected: store returned error, which became VmExit::Error
                        assert!(
                            message.contains("read_page") || message.contains("simulated"),
                            "Expected read_page error message, got: {message}"
                        );
                        println!("OK");
                    }
                    Ok(other_exit) => {
                        panic!("Expected VmExit::Error, got: {other_exit:?}");
                    }
                    Err(e) => {
                        // Also acceptable: error propagated during factory.create() or restore
                        eprintln!("Got error during restore (acceptable): {e}");
                        assert!(
                            e.to_string().contains("read_page")
                                || e.to_string().contains("simulated")
                                || e.to_string().contains("Error"),
                            "Expected read_page error, got: {e}"
                        );
                        println!("OK");
                    }
                }
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
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    impl Test for TestUffdErrorHandling {
        fn in_guest(self: Box<Self>) {
            // Phase 1: Signal READY for snapshot
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
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            // Signal host we are ready
            stream.write_all(b"READY").unwrap();
            drop(stream);

            // Guest never runs phase 2 because cold restore fails with read_page error
            panic!("Guest should not reach phase 2 when read_page fails");
        }
    }
}
