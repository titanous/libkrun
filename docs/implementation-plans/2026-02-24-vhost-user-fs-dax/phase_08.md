# VhostUserFs with DAX Implementation Plan - Phase 8

**Goal:** End-to-end integration tests verifying virtio-fs over vhost-user with DAX using the test daemon from Phase 7 and the `#[host]`/`#[guest]` proc macro framework.

**Architecture:** Tests use the existing integration test framework: host-side code spawns the test daemon, configures a VM with `add_virtiofs_vhost_user()`, and starts the VM. Guest-side code mounts the virtiofs filesystem, accesses files via DAX, and verifies byte patterns. For the snapshot test, the host triggers snapshot/restore while the guest verifies DAX content survives.

**Tech Stack:** Rust, libkrun Builder API, `#[host]`/`#[guest]` proc macros, test-daemon binary

**Scope:** 8 phases from original design (phase 8 of 8)

**Codebase verified:** 2026-02-24

**Reference files:**
- Test registration: `tests/test_cases/src/lib.rs:55-100`
- Rust API helpers: `tests/test_cases/src/krun_rust.rs`
- Snapshot test pattern: `tests/test_cases/src/test_snapshot_restore.rs`
- Custom block test pattern: `tests/test_cases/src/test_custom_block_backend.rs`
- Proc macros: `tests/macros/src/lib.rs:1-28`
- tests/CLAUDE.md, tests/test_cases/Cargo.toml

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-fs-dax.AC6: End-to-end integration tests pass
- **vhost-user-fs-dax.AC6.1 Success:** Guest mounts virtiofs, reads file, receives DAX-specific byte pattern (proving DAX active, not FUSE_READ fallback)
- **vhost-user-fs-dax.AC6.2 Success:** Guest writes to a DAX-mapped file, reads back via DAX, and verifies the written content persists
- **vhost-user-fs-dax.AC6.3 Success:** Snapshot/restore test: mount + DAX read, snapshot, daemon restart, restore, DAX content survives
- **vhost-user-fs-dax.AC6.4 Success:** Tests pass with `make test FEATURE_FLAGS="--features embedded_init,vhost-user"`
- **vhost-user-fs-dax.AC6.5 Edge:** Tests use `#[host]`/`#[guest]` proc macro framework consistent with existing test patterns

---

<!-- START_TASK_1 -->
### Task 1: Add vhost-user feature to test_cases dependencies

**Files:**
- Modify: `tests/test_cases/Cargo.toml`

**Implementation:**

Add `vhost-user` feature to the libkrun dependency:
```toml
libkrun = { path = "../../src/libkrun", optional = true, features = ["embedded_init", "net", "blk", "snapshot", "vhost-user"] }
```

This enables the `add_virtiofs_vhost_user()` Builder method in host-side test code.

Also add test-daemon as a build dependency or reference. The host-side test needs to know the path to the compiled test-daemon binary. Follow the pattern of KRUN_TEST_GUEST_AGENT_PATH — add an env var like `KRUN_TEST_DAEMON_PATH` or discover the path relative to the test binary.

**Verification:**
```bash
cd tests && cargo build -p test_cases --features host
```

**Commit:** `feat(tests): add vhost-user feature to test_cases dependencies`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Create test_vhost_user_fs module with host helpers

**Files:**
- Create: `tests/test_cases/src/test_vhost_user_fs.rs`
- Modify: `tests/test_cases/src/lib.rs` (add module + register tests)

**Implementation:**

Create the test file with host-side helpers:

```rust
use macros::{guest, host};

pub struct TestVhostUserFsDaxRead;
pub struct TestVhostUserFsDaxWrite;
pub struct TestVhostUserFsDaxSnapshot;

const DAX_WINDOW_MIB: u32 = 32;
const FS_TAG: &str = "testfs";
```

Host helper to start test daemon:

```rust
#[host]
mod host_helpers {
    use std::process::{Child, Command};
    use std::path::Path;
    use std::thread;
    use std::time::Duration;

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
```

Register tests in `lib.rs`:

```rust
mod test_vhost_user_fs;
use test_vhost_user_fs::{TestVhostUserFsDaxRead, TestVhostUserFsDaxWrite, TestVhostUserFsDaxSnapshot};

// In test_cases():
TestCase::new("vhost-user-fs-dax-read", Box::new(TestVhostUserFsDaxRead)),
TestCase::new("vhost-user-fs-dax-write", Box::new(TestVhostUserFsDaxWrite)),
TestCase::new("vhost-user-fs-dax-snapshot", Box::new(TestVhostUserFsDaxSnapshot)),
```

