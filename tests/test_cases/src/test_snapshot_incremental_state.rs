//! Integration test for incremental snapshot preserving guest state.
//!
//! Tests AC6.3: Incremental snapshot/restore preserves guest state after workload

use macros::{guest, host};

pub struct TestSnapshotIncrementalState;

const VSOCK_PORT: u32 = 5682;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::mem_block_backend::{MemBlockBackend, MemBlockBackendFactory};
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    use std::thread;

    impl Test for TestSnapshotIncrementalState {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            const SECTOR_COUNT: u64 = 64;
            const FILL_BYTE: u8 = 0xFF;

            let sock_path = test_setup.tmp_dir.join("snap_incr_state_control.sock");
            let full_snap_dir = test_setup.tmp_dir.join("full_snapshot");
            let incr_snap_dir = test_setup.tmp_dir.join("incr_snapshot");

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

            let vm_thread = thread::spawn(move || context.run());

            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Phase 1: Wait for guest to signal READY (initial baseline)
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"READY");

            // Take full snapshot (baseline state)
            handle.snapshot(&full_snap_dir)?;

            // Enable dirty tracking for subsequent changes
            handle.enable_dirty_tracking()?;

            // Phase 2: Signal guest to perform workload
            stream.write_all(b"WORKLOAD").unwrap();

            // Wait for guest to signal WORKLOAD_DONE
            let mut buf = vec![0u8; 13];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"WORKLOAD_DONE");

            // Take incremental snapshot (only dirty pages)
            handle.incremental_snapshot(&incr_snap_dir)?;

            // Restore from incremental snapshot
            handle.restore_incremental_snapshot(&incr_snap_dir)?;

            // Phase 3: Signal guest to verify state
            stream.write_all(b"VERIFY").unwrap();

            // Guest verifies and prints "OK"
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

    const TEST_PATTERN: &[u8] = b"WORKLOAD_TEST_PATTERN";

    impl Test for TestSnapshotIncrementalState {
        fn in_guest(self: Box<Self>) {
            // Use static variables to track state across snapshot/restore
            static COUNTER: std::sync::atomic::AtomicI32 =
                std::sync::atomic::AtomicI32::new(0);
            static WORKLOAD_DONE: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);

            let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
                .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Phase 1: Signal ready (baseline state captured at this point)
            stream.write_all(b"READY").unwrap();

            // Wait for WORKLOAD signal
            let mut buf = vec![0u8; 8];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"WORKLOAD");

            // Phase 2: Perform workload
            // Write to block device
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/vda")
                .expect("Failed to open /dev/vda");

            let mut sector_data = vec![0u8; 512];
            sector_data[..TEST_PATTERN.len()].copy_from_slice(TEST_PATTERN);
            f.seek(SeekFrom::Start(0))
                .expect("Failed to seek to sector 0");
            f.write_all(&sector_data)
                .expect("Failed to write sector 0");
            f.flush().expect("Failed to flush");

            // Update static variable to track state
            COUNTER.store(99, std::sync::atomic::Ordering::SeqCst);
            WORKLOAD_DONE.store(true, std::sync::atomic::Ordering::SeqCst);

            // Signal that workload is complete
            stream.write_all(b"WORKLOAD_DONE").unwrap();

            // Wait for VERIFY signal (after incremental restore)
            let mut buf = vec![0u8; 6];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"VERIFY");

            // Phase 3: Verify state after incremental restore
            // Check static variables
            let counter = COUNTER.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                counter, 99,
                "counter was {counter} after incremental restore, expected 99"
            );

            assert!(
                WORKLOAD_DONE.load(std::sync::atomic::Ordering::SeqCst),
                "workload_done flag not set after restore"
            );

            // Verify block device data persisted
            f.seek(SeekFrom::Start(0))
                .expect("Failed to seek back to sector 0");
            let mut read_sector = vec![0u8; 512];
            f.read_exact(&mut read_sector)
                .expect("Failed to read sector 0");

            assert_eq!(
                &read_sector[..TEST_PATTERN.len()],
                TEST_PATTERN,
                "block device data does not match after incremental restore"
            );

            println!("OK");
        }
    }
}
