use macros::{guest, host};

pub struct TestVhostUserFsDaxAlways;
pub struct TestVhostUserFsDaxInode;
pub struct TestVhostUserFsDaxNever;

const VSOCK_PORT_ALWAYS: u32 = 5685;
const VSOCK_PORT_INODE: u32 = 5686;
const VSOCK_PORT_NEVER: u32 = 5687;

#[host]
mod host_helpers {
    use std::path::Path;
    use std::process::{Child, Command};
    use std::thread;
    use std::time::Duration;

    pub const DAX_WINDOW_MIB: u32 = 32;
    pub const FS_TAG: &str = "testfs";

    /// Start the test daemon and return the child process handle.
    pub fn start_test_daemon(socket_path: &Path) -> Child {
        let daemon_path = std::env::var("KRUN_TEST_DAEMON_PATH").unwrap_or_else(|_| {
            let exe = std::env::current_exe().unwrap();
            exe.parent()
                .unwrap()
                .join("test-daemon")
                .to_string_lossy()
                .to_string()
        });

        let child = Command::new(&daemon_path)
            .arg("--socket-path")
            .arg(socket_path)
            .spawn()
            .expect("Failed to start test-daemon");

        for _ in 0..50 {
            if socket_path.exists() {
                return child;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("test-daemon did not create socket within 5 seconds");
    }

    /// Common host-side pattern: start daemon, configure VM with vsock + virtiofs,
    /// wait for guest READY, snapshot, kill+restart daemon, restore, send CHECK,
    /// wait for VM exit, clean up.
    pub fn run_snapshot_test(
        test_setup: &crate::TestSetup,
        sock_name: &str,
        vsock_port: u32,
        dax_window_mib: Option<u32>,
    ) -> anyhow::Result<()> {
        use crate::krun_rust::setup_fs_builder;
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let socket_path = test_setup.tmp_dir.join(format!("{}.sock", sock_name));
        let snap_dir = test_setup.tmp_dir.join("snapshot");
        let vsock_path = test_setup
            .tmp_dir
            .join(format!("{}_control.sock", sock_name));

        // 1. Start test daemon
        let mut daemon = start_test_daemon(&socket_path);

        // 2. Configure VM
        let mut builder = krun::Builder::new();
        builder.vm_config(1, 512)?;
        setup_fs_builder(&mut builder, test_setup)?;
        builder.add_virtiofs_vhost_user(FS_TAG, socket_path.to_str().unwrap(), dax_window_mib)?;

        let listener = UnixListener::bind(&vsock_path)?;
        builder.add_vsock_port(vsock_port, vsock_path, false);

        let context = builder.build()?;
        let handle = context.vm_handle();
        let vm_thread = thread::spawn(move || context.run());

        // 3. Wait for guest READY
        let (mut stream, _) = listener.accept()?;
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf)?;
        assert_eq!(&buf, b"READY");

        // 4. Snapshot
        handle.snapshot(&snap_dir)?;

        // 5. Kill and restart daemon
        daemon.kill()?;
        daemon.wait()?;
        std::fs::remove_file(&socket_path).ok();
        daemon = start_test_daemon(&socket_path);

        // 6. Restore
        handle.restore_snapshot(&snap_dir)?;

        // 7. Signal guest to verify
        stream.write_all(b"CHECK")?;

        // 8. Wait for VM
        vm_thread.join().ok();

        // 9. Clean up
        daemon.kill().ok();
        daemon.wait().ok();

        Ok(())
    }
}

#[guest]
mod guest_helpers {
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// Mount virtiofs using libc::mount syscall directly.
    /// The guest VM has no /bin/mount binary.
    /// Tries with dax_option first, falls back to no options if EINVAL.
    pub fn mount_virtiofs(tag: &str, mountpoint: &str, dax_option: &str) {
        use std::ffi::CString;
        use std::fs;

        fs::create_dir_all(mountpoint).unwrap();

        let source = CString::new(tag).unwrap();
        let target = CString::new(mountpoint).unwrap();
        let fstype = CString::new("virtiofs").unwrap();
        let options = CString::new(dax_option).unwrap();

        let ret = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                0,
                options.as_ptr() as *const libc::c_void,
            )
        };
        if ret == 0 {
            return;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINVAL) && !dax_option.is_empty() {
            println!(
                "mount with '{}' failed (EINVAL), retrying without dax option",
                dax_option
            );
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
                "mount without dax option also failed: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        panic!("mount failed with errno {}", err);
    }