**Verification:**
```bash
cd tests && cargo build -p test_cases --features host
```

**Commit:** `feat(tests): create vhost-user-fs test module with host helpers`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Implement DAX read test

**Verifies:** vhost-user-fs-dax.AC6.1, vhost-user-fs-dax.AC6.5

**Files:**
- Modify: `tests/test_cases/src/test_vhost_user_fs.rs`

**Testing:**

**Host side (TestVhostUserFsDaxRead):**
```rust
#[host]
impl Test for TestVhostUserFsDaxRead {
    fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
        let socket_path = test_setup.tmp_dir.join("vhost-fs.sock");

        // 1. Start test daemon
        let mut daemon = start_test_daemon(&socket_path);

        // 2. Configure VM with vhost-user FS + DAX
        let mut builder = krun::Builder::new();
        builder.vm_config(1, 512)?;
        setup_fs_builder(&mut builder, &test_setup)?;
        builder.add_virtiofs_vhost_user(FS_TAG, socket_path.to_str().unwrap(), Some(DAX_WINDOW_MIB));

        // 3. Start VM
        let context = builder.build()?;
        let vm_thread = std::thread::spawn(move || context.run());

        // 4. Wait for VM to finish
        vm_thread.join().ok();

        // 5. Clean up daemon
        daemon.kill().ok();
        daemon.wait().ok();

        Ok(())
    }
}
```

**Guest side:**
```rust
#[guest]
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
```

**Note on DAX activation:** The guest kernel must support DAX (v6.2+ for per-file DAX with `FUSE_HAS_INODE_DAX`). The `dax=inode` mount option enables per-file DAX mode. When the kernel sees FUSE_ATTR_DAX on a file, it sends FUSE_SETUPMAPPING instead of FUSE_READ. The daemon's SETUPMAPPING handler writes 0xBB (the DAX pattern) to the DAX window. If DAX is not active, the kernel falls back to FUSE_READ which returns 0xAA. The test checks for 0xAA specifically to provide a diagnostic message about kernel DAX support before asserting.

**Verification:**
```bash
make test FEATURE_FLAGS="--features embedded_init,vhost-user"
```

**Commit:** `test(integration): add vhost-user-fs DAX read test`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Implement DAX write test

**Verifies:** vhost-user-fs-dax.AC6.2

**Files:**
- Modify: `tests/test_cases/src/test_vhost_user_fs.rs`

**Testing:**

**Host side (TestVhostUserFsDaxWrite):**
Same as DAX read test — start daemon, configure VM with vhost-user FS + DAX, start VM, wait.

**Guest side:**
```rust
#[guest]
impl Test for TestVhostUserFsDaxWrite {
    fn in_guest(self: Box<Self>) {
        use std::fs::{self, OpenOptions};
        use std::io::{Read, Write, Seek, SeekFrom};
        use std::process::Command;

        // 1. Mount virtiofs with DAX
        fs::create_dir_all("/mnt/testfs").unwrap();
        Command::new("mount")
            .args(["-t", "virtiofs", "testfs", "/mnt/testfs", "-o", "dax=inode"])
            .status()
            .unwrap();

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
```

AC6.2: Guest writes 0xCC via DAX, reads back 0xCC via DAX, proving write persistence.

**Verification:**
```bash
make test FEATURE_FLAGS="--features embedded_init,vhost-user"
```

**Commit:** `test(integration): add vhost-user-fs DAX write test`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Implement snapshot/restore test

**Verifies:** vhost-user-fs-dax.AC6.3

**Files:**
- Modify: `tests/test_cases/src/test_vhost_user_fs.rs`

**Testing:**

Follows the snapshot test pattern from `test_snapshot_restore.rs`. This relies on **hot-restore** semantics: `handle.restore_snapshot()` restores into the same VM process with the same socket descriptors. The vsock stream between host and guest survives because the host-side descriptor remains valid and the guest's kernel state is reset to the snapshot point. This matches the existing `test_snapshot_restore.rs` behavior.

