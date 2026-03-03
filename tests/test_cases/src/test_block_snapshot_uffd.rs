//! Integration test: block write → snapshot → UFFD cold restore (AC3.5).
//!
//! Phase 1: Boot VM with block device, guest writes pattern to sector 0,
//!          host snapshots, VM exits.
//! Phase 2: Cold UFFD restore with new block device connected via fresh factory.
//!          Guest verifies the written pattern is still available.

use macros::{guest, host};

pub struct TestBlockSnapshotUffd;

const VSOCK_PORT: u32 = 5722;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::mem_block_backend::{MemBlockBackend, MemBlockBackendFactory};
    use crate::mock_snapshot_store::EmptyPreloadStoreFactory;
    use crate::{Test, TestSetup};
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    const SECTOR_COUNT: u64 = 32;
    const FILL_BYTE: u8 = 0x00;

    impl Test for TestBlockSnapshotUffd {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("block_uffd_snap");
            // Shared data handle persists across phases so the backend data
            // is available to both the pre-snapshot VM and the restored VM.
            let data_handle: Arc<tokio::sync::Mutex<Vec<u8>>>;

            // Phase 1: Boot, guest writes, host snapshots, VM exits
            {
                let sock_path = test_setup.tmp_dir.join("bsu_block_phase1.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                let (backend, handle) = MemBlockBackend::new(SECTOR_COUNT, FILL_BYTE);
                data_handle = handle;
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

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_block_cfg(block_cfg);
                builder.add_vsock_port(VSOCK_PORT, sock_path, false);

                let context = builder.build()?;
                let handle = context.vm_handle();

                let vm_thread = thread::spawn(move || context.run());

                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();

                // Wait for guest to write pattern to block device
                let mut buf = vec![0u8; 7];
                stream.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"WRITTEN");

                // Snapshot the VM state (block driver state + guest RAM)
                handle.snapshot(&snap_dir)?;

                // VM exits (guest drops connection after WRITTEN)
                drop(stream);
                vm_thread.join().ok();
            }

            // Verify the backend data from phase 1 contains the written pattern
            {
                let data = data_handle.blocking_lock();
                assert_eq!(
                    &data[..8],
                    b"BLOCKWRT",
                    "block backend should contain written pattern after phase 1"
                );
            }

            // Phase 2: Cold UFFD restore — block device re-connected via new factory
            // The restored guest RAM contains the block driver's internal state;
            // the backend data handle is shared (same Arc) so written data persists.
            {
                let sock_path = test_setup.tmp_dir.join("bsu_block_phase2.sock");
                let listener = UnixListener::bind(&sock_path).unwrap();

                // Create a fresh backend with the same data (simulate persistent storage)
                let data_snapshot = data_handle.blocking_lock().clone();
                let preserved_data = Arc::new(tokio::sync::Mutex::new(data_snapshot));
                let backend = MemBlockBackend::from_data(preserved_data.clone());
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

                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;
                builder.add_block_cfg(block_cfg);
                builder.add_vsock_port(VSOCK_PORT, sock_path, false);

                let context = builder.build()?;

                let store_factory =
                    EmptyPreloadStoreFactory::new(&snap_dir, &[] as &[&std::path::Path]);

                let listener_thread = thread::spawn(move || {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
                        let mut buf = vec![0u8; 8];
                        let _ = stream.read_exact(&mut buf);
                        assert_eq!(&buf, b"VERIFIED", "expected VERIFIED from guest");
                    }
                });

                context.restore_and_run_with_store(Box::new(store_factory))?;

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
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    const WRITE_PATTERN: &[u8] = b"BLOCKWRT";

    impl Test for TestBlockSnapshotUffd {
        fn in_guest(self: Box<Self>) {
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/vda")
                .expect("Failed to open /dev/vda");

            // Phase 1: write known pattern to sector 0
            let mut sector = vec![0u8; 512];
            sector[..WRITE_PATTERN.len()].copy_from_slice(WRITE_PATTERN);
            f.seek(SeekFrom::Start(0)).expect("seek to sector 0");
            f.write_all(&sector).expect("write sector 0");
            f.flush().expect("flush");

            let mut stream = vsock_connect(VSOCK_PORT);
            stream.write_all(b"WRITTEN").unwrap();

            // Drop connection — host snapshots, VM exits
            drop(stream);
            drop(f);

            // Phase 2: reconnect after UFFD cold restore
            let mut f = OpenOptions::new()
                .read(true)
                .open("/dev/vda")
                .expect("Failed to reopen /dev/vda after restore");

            let mut stream = vsock_connect(VSOCK_PORT);

            // Read back sector 0 and verify pattern (data persisted in backend)
            f.seek(SeekFrom::Start(0)).expect("seek to sector 0");
            let mut read_back = vec![0u8; 512];
            f.read_exact(&mut read_back).expect("read sector 0 after restore");

            assert_eq!(
                &read_back[..WRITE_PATTERN.len()],
                WRITE_PATTERN,
                "block data should match written pattern after UFFD restore"
            );

            stream.write_all(b"VERIFIED").unwrap();
            println!("OK");
        }
    }
}
