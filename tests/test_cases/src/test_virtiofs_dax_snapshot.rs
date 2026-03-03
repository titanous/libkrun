//! Integration test: virtiofs with DAX window + snapshot/restore (AC3.6).
//!
//! Uses a PassthroughFs (generic FileSystem) with a 512 MiB DAX window.
//! Guest writes a file, host takes a hot snapshot and restores, guest reads
//! back and verifies the file content survived the snapshot/restore cycle.

use macros::{guest, host};

pub struct TestVirtiofsDaxSnapshot;

const VSOCK_PORT: u32 = 5723;
const FS_TAG: &str = "daxfs";

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    const DAX_WINDOW_SIZE: usize = 1 << 29; // 512 MiB

    impl Test for TestVirtiofsDaxSnapshot {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            use anyhow::Context;
            use std::fs;

            let fs_root = test_setup.tmp_dir.join("dax_fs_root");
            fs::create_dir_all(&fs_root).context("create fs root")?;

            // Pre-create a file that the guest will verify before writing
            fs::write(fs_root.join("pre-existing.txt"), b"PRE_EXISTING_CONTENT")
                .context("write pre-existing.txt")?;

            let cfg = krun::passthrough::Config {
                root_dir: fs_root
                    .to_str()
                    .context("fs_root not valid UTF-8")?
                    .to_string(),
                ..Default::default()
            };
            let pt = krun::passthrough::PassthroughFs::new(cfg).context("PassthroughFs::new")?;

            let sock_path = test_setup.tmp_dir.join("dax_snap_ctrl.sock");
            let snap_dir = test_setup.tmp_dir.join("snapshot");
            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_virtiofs(FS_TAG, Box::new(pt), Some(DAX_WINDOW_SIZE));
            builder.add_vsock_port(VSOCK_PORT, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();

            let vm_thread = thread::spawn(move || context.run());

            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();

            // Phase 1: wait for guest to write file
            let mut buf = vec![0u8; 7];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"WRITTEN");

            // Take hot snapshot
            handle.snapshot(&snap_dir)?;

            // Signal guest to proceed with restore-phase verification
            stream.write_all(b"SNAP_OK").unwrap();

            // Wait for guest to verify after hot restore
            let mut buf = vec![0u8; 2];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"OK");

            drop(stream);
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
    use std::ffi::CString;
    use std::fs;
    use std::io::{Read, Write};

    const MOUNT_POINT: &str = "/mnt/dax";

    impl Test for TestVirtiofsDaxSnapshot {
        fn in_guest(self: Box<Self>) {
            // Mount the DAX-enabled virtiofs
            fs::create_dir_all(MOUNT_POINT).expect("create mountpoint");

            let source = CString::new(FS_TAG).unwrap();
            let target = CString::new(MOUNT_POINT).unwrap();
            let fstype = CString::new("virtiofs").unwrap();

            let ret = unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    fstype.as_ptr(),
                    0,
                    std::ptr::null(),
                )
            };
            assert!(
                ret == 0,
                "mount virtiofs failed: {}",
                std::io::Error::last_os_error()
            );

            // Verify pre-existing file
            let pre_path = format!("{}/pre-existing.txt", MOUNT_POINT);
            let pre_content = fs::read(&pre_path).expect("read pre-existing.txt");
            assert_eq!(&pre_content, b"PRE_EXISTING_CONTENT");

            // Write a test file
            let test_path = format!("{}/snapshot-test.txt", MOUNT_POINT);
            let test_data = b"DATA_AFTER_SNAPSHOT_RESTORE";
            fs::write(&test_path, test_data).expect("write test file to DAX fs");

            // Signal host that file is written; host will snapshot
            let mut stream = vsock_connect(VSOCK_PORT);
            stream.write_all(b"WRITTEN").unwrap();

            // Wait for host to snapshot and restore
            let mut buf = vec![0u8; 7];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"SNAP_OK");

            // After hot restore, verify file content
            let read_back = fs::read(&test_path).expect("read snapshot-test.txt after restore");
            assert_eq!(
                &read_back, test_data,
                "file content should match after hot snapshot/restore"
            );

            stream.write_all(b"OK").unwrap();
            println!("OK");
        }
    }
}
