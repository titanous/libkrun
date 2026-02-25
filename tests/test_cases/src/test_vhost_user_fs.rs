use macros::{guest, host};

pub struct TestVhostUserFsDaxRead;
pub struct TestVhostUserFsDaxWrite;
pub struct TestVhostUserFsDaxSnapshot;

const VSOCK_PORT: u32 = 5685;

#[host]
mod host_helpers {
    use std::process::{Child, Command};
    use std::path::Path;
    use std::thread;
    use std::time::Duration;

    pub const DAX_WINDOW_MIB: u32 = 32;
    pub const FS_TAG: &str = "testfs";

    /// Start the test daemon and return the child process handle.
    /// Caller must kill the child when done.
    pub fn start_test_daemon(socket_path: &Path) -> Child {
        // Find test-daemon binary
        // Built alongside test_cases in the tests workspace
        let daemon_path = std::env::var("KRUN_TEST_DAEMON_PATH")
            .unwrap_or_else(|_| {
                // Fallback: look relative to current exe
                let exe = std::env::current_exe().unwrap();
                exe.parent().unwrap().join("test-daemon").to_string_lossy().to_string()
            });

        let child = Command::new(&daemon_path)
            .arg("--socket-path")
            .arg(socket_path)
            .spawn()
            .expect("Failed to start test-daemon");

        // Wait for socket to appear
        for _ in 0..50 {
            if socket_path.exists() {
                return child;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("test-daemon did not create socket within 5 seconds");
    }
}

#[host]
mod dax_read {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::thread;

    impl Test for TestVhostUserFsDaxRead {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let socket_path = test_setup.tmp_dir.join("vhost-fs.sock");

            // 1. Start test daemon
            let mut daemon = host_helpers::start_test_daemon(&socket_path);

            // 2. Configure VM with vhost-user FS + DAX
            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_virtiofs_vhost_user(host_helpers::FS_TAG, socket_path.to_str().unwrap(), Some(host_helpers::DAX_WINDOW_MIB))?;

            // 3. Start VM
            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());

            // 4. Wait for VM to finish
            vm_thread.join().ok();

            // 5. Clean up daemon
            daemon.kill().ok();
            daemon.wait().ok();

            Ok(())
        }
    }
}

#[guest]
mod dax_read_guest {
    use super::*;
    use crate::Test;

    impl Test for TestVhostUserFsDaxRead {
        fn in_guest(self: Box<Self>) {
            use std::fs;
            use std::process::Command;

            // 1. Create mount point and mount virtiofs
            fs::create_dir_all("/mnt/testfs").unwrap();
            let status = Command::new("mount")
                .args(["-t", "virtiofs", "testfs", "/mnt/testfs", "-o", "dax=inode"])
                .status()
                .unwrap();
            assert!(status.success(), "mount failed");

            // 2. Read file via DAX (mmap)
            // When DAX is active, reading a file that has FUSE_ATTR_DAX will use
            // the DAX window instead of FUSE_READ. The daemon writes 0xBB to DAX
            // but returns 0xAA via FUSE_READ.
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();

            // 3. Verify DAX-specific byte pattern (0xBB, not FUSE_READ's 0xAA)
            // AC6.1: Proves DAX is active
            if data[0] == 0xAA {
                // Diagnostic: got FUSE_READ content instead of DAX content.
                // This means the kernel is not using DAX. Common causes:
                // - Kernel version < 6.2 (no per-file DAX support)
                // - Missing CONFIG_FUSE_DAX kernel config
                // - dax=inode mount option not taking effect
                // Check kernel version for diagnostic output:
                let uname = std::process::Command::new("uname").arg("-r").output();
                let kver = uname.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_else(|_| "unknown".to_string());
                panic!(
                    "Got FUSE_READ pattern 0xAA instead of DAX pattern 0xBB. \
                     DAX is not active. Kernel version: {}. \
                     Requires kernel >= 6.2 with CONFIG_FUSE_DAX.",
                    kver
                );
            }
            assert!(
                data.iter().all(|&b| b == 0xBB),
                "Expected DAX pattern 0xBB but got mixed content starting with {:02x}",
                data[0]
            );

            println!("OK");
        }
    }
}

#[host]
mod dax_write {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::thread;

    impl Test for TestVhostUserFsDaxWrite {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let socket_path = test_setup.tmp_dir.join("vhost-fs-write.sock");

            // 1. Start test daemon
            let mut daemon = host_helpers::start_test_daemon(&socket_path);

            // 2. Configure VM with vhost-user FS + DAX
            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_virtiofs_vhost_user(host_helpers::FS_TAG, socket_path.to_str().unwrap(), Some(host_helpers::DAX_WINDOW_MIB))?;

