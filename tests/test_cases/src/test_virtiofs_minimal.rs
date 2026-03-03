//! Integration test for a minimal FileSystem implementation.
//!
//! Verifies that a FileSystem impl with only lookup+read+getattr+open+opendir+readdir
//! can serve files via virtiofs. All other operations return ENOSYS.

use macros::{guest, host};

pub struct TestVirtiofsMinimalFs;

const VSOCK_PORT: u32 = 5720;
const FS_TAG: &str = "minimalfs";
const TEST_FILE: &str = "hello.txt";
const TEST_CONTENT: &[u8] = b"minimal filesystem content";

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::minimal_filesystem::MinimalFileSystem;
    use crate::{Test, TestSetup};
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    impl Test for TestVirtiofsMinimalFs {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("minimal_fs_ctrl.sock");
            let listener = UnixListener::bind(&sock_path).unwrap();

            let fs = MinimalFileSystem::new(vec![(TEST_FILE, TEST_CONTENT.to_vec())]);

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 256)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_virtiofs(
                FS_TAG,
                Box::new(fs),
                None, // no DAX window — test FUSE path only
            );
            builder.add_vsock_port(VSOCK_PORT, sock_path, false);

            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());

            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut buf = vec![0u8; 2];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"OK", "expected OK from guest virtiofs minimal test");

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
    use std::io::Write;

    const MOUNT_POINT: &str = "/mnt/minimal";

    impl Test for TestVirtiofsMinimalFs {
        fn in_guest(self: Box<Self>) {
            // Mount the minimal virtiofs
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

            // Read the test file
            let path = format!("{}/{}", MOUNT_POINT, TEST_FILE);
            let content = fs::read(&path).expect("read test file from minimal virtiofs");
            assert_eq!(
                &content, TEST_CONTENT,
                "content mismatch: got {:?}",
                &content
            );

            // Verify ENOSYS for unimplemented ops (write returns EROFS or ENOSYS)
            let write_result = fs::write(&path, b"should fail");
            assert!(
                write_result.is_err(),
                "write to read-only minimal FS should fail"
            );

            let mut stream = vsock_connect(VSOCK_PORT);
            stream.write_all(b"OK").unwrap();

            println!("OK");
        }
    }
}
