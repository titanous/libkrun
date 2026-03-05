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
    use std::os::unix::fs::OpenOptionsExt;
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom};

    // O_DIRECT requires 512-byte aligned buffers.
    #[repr(align(512))]
    struct AlignedSector([u8; 512]);

    impl Test for TestBlockBackendErrors {
        fn in_guest(self: Box<Self>) {
            // O_DIRECT bypasses the page cache so each read goes directly to
            // the backend, ensuring sector-5 errors aren't hidden by readahead.
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_DIRECT)
                .open("/dev/vda")
                .expect("Failed to open /dev/vda");

            // Sector 0: good sector — read should succeed and return 0xAA bytes
            let mut buf = AlignedSector([0u8; 512]);
            f.read_exact(&mut buf.0).expect("sector 0 read should succeed");
            assert!(
                buf.0.iter().all(|&b| b == 0xAA),
                "sector 0 should be all 0xAA, got {:?}",
                &buf.0[..4]
            );

            // Sector 5: error sector — read should fail with EIO
            f.seek(SeekFrom::Start(5 * 512))
                .expect("seek to sector 5 should succeed");
            let result = f.read_exact(&mut buf.0);
            assert!(
                result.is_err(),
                "sector 5 read should return an error (EIO from backend)"
            );

            // Gracefully exit after handling the error
            println!("OK");
        }
    }
}
