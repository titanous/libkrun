//! Integration test for SlowBlockBackend with artificial latency.
//!
//! Verifies that a custom AsyncBlockBackend with a per-operation delay
//! still delivers correct data and that the VM runs to clean exit.

use macros::{guest, host};

pub struct TestBlockBackendSlow;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::slow_block_backend::{SlowBlockBackend, SlowBlockBackendFactory};
    use crate::{Test, TestSetup};
    use std::thread;
    use std::time::Duration;

    const SECTOR_COUNT: u64 = 16;
    const FILL_BYTE: u8 = 0x00;
    const OP_DELAY: Duration = Duration::from_millis(20);

    impl Test for TestBlockBackendSlow {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let (backend, _data) = SlowBlockBackend::new(SECTOR_COUNT, FILL_BYTE, OP_DELAY);
            let factory = SlowBlockBackendFactory::new(backend);

            let block_cfg = krun::BlockDeviceConfig {
                block_id: "test-slow-block".to_string(),
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
    use std::io::{Read, Seek, SeekFrom, Write};

    const WRITE_PATTERN: &[u8] = b"SLOW_BACKEND_TEST";

    impl Test for TestBlockBackendSlow {
        fn in_guest(self: Box<Self>) {
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/vda")
                .expect("Failed to open /dev/vda");

            // Write pattern to sector 0
            let mut sector = vec![0u8; 512];
            sector[..WRITE_PATTERN.len()].copy_from_slice(WRITE_PATTERN);
            f.seek(SeekFrom::Start(0)).expect("seek to sector 0");
            f.write_all(&sector).expect("write to sector 0");
            f.flush().expect("flush");

            // Read back and verify
            f.seek(SeekFrom::Start(0))
                .expect("seek to sector 0 for read");
            let mut read_back = vec![0u8; 512];
            f.read_exact(&mut read_back).expect("read sector 0");

            assert_eq!(
                &read_back[..WRITE_PATTERN.len()],
                WRITE_PATTERN,
                "read-back mismatch after slow write"
            );

            println!("OK");
        }
    }
}