            // 3. Start VM
            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());

            // 4. Wait for VM to finish
            vm_thread.join().ok();

            // 5. Clean up daemon
            daemon.kill().ok();
            daemon.wait().ok();

            Ok(())
        }
    }
}

#[guest]
mod dax_write_guest {
    use super::*;
    use crate::Test;

    impl Test for TestVhostUserFsDaxWrite {
        fn in_guest(self: Box<Self>) {
            use std::fs::{self, OpenOptions};
            use std::io::{Write};
            use std::process::Command;

            // 1. Mount virtiofs with DAX
            fs::create_dir_all("/mnt/testfs").unwrap();
            let status = Command::new("mount")
                .args(["-t", "virtiofs", "testfs", "/mnt/testfs", "-o", "dax=inode"])
                .status()
                .unwrap();
            assert!(status.success(), "mount failed");

            // 2. Write known pattern to file via DAX
            let write_pattern = vec![0xCC_u8; 4096];
            {
                let mut f = OpenOptions::new()
                    .write(true)
                    .open("/mnt/testfs/hello.txt")
                    .unwrap();
                f.write_all(&write_pattern).unwrap();
                f.flush().unwrap();
            }

            // 3. Read back and verify written content persists
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();
            assert!(
                data.iter().take(4096).all(|&b| b == 0xCC),
                "Expected written pattern 0xCC but got {:02x}",
                data[0]
            );

            println!("OK");
        }
    }
}

#[host]
mod dax_snapshot {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    impl Test for TestVhostUserFsDaxSnapshot {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let socket_path = test_setup.tmp_dir.join("vhost-fs-snap.sock");
            let snap_dir = test_setup.tmp_dir.join("snapshot");
            let vsock_path = test_setup.tmp_dir.join("snap_control.sock");

            // 1. Start test daemon
            let mut daemon = host_helpers::start_test_daemon(&socket_path);

            // 2. Configure VM
            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_virtiofs_vhost_user(host_helpers::FS_TAG, socket_path.to_str().unwrap(), Some(host_helpers::DAX_WINDOW_MIB))?;

            // Add vsock for guest synchronization
            let listener = UnixListener::bind(&vsock_path)?;
            builder.add_vsock_port(VSOCK_PORT, vsock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();
            let vm_thread = thread::spawn(move || context.run());

            // 3. Wait for guest to signal READY (DAX read succeeded)
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
            daemon = host_helpers::start_test_daemon(&socket_path);

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
}

#[guest]
mod dax_snapshot_guest {
    use super::*;
    use crate::Test;

    impl Test for TestVhostUserFsDaxSnapshot {
        fn in_guest(self: Box<Self>) {
            use std::fs;
            use std::process::Command;
            use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
            use nix::libc::VMADDR_CID_HOST;
            use std::io::{Read, Write};
            use std::os::fd::AsRawFd;
            use std::os::unix::net::UnixStream;
            use std::time::Duration;

            // 1. Mount virtiofs with DAX
            fs::create_dir_all("/mnt/testfs").unwrap();
            let status = Command::new("mount")
                .args(["-t", "virtiofs", "testfs", "/mnt/testfs", "-o", "dax=inode"])
                .status()
                .unwrap();
            assert!(status.success(), "mount failed");

            // 2. Read file via DAX, verify 0xBB pattern
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();
            assert!(data.iter().all(|&b| b == 0xBB), "Pre-snapshot DAX read failed");

            // 3. Signal host: READY
            let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None).unwrap();
            connect(sock.as_raw_fd(), &VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT)).unwrap();
            let mut stream = UnixStream::from(sock);
            stream.write_all(b"READY").unwrap();

            // --- SNAPSHOT HAPPENS HERE ---
            // After restore, guest resumes execution from this point.
            // The blocking read_exact below will receive the host's "CHECK"
            // message, matching the existing test_snapshot_restore.rs pattern.

            // 4. Wait for host: CHECK (after restore)
            let mut buf = [0u8; 5];
            stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"CHECK");

            // 5. Verify file access works after restore
            // The DAX window is a cache: after restore the memfd is zeroed,
            // and the kernel re-faults pages from the daemon (whose state was
            // restored via DEVICE_STATE). fs::read() triggers this re-population.
            let data = fs::read("/mnt/testfs/hello.txt").unwrap();
            assert!(
                data.iter().all(|&b| b == 0xBB),
                "Post-restore file read failed: got {:02x}, expected 0xBB (daemon state restore or DAX re-fault failed)",
                data[0]
            );

            println!("OK");
        }
    }
}