    /// Connect to host via vsock and send READY.
    pub fn vsock_send_ready(port: u32) -> UnixStream {
        use nix::libc::VMADDR_CID_HOST;
        use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
        use std::io::Write;
        use std::os::fd::AsRawFd;

        let sock = socket(
            AddressFamily::Vsock,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .unwrap();
        connect(sock.as_raw_fd(), &VsockAddr::new(VMADDR_CID_HOST, port)).unwrap();
        let mut stream = UnixStream::from(sock);
        stream.write_all(b"READY").unwrap();
        stream
    }

    /// Wait for host CHECK signal after snapshot/restore.
    pub fn wait_for_check(stream: &mut UnixStream) {
        use std::io::Read;

        let mut buf = [0u8; 5];
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"CHECK");
    }
}

// =============================================================================
// dax=always: read (0xBB via DAX) → write (0xCC) → readback → snapshot → read (0xBB)
// =============================================================================

#[host]
mod dax_always_host {
    use super::*;
    use crate::{Test, TestSetup};

    impl Test for TestVhostUserFsDaxAlways {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            host_helpers::run_snapshot_test(
                &test_setup,
                "dax-always",
                VSOCK_PORT_ALWAYS,
                Some(host_helpers::DAX_WINDOW_MIB),
            )
        }
    }
}

#[guest]
mod dax_always_guest {
    use super::*;
    use crate::Test;

    impl Test for TestVhostUserFsDaxAlways {
        fn in_guest(self: Box<Self>) {
            use std::fs;
            use std::fs::OpenOptions;
            use std::io::Write;

            // 1. Mount with dax=always
            guest_helpers::mount_virtiofs("testfs", "/mnt/testfs", "dax=always");

            // 2. Read via DAX — daemon fills DAX window with 0xBB
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();
            if data[0] == 0xAA {
                panic!("Got FUSE_READ pattern 0xAA instead of DAX pattern 0xBB — DAX not active");
            }
            assert!(
                data.iter().all(|&b| b == 0xBB),
                "Expected 0xBB, got {:02x}",
                data[0]
            );

            // 3. Write 0xCC via DAX, read back
            {
                let mut f = OpenOptions::new()
                    .write(true)
                    .open("/mnt/testfs/hello.txt")
                    .unwrap();
                f.write_all(&vec![0xCC; 4096]).unwrap();
                f.flush().unwrap();
            }
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();
            assert!(
                data.iter().take(4096).all(|&b| b == 0xCC),
                "Write readback: expected 0xCC, got {:02x}",
                data[0]
            );

            // 4. Signal READY, wait for snapshot/restore
            let mut stream = guest_helpers::vsock_send_ready(VSOCK_PORT_ALWAYS);
            guest_helpers::wait_for_check(&mut stream);

            // 5. Post-restore: DAX memfd content survives snapshot (0xCC from write persists)
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();
            assert!(
                data.iter().take(4096).all(|&b| b == 0xCC),
                "Post-restore: expected 0xCC (written data preserved in DAX memfd), got {:02x}",
                data[0]
            );

            println!("OK");
        }
    }
}

// =============================================================================
// dax=inode: hello.txt (DAX, 0xBB) + nodax.txt (no DAX, 0xAA) → snapshot → both survive
// =============================================================================

#[host]
mod dax_inode_host {
    use super::*;
    use crate::{Test, TestSetup};

    impl Test for TestVhostUserFsDaxInode {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            host_helpers::run_snapshot_test(
                &test_setup,
                "dax-inode",
                VSOCK_PORT_INODE,
                Some(host_helpers::DAX_WINDOW_MIB),
            )
        }
    }
}

#[guest]
mod dax_inode_guest {
    use super::*;
    use crate::Test;

