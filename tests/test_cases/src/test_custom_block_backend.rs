//! Integration test for custom AsyncBlockBackend.
//!
//! Tests:
//! - AC7.5: Guest reads pre-filled data from custom block backend
//! - AC7.6: Guest writes data to custom block backend; host verifies writes after VM exits

use macros::{guest, host};

pub struct TestCustomBlockBackend;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::mem_block_backend::{MemBlockBackend, MemBlockBackendFactory};
    use crate::{Test, TestSetup};
    use std::thread;

    impl Test for TestCustomBlockBackend {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            const SECTOR_COUNT: u64 = 64; // 64 * 512 = 32 KiB
            const FILL_BYTE: u8 = 0x5A;

            let (backend, data_handle) = MemBlockBackend::new(SECTOR_COUNT, FILL_BYTE);
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
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_block_cfg(block_cfg);

            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());

            // Wait for guest to finish (it prints "OK" then exits)
            vm_thread.join().ok();

            // AC7.6: verify guest wrote 0xAB to sector 1 (bytes 512..1024)
            let data = data_handle.blocking_lock();
            let sector1 = &data[512..1024];
            assert!(
                sector1.iter().all(|&b| b == 0xAB),
                "Expected sector 1 to be all 0xAB after guest write"
            );

            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    impl Test for TestCustomBlockBackend {
        fn in_guest(self: Box<Self>) {
            // AC7.5: The custom block device appears as /dev/vda in the guest
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/vda")
                .expect("Failed to open /dev/vda");

            // AC7.5: read sector 0 and verify it's 0x5A (pre-filled by backend)
            let mut sector0 = vec![0u8; 512];
            f.read_exact(&mut sector0)
                .expect("Failed to read sector 0");
            assert!(
                sector0.iter().all(|&b| b == 0x5A),
                "Expected sector 0 to be all 0x5A"
            );

            // AC7.6: write 0xAB to sector 1
            f.seek(SeekFrom::Start(512))
                .expect("Failed to seek to sector 1");
            let sector1_data = vec![0xABu8; 512];
            f.write_all(&sector1_data)
                .expect("Failed to write sector 1");
            f.flush().expect("Failed to flush");

            println!("OK");
        }
    }
}
