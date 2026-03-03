//! Integration test for FailingBlockBackend error handling.
//!
//! Verifies that a custom AsyncBlockBackend returning I/O errors on specific
//! sectors surfaces as guest I/O errors (EIO) that the guest can handle, and
//! that the VM exits cleanly after the guest handles the error.

use macros::{guest, host};

pub struct TestBlockBackendErrors;

#[host]
mod host {
    use super::*;
    use crate::failing_block_backend::{FailingBlockBackend, FailingBlockBackendFactory};
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::thread;

    const ERROR_SECTOR: u64 = 5;
    const SECTOR_COUNT: u64 = 16;
    const FILL_BYTE: u8 = 0xAA;

    impl Test for TestBlockBackendErrors {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let (backend, _data) =
                FailingBlockBackend::new(SECTOR_COUNT, FILL_BYTE, [ERROR_SECTOR]);
            let factory = FailingBlockBackendFactory::new(backend);

            let block_cfg = krun::BlockDeviceConfig {
                block_id: "test-err-block".to_string(),
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

            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());

            // Guest handles the error and exits cleanly; just wait for VM.
            vm_thread.join().ok();

            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom};

    impl Test for TestBlockBackendErrors {
        fn in_guest(self: Box<Self>) {
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/vda")
                .expect("Failed to open /dev/vda");

            // Sector 0: good sector — read should succeed and return 0xAA bytes
            let mut buf = vec![0u8; 512];
            f.read_exact(&mut buf).expect("sector 0 read should succeed");
            assert!(
                buf.iter().all(|&b| b == 0xAA),
                "sector 0 should be all 0xAA, got {:?}",
                &buf[..4]
            );

            // Sector 5: error sector — read should fail with EIO
            f.seek(SeekFrom::Start(5 * 512))
                .expect("seek to sector 5 should succeed");
            let result = f.read_exact(&mut buf);
            assert!(
                result.is_err(),
                "sector 5 read should return an error (EIO from backend)"
            );

            // Gracefully exit after handling the error
            println!("OK");
        }
    }
}