    impl Test for TestVhostUserFsDaxInode {
        fn in_guest(self: Box<Self>) {
            use std::fs;
            use std::fs::OpenOptions;
            use std::io::Write;

            // 1. Mount with dax=inode — per-inode DAX controlled by FUSE_ATTR_DAX
            guest_helpers::mount_virtiofs("testfs", "/mnt/testfs", "dax=inode");

            // 2. Read hello.txt (dax_enabled=true) — should use DAX window → 0xBB
            let dax_data = fs::read("/mnt/testfs/hello.txt").unwrap();
            if dax_data[0] == 0xAA {
                panic!("hello.txt: got FUSE_READ pattern 0xAA, expected DAX pattern 0xBB — per-inode DAX not working");
            }
            assert!(
                dax_data.iter().all(|&b| b == 0xBB),
                "hello.txt: expected 0xBB, got {:02x}",
                dax_data[0]
            );

            // 3. Read nodax.txt (dax_enabled=false) — should use FUSE_READ → 0xAA
            let nodax_data = fs::read("/mnt/testfs/nodax.txt").unwrap();
            assert!(
                nodax_data.iter().all(|&b| b == 0xAA),
                "nodax.txt: expected FUSE_READ pattern 0xAA, got {:02x}",
                nodax_data[0]
            );

            // 4. Write 0xCC to hello.txt via DAX, read back
            {
                let mut f = OpenOptions::new()
                    .write(true)
                    .open("/mnt/testfs/hello.txt")
                    .unwrap();
                f.write_all(&vec![0xCC; 4096]).unwrap();
                f.flush().unwrap();
            }
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();
            assert!(
                data.iter().take(4096).all(|&b| b == 0xCC),
                "Write readback: expected 0xCC, got {:02x}",
                data[0]
            );

            // 5. Signal READY, wait for snapshot/restore
            let mut stream = guest_helpers::vsock_send_ready(VSOCK_PORT_INODE);
            guest_helpers::wait_for_check(&mut stream);

            // 6. Post-restore: hello.txt DAX content survives (0xCC), nodax.txt still via FUSE_READ (0xAA)
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();
            assert!(
                data.iter().take(4096).all(|&b| b == 0xCC),
                "Post-restore hello.txt: expected 0xCC, got {:02x}",
                data[0]
            );
            let nodax_data = fs::read("/mnt/testfs/nodax.txt").unwrap();
            assert!(
                nodax_data.iter().all(|&b| b == 0xAA),
                "Post-restore nodax.txt: expected 0xAA, got {:02x}",
                nodax_data[0]
            );

            println!("OK");
        }
    }
}

// =============================================================================
// dax=never: all reads via FUSE_READ (0xAA) even with DAX window configured → snapshot → read
// =============================================================================

#[host]
mod dax_never_host {
    use super::*;
    use crate::{Test, TestSetup};

    impl Test for TestVhostUserFsDaxNever {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            // DAX window is configured but guest mounts with dax=never
            host_helpers::run_snapshot_test(
                &test_setup,
                "dax-never",
                VSOCK_PORT_NEVER,
                Some(host_helpers::DAX_WINDOW_MIB),
            )
        }
    }
}

#[guest]
mod dax_never_guest {
    use super::*;
    use crate::Test;

    impl Test for TestVhostUserFsDaxNever {
        fn in_guest(self: Box<Self>) {
            use std::fs;

            // 1. Mount with dax=never — kernel ignores DAX window, uses FUSE_READ
            guest_helpers::mount_virtiofs("testfs", "/mnt/testfs", "dax=never");

            // 2. Read hello.txt — must get FUSE_READ pattern 0xAA (not DAX 0xBB)
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();
            assert!(
                data.iter().all(|&b| b == 0xAA),
                "Expected FUSE_READ pattern 0xAA with dax=never, got {:02x}",
                data[0]
            );

            // 3. Signal READY, wait for snapshot/restore
            let mut stream = guest_helpers::vsock_send_ready(VSOCK_PORT_NEVER);
            guest_helpers::wait_for_check(&mut stream);

            // 4. Post-restore: verify FUSE_READ still works
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();
            assert!(
                data.iter().all(|&b| b == 0xAA),
                "Post-restore: expected 0xAA, got {:02x}",
                data[0]
            );

            println!("OK");
        }
    }
}
