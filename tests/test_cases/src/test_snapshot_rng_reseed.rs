//! Integration test for RNG reseed behavior after snapshot/restore (clone divergence).
//!
//! Validates that VMGENID triggers kernel CSPRNG reseed, causing both VM clones
//! to produce different bytes from /dev/urandom. The kernel detects a new VMGENID
//! value via platform interrupt (PIC IRQ via irqfd on x86_64, SPI on aarch64) and automatically
//! reseeds the CSPRNG.
//!
//! Design: vsock is used only for host→guest commands (READ/DONE). The guest
//! writes entropy to a virtiofs file (/entropy.bin) which the host reads directly
//! from the shared rootfs. This avoids guest→host vsock writes after restore,
//! which are unreliable because the virtio TX ring state becomes inconsistent
//! between the restored guest and the live host-side muxer across multiple
//! restore cycles.

use macros::{guest, host};

pub struct TestSnapshotRngReseed;

const VSOCK_PORT: u32 = 5688;
const ENTROPY_FILE: &str = "entropy.bin";

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    /// Poll for a file to appear and have the expected size.
    fn wait_for_file(path: &Path, expected_len: usize, timeout: Duration) -> Vec<u8> {
        let start = Instant::now();
        loop {
            if let Ok(data) = std::fs::read(path) {
                if data.len() == expected_len {
                    // Remove so the next restore cycle starts clean.
                    std::fs::remove_file(path).ok();
                    return data;
                }
            }
            assert!(
                start.elapsed() < timeout,
                "timed out waiting for {path:?} ({expected_len} bytes)",
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    impl Test for TestSnapshotRngReseed {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("snap_rng_control.sock");
            let snap_dir = test_setup.tmp_dir.join("snapshot");
            let entropy_path = test_setup.tmp_dir.join("root").join(ENTROPY_FILE);

            // Bind the listener BEFORE starting the VM so the guest can
            // connect on first boot without any retry/error path.
            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_vsock_port(VSOCK_PORT, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();
            let vm_thread = thread::spawn(move || context.run());

            // Accept the initial connection.
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

            // Snapshot while the guest is idle in the command loop.
            handle.snapshot(&snap_dir)?;

            // Clean up any stale entropy file before restore cycles.
            std::fs::remove_file(&entropy_path).ok();

            // --- Restore cycle 1 ---
            handle.restore_snapshot(&snap_dir)?;
            stream.write_all(b"READ").unwrap();
            let first_entropy = wait_for_file(&entropy_path, 32, Duration::from_secs(10));

            // --- Restore cycle 2 ---
            handle.restore_snapshot(&snap_dir)?;
            stream.write_all(b"READ").unwrap();
            let second_entropy = wait_for_file(&entropy_path, 32, Duration::from_secs(10));

            // Verify that VMGENID caused entropy divergence between clones.
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
            let mut stream = vsock_connect(VSOCK_PORT);
            stream.write_all(b"READY").unwrap();

            loop {
                let mut cmd = [0u8; 4];
                // EOF or connection reset → exit.
                if stream.read_exact(&mut cmd).is_err() {
                    break;
                }
                match &cmd {
                    b"READ" => {
                        let mut entropy = [0u8; 32];
                        File::open("/dev/urandom")
                            .unwrap()
                            .read_exact(&mut entropy)
                            .unwrap();
                        // Write entropy to virtiofs file (host reads directly).
                        // Avoids guest→host vsock writes which break after
                        // multiple restore cycles.
                        std::fs::write(format!("/{ENTROPY_FILE}"), &entropy).unwrap();
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
