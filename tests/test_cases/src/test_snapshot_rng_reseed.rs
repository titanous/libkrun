//! Integration test for RNG reseed behavior after snapshot/restore (clone divergence).
//!
//! GREEN phase: This test validates that VMGENID triggers kernel CSPRNG reseed,
//! causing both VM clones to produce different bytes from /dev/urandom. The kernel
//! detects a new VMGENID value via platform interrupt (GED on x86_64, SPI on aarch64)
//! and automatically reseeds the CSPRNG. This test demonstrates the outcome: entropy
//! divergence post-restore, ensuring cryptographic independence between clones.
//!
//! Design: the vsock listener is bound BEFORE the VM starts so the guest connects
//! immediately on boot (no retry loop). The snapshot is taken after the guest sends
//! "READY" (connection established, guest idle in command loop). The same vsock
//! proxy/stream persists across both restore cycles — the muxer proxy_map is NOT
//! reset on restore, so the host can communicate with the guest using the same
//! Unix stream after each restore.

use macros::{guest, host};

pub struct TestSnapshotRngReseed;

const VSOCK_PORT: u32 = 5688;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestSnapshotRngReseed {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("snap_rng_control.sock");
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            // Bind the listener BEFORE starting the VM so the guest can
            // connect on first boot without any retry/error path. The vsock
            // muxer only registers the proxy fd with epoll (needed for data
            // flow and HANG_UP detection) when the Unix connect() succeeds; if
            // no listener exists the connect fails and the proxy is in a broken
            // state that can't be recovered across restore cycles.
            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_vsock_port(VSOCK_PORT, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();
            let vm_thread = thread::spawn(move || context.run());

            // Accept the initial connection — the guest connects on first boot.
            // UnixListener::accept() blocks until the guest binary starts and
            // calls vsock_connect(). Kernel boots in ~83ms; add generous margin.
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(15)))
                .unwrap();

            // Read "READY" — the guest sends this immediately after connecting.
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"READY");

            // Snapshot while the guest is idle in the command loop with the
            // connection already established. The vsock proxy persists across
            // restores (muxer proxy_map is not reset), so we can use the same
            // stream after each restore_snapshot() call.
            handle.snapshot(&snap_dir)?;

            // --- Restore cycle 1 ---
            // Guest resets to snapshot state: in the command loop, waiting for
            // read_exact. VMGENID triggers kernel CSPRNG reseed via platform
            // interrupt. The muxer proxy and host stream are still live.
            handle.restore_snapshot(&snap_dir)?;

            stream.write_all(b"READ").unwrap();
            let mut first_entropy = [0u8; 32];
            stream.read_exact(&mut first_entropy).unwrap();

            // --- Restore cycle 2 ---
            // Guest resets to snapshot state again. VMGENID changes, triggering
            // another kernel CSPRNG reseed. Same proxy/stream still valid.
            handle.restore_snapshot(&snap_dir)?;

            stream.write_all(b"READ").unwrap();
            let mut second_entropy = [0u8; 32];
            stream.read_exact(&mut second_entropy).unwrap();

            // Assert BEFORE sending DONE.
            // This verifies that VMGENID caused entropy divergence between clones.
            assert_ne!(
                first_entropy, second_entropy,
                "Both VM clones produced identical /dev/urandom output — \
                 VMGENID-triggered CSPRNG reseed is missing or not working"
            );

            // Signal guest to exit cleanly.
            stream.write_all(b"DONE").unwrap();

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
    use std::fs::File;
    use std::io::{Read, Write};

    impl Test for TestSnapshotRngReseed {
        fn in_guest(self: Box<Self>) {
            // The listener is pre-bound on the host, so this connects on first
            // try. vsock_connect() internally retries on failure, but with the
            // listener already present the first attempt succeeds.
            let mut stream = vsock_connect(VSOCK_PORT);
            stream.write_all(b"READY").unwrap();

            loop {
                let mut cmd = [0u8; 4];
                // EOF or connection reset (e.g. stream dropped by host) → exit.
                match stream.read_exact(&mut cmd) {
                    Err(_) => break,
                    Ok(()) => {}
                }
                match &cmd {
                    b"READ" => {
                        let mut entropy = [0u8; 32];
                        File::open("/dev/urandom")
                            .unwrap()
                            .read_exact(&mut entropy)
                            .unwrap();
                        stream.write_all(&entropy).unwrap();
                    }
                    b"DONE" => {
                        println!("OK");
                        break;
                    }
                    other => panic!("unknown command from host: {:?}", other),
                }
            }
        }
    }
}