**Host side (TestVhostUserFsDaxSnapshot):**
```rust
#[host]
impl Test for TestVhostUserFsDaxSnapshot {
    fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
        let socket_path = test_setup.tmp_dir.join("vhost-fs.sock");
        let snap_dir = test_setup.tmp_dir.join("snapshot");
        let vsock_path = test_setup.tmp_dir.join("snap_control.sock");

        // 1. Start test daemon
        let mut daemon = start_test_daemon(&socket_path);

        // 2. Configure VM
        let mut builder = krun::Builder::new();
        builder.vm_config(1, 512)?;
        setup_fs_builder(&mut builder, &test_setup)?;
        builder.add_virtiofs_vhost_user(FS_TAG, socket_path.to_str().unwrap(), Some(DAX_WINDOW_MIB));

        // Add vsock for guest synchronization
        let listener = std::os::unix::net::UnixListener::bind(&vsock_path)?;
        builder.add_vsock_port(5679, vsock_path, false);

        let context = builder.build()?;
        let handle = context.vm_handle();
        let vm_thread = std::thread::spawn(move || context.run());

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
```

**Guest side:**
```rust
#[guest]
impl Test for TestVhostUserFsDaxSnapshot {
    fn in_guest(self: Box<Self>) {
        use std::fs;
        use std::process::Command;
        use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
        use nix::libc::VMADDR_CID_HOST;
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;

        // 1. Mount virtiofs with DAX
        fs::create_dir_all("/mnt/testfs").unwrap();
        Command::new("mount")
            .args(["-t", "virtiofs", "testfs", "/mnt/testfs", "-o", "dax=inode"])
            .status()
            .unwrap();

        // 2. Read file via DAX, verify 0xBB pattern
        let data = fs::read("/mnt/testfs/hello.txt").unwrap();
        assert!(data.iter().all(|&b| b == 0xBB), "Pre-snapshot DAX read failed");

        // 3. Signal host: READY
        let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None).unwrap();
        connect(sock.as_raw_fd(), &VsockAddr::new(VMADDR_CID_HOST, 5679)).unwrap();
        let mut stream = UnixStream::from(sock);
        stream.write_all(b"READY").unwrap();

        // --- SNAPSHOT HAPPENS HERE ---
        // After restore, guest resumes execution from this point.
        // The blocking read_exact below will receive the host's "CHECK"
        // message, matching the existing test_snapshot_restore.rs pattern.

        // 4. Wait for host: CHECK (after restore)
        let mut buf = [0u8; 5];
        stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).unwrap();
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
```

AC6.3: mount + DAX read (0xBB), snapshot, daemon restart + state restore, restore VM, file read returns correct data (daemon serves 0xBB via DAX cache re-population).

**Verification:**
```bash
make test FEATURE_FLAGS="--features embedded_init,vhost-user"
```

**Commit:** `test(integration): add vhost-user-fs DAX snapshot/restore test`
<!-- END_TASK_5 -->

<!-- START_TASK_6 -->
### Task 6: Update test runner and Makefile for vhost-user tests

**Verifies:** vhost-user-fs-dax.AC6.4

**Files:**
- Modify: `tests/run.sh` (if needed to build test-daemon)
- Modify: `Makefile` (if needed for vhost-user test target)

**Implementation:**

Ensure the test runner builds test-daemon alongside the test harness. The test-daemon must be compiled and available at runtime. Options:

1. Build test-daemon as part of `cargo build` in the tests workspace (it's already a workspace member)
2. Set `KRUN_TEST_DAEMON_PATH` env var in run.sh to point to the compiled binary

Check that the existing `make test` flow includes the vhost-user feature:
```bash
make test FEATURE_FLAGS="--features embedded_init,vhost-user"
```

This should compile test_cases with the vhost-user feature enabled (via the libkrun dependency), and build test-daemon as a workspace member.

**Verification:**

Run the full test suite:
```bash
make test FEATURE_FLAGS="--features embedded_init,vhost-user"
```

Expected: All existing tests still pass, plus the three new vhost-user-fs tests:
- `vhost-user-fs-dax-read`
- `vhost-user-fs-dax-write`
- `vhost-user-fs-dax-snapshot`

AC6.4: Tests pass with the specified feature flags.
AC6.5: Tests use the `#[host]`/`#[guest]` framework consistently.

**Commit:** `feat(tests): wire up vhost-user-fs tests in test runner`
<!-- END_TASK_6 -->
