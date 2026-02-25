//! Integration test for block device data snapshot/restore.
//!
//! Tests AC6.2: Guest reads block device data that was written before snapshot

use macros::{guest, host};

pub struct TestSnapshotBlock;

const VSOCK_PORT: u32 = 5681;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::mem_block_backend::{MemBlockBackend, MemBlockBackendFactory};
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestSnapshotBlock {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            const SECTOR_COUNT: u64 = 64; // 64 * 512 = 32 KiB
            const FILL_BYTE: u8 = 0xFF;

            let sock_path = test_setup.tmp_dir.join("snap_block_control.sock");
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            let (backend, _data_handle) = MemBlockBackend::new(SECTOR_COUNT, FILL_BYTE);
            let factory = MemBlockBackendFactory::new(backend);

            let block_cfg = krun::BlockDeviceConfig {
                block_id: "test-block".to_string(),
                cache_type: krun::CacheType::Writeback,
                disk_type: krun::BlockDeviceType::CustomAsyncFactory {
                    factory: Box::new(factory),
                },
                is_disk_read_only: false,
                direct_io: false,
            };

            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_block_cfg(block_cfg);
            builder.add_vsock_port(VSOCK_PORT, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();

            // Spawn VM in background — it runs until the guest exits
            let vm_thread = thread::spawn(move || context.run());

            // Wait for guest to signal READY
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

            // Signal guest to verify block device data
            stream.write_all(b"CHECK").unwrap();

            // Guest verifies block device data and prints "OK", then exits
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
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    const TEST_PATTERN: &[u8] = b"SNAPSHOT_TEST_DATA";

    impl Test for TestSnapshotBlock {
        fn in_guest(self: Box<Self>) {
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
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            // Open block device and write known pattern to first sector
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/vda")
                .expect("Failed to open /dev/vda");

            // Write test pattern to first sector
            let mut sector_data = vec![0u8; 512];
            sector_data[..TEST_PATTERN.len()].copy_from_slice(TEST_PATTERN);
            f.seek(SeekFrom::Start(0))
                .expect("Failed to seek to sector 0");
            f.write_all(&sector_data).expect("Failed to write sector 0");
            f.flush().expect("Failed to flush");

            // Signal host we have written to block device
            stream.write_all(b"READY").unwrap();

            // Wait for host to signal CHECK (after snapshot+restore)
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"CHECK");

            // After restore, read back the first sector and verify pattern
            f.seek(SeekFrom::Start(0))
                .expect("Failed to seek back to sector 0");
            let mut read_sector = vec![0u8; 512];
            f.read_exact(&mut read_sector)
                .expect("Failed to read sector 0");

            // Verify the pattern matches
            assert_eq!(
                &read_sector[..TEST_PATTERN.len()],
                TEST_PATTERN,
                "block device data does not match after restore"
            );

            println!("OK");
        }
    }
}
