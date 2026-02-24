# Test Coverage Implementation Plan — Phase 6

**Goal:** End-to-end snapshot/restore integration tests — full snapshot cycle, incremental snapshot cycle, and error path tests for malformed snapshot files.

**Architecture:** Three new test files in `tests/test_cases/src/`. AC6.1/6.2 use the vsock coordination pattern from `TestVsockGuestConnect`: guest signals `"READY"` to host via vsock, host performs snapshot operations, guest verifies state, guest prints `"OK"`. AC6.3/6.4/6.5 are host-only: they construct malformed snapshot files directly via `bincode` serialization of `SnapshotHeader` (without running a guest VM), call `Context::restore_and_run()`, and assert the correct `StartError` variant is returned, then print `"OK"`. All tests gated on `#[cfg(feature = "snapshot")]` in `krun`. Note: AC6.3/6.4/6.5 need `SnapshotHeader` to be accessible for constructing test files — the implementor must check if it is re-exported from `krun` or if test files are written by modifying bytes of a valid snapshot.

**Tech Stack:** Rust, `krun` crate, `bincode` 1.3, `std::net::UnixListener/UnixStream`, `macros::{host, guest}`.

**Scope:** Phase 6 of 8 phases

**Dependencies:** Phase 5 (infrastructure). Also requires Phase 7 Task 1 (`Builder::vm_config()` → `Result`) to be completed before or alongside this phase, because Phase 6 code uses `builder.vm_config(N, M)?`. If executing Phase 6 before Phase 7 Task 1, drop the `?` and treat `vm_config()` as returning `&mut Self`, then add `?` after Phase 7 Task 1 is applied.

**Codebase verified:** 2026-02-23

---

## Acceptance Criteria Coverage

This phase implements and tests:

### test-coverage.AC6: Snapshot/restore integration (full cycle)
- **test-coverage.AC6.1 Success:** VM starts, guest signals `"READY"` via vsock, host creates full snapshot, VM restores, guest verifies a pre-snapshot counter value is preserved
- **test-coverage.AC6.2 Success:** Full snapshot taken → dirty tracking enabled → guest modifies a memory region → incremental snapshot taken → restore from incremental → guest verifies only the written region changed
- **test-coverage.AC6.3 Failure:** Restore from file with wrong magic → `InvalidMagic` returned to caller
- **test-coverage.AC6.4 Failure:** Restore with vCPU count mismatch → `VcpuCountMismatch` returned
- **test-coverage.AC6.5 Failure:** Restore with `nested_enabled` mismatch → error returned

---

## Codebase Findings (Phase 6 Investigation)

### Snapshot VmHandle methods (all `#[cfg(feature = "snapshot")]`)

All on `VmHandle` (obtained via `context.vm_handle()` before `context.run()`):

```rust
handle.snapshot(&path) -> Result<(), StartError>
    // Pauses vCPUs, creates snapshot dir at path, resumes vCPUs

handle.restore_snapshot(&path) -> Result<(), StartError>
    // Pauses vCPUs, restores snapshot from path (hot restore), resumes vCPUs

handle.enable_dirty_tracking() -> Result<(), StartError>
    // Enables dirty page tracking for subsequent incremental snapshots

handle.incremental_snapshot(&path) -> Result<(), StartError>
    // Pauses vCPUs, creates incremental snapshot (dirty pages only), resumes

handle.restore_incremental_snapshot(&path) -> Result<(), StartError>
    // Pauses vCPUs, restores incremental snapshot, resumes
```

`Context::restore_and_run(self, base_path, incremental_paths)` — cold restore from snapshot files without a running VM. Returns `Err(StartError::...)` immediately if snapshot files are malformed.

### SnapshotError → StartError wrapping

`VmHandle::snapshot_err_to_start_error()` wraps `SnapshotError` inside `StartError::Microvm(StartMicrovmError::Internal(...))`. The error string is preserved but the variant nesting is deep. To check for specific snapshot errors in tests, match on `StartError` and inspect the inner message string, or check the error display. The task-implementor should verify whether `SnapshotError` variants can be matched directly from the public API or only via string matching.

### SnapshotHeader (from phase_01.md investigation)

```rust
struct SnapshotHeader {
    magic: u32,       // must be 0x4B52_534E
    version: u32,     // must be 1
    vcpu_count: u32,
    ram_regions: Vec<(u64, u64)>,
    nested_enabled: bool,
}
```

Serialized with `bincode` 1.3 into the snapshot header file. To create malformed files for AC6.3/6.4/6.5, the implementor can write raw bytes directly (wrong magic = any 4 bytes ≠ `0x4B52_534E`). The snapshot directory layout (what files it contains, their names) must be verified by the task-implementor by calling `snapshot()` in AC6.1 and inspecting the created directory.

### Vsock coordination pattern (from TestVsockGuestConnect)

```rust
const VSOCK_PORT: u32 = SOME_PORT;

// Host thread waits for connection and coordinates:
let listener = UnixListener::bind(&sock_path).unwrap();
// ... in a separate thread: ...
let (mut stream, _) = listener.accept().unwrap();
stream_expect_msg(&mut stream, b"READY");
// ... do snapshot operations ...

// Guest side:
let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None).unwrap();
connect(sock, VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT)).unwrap();
stream.write_all(b"READY").unwrap();
// ... wait for response ...
println!("OK");
```

The sock_path is a Unix socket on the host that vsock maps to via `builder.add_vsock_port(VSOCK_PORT, sock_path, false)`.

### AC6.3/6.4/6.5 approach — host-only error tests

These tests do NOT start a VM. They:
1. Create a snapshot directory with a malformed header file
2. Build a minimal `Context` (1 vCPU, 256 MiB RAM, no exec, no root)
3. Call `context.restore_and_run(&bad_dir, &[])` — fails immediately on snapshot validation
4. Assert the returned `StartError` contains the expected message/variant
5. Print `"OK"` to stdout; child process exits cleanly

**Important:** `Builder::build()` requires KVM (it sets up the VMM). These tests must run in the integration test environment (with `/dev/kvm`). They are "host-only" in the sense that no guest VM executes, but the Builder/Context initialization still runs.

---

## Tasks

<!-- START_SUBCOMPONENT_A (tasks 1-4) -->

<!-- START_TASK_1 -->
### Task 1: Create test_snapshot_restore.rs (AC6.1 — full snapshot cycle)

**Verifies:** test-coverage.AC6.1

**Files:**
- Create: `tests/test_cases/src/test_snapshot_restore.rs`

**Implementation:**

```rust
use macros::{guest, host};

pub struct TestSnapshotRestore;

const VSOCK_PORT: u32 = 5678;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    use std::path::PathBuf;
    use std::thread;

    impl Test for TestSnapshotRestore {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("snap_control.sock");
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_vsock_port(VSOCK_PORT, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();

            // Spawn VM in background — it runs until the guest exits
            let vm_thread = thread::spawn(move || context.run());

            // Wait for guest to signal READY (guest has set counter to 42)
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"READY");

            // Take full snapshot
            handle.snapshot(&snap_dir)?;

            // Hot-restore the snapshot (resets VM state back to the snapshot point)
            handle.restore_snapshot(&snap_dir)?;

            // Signal guest to verify counter is still 42
            stream.write_all(b"CHECK").unwrap();

            // Guest verifies counter == 42, prints "OK", then exits
            vm_thread.join().ok();
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use nix::libc::VMADDR_CID_HOST;
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    impl Test for TestSnapshotRestore {
        fn in_guest(self: Box<Self>) {
            // Use a static variable to survive snapshot/restore
            static COUNTER: std::sync::atomic::AtomicI32 =
                std::sync::atomic::AtomicI32::new(0);

            // Set counter to 42 — this value must be preserved after restore
            COUNTER.store(42, std::sync::atomic::Ordering::SeqCst);

            let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
                .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Signal host we are ready
            stream.write_all(b"READY").unwrap();

            // Wait for host to signal CHECK (after snapshot+restore)
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"CHECK");

            // After restore, counter must still be 42
            let val = COUNTER.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(val, 42, "counter was {val} after restore, expected 42");

            println!("OK");
        }
    }
}
```

**Note on static counter and restore:** `restore_snapshot()` resets the VM's memory and register state back to the snapshot point. Since the counter was set to 42 before signaling READY, and the snapshot was taken after that, the counter will be 42 after restore. The static `AtomicI32` lives in guest memory which is fully restored.

**Note on vsock after hot-restore:** `restore_snapshot()` restores the full VM memory including the guest's virtio-vsock socket state. The host-side Unix socket connection (`stream`, obtained from `listener.accept()`) remains intact as a real OS-level socket. The guest should still be in the state of waiting for `b"CHECK"`. However, whether the virtio-vsock descriptor ring and device state survive restore depends on whether the snapshot captures virtio device state. If the post-restore vsock write fails or times out, use an alternative: re-accept a new connection on the listener after restore, and have the guest re-connect.

**Note on vm_thread.join():** After the guest prints "OK" and exits, `context.run()` may return or the process may exit at OS level. Either way, the child process exits with stdout containing "OK\n".

**Verification:**

Run: `cargo build --features host -p test_cases && cargo build --features guest -p test_cases`
Expected: Both compile.

**Commit:** `test(snapshot): add full snapshot/restore integration test for AC6.1`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add incremental snapshot test (AC6.2)

**Verifies:** test-coverage.AC6.2

**Files:**
- Modify: `tests/test_cases/src/test_snapshot_restore.rs` — add `TestSnapshotRestoreIncremental`

**Implementation:**

Add a second struct and implementation to `test_snapshot_restore.rs`:

```rust
pub struct TestSnapshotRestoreIncremental;

const VSOCK_PORT_INCR: u32 = 5679;

#[host]
mod host_incr {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    use std::thread;

    impl Test for TestSnapshotRestoreIncremental {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("snap_incr_control.sock");
            let full_snap_dir = test_setup.tmp_dir.join("full_snapshot");
            let incr_snap_dir = test_setup.tmp_dir.join("incr_snapshot");

            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_vsock_port(VSOCK_PORT_INCR, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();

            let vm_thread = thread::spawn(move || context.run());

            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Phase 1: guest writes initial data, signals READY
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"READY");

            // Take full snapshot and enable dirty tracking
            handle.snapshot(&full_snap_dir)?;
            handle.enable_dirty_tracking()?;

            // Signal guest to write to a specific memory region
            stream.write_all(b"WRITE").unwrap();

            // Phase 2: guest writes known pattern, signals WRITTEN
            let mut buf = vec![0u8; 7];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"WRITTEN");

            // Take incremental snapshot (only dirty pages)
            handle.incremental_snapshot(&incr_snap_dir)?;

            // Restore from incremental snapshot
            handle.restore_incremental_snapshot(&incr_snap_dir)?;

            // Signal guest to verify: data written after full snapshot must still be present
            stream.write_all(b"VERIFY").unwrap();

            // Guest verifies and prints "OK"
            vm_thread.join().ok();
            Ok(())
        }
    }
}

#[guest]
mod guest_incr {
    use super::*;
    use crate::Test;
    use nix::libc::VMADDR_CID_HOST;
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    // A fixed-size buffer in data segment — this memory will be tracked as dirty
    static mut TEST_REGION: [u8; 64] = [0u8; 64];
    const EXPECTED_PATTERN: u8 = 0xAB;

    impl Test for TestSnapshotRestoreIncremental {
        fn in_guest(self: Box<Self>) {
            let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
                .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT_INCR);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Phase 1: signal ready with initial region state (zeros)
            stream.write_all(b"READY").unwrap();

            // Wait for WRITE signal
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"WRITE");

            // Write known pattern into the region
            unsafe { TEST_REGION.fill(EXPECTED_PATTERN); }

            stream.write_all(b"WRITTEN").unwrap();

            // Wait for VERIFY signal (after incremental restore)
            let mut buf = vec![0u8; 6];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"VERIFY");

            // After incremental restore, the written region should still have our pattern
            let pattern = unsafe { TEST_REGION[0] };
            assert_eq!(
                pattern, EXPECTED_PATTERN,
                "region pattern was {pattern:#x} after incremental restore"
            );

            println!("OK");
        }
    }
}
```

**Note on module naming:** The `#[host]` and `#[guest]` macros create `mod host` and `mod guest`. If both structs are in the same file, use different module names (`host_incr` / `guest_incr`) or combine them into the same host/guest modules. The task-implementor should verify the macro behavior and adjust accordingly.

**Commit:** `test(snapshot): add incremental snapshot integration test for AC6.2`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Create test_snapshot_errors.rs (AC6.3, AC6.4, AC6.5)

**Verifies:** test-coverage.AC6.3, test-coverage.AC6.4, test-coverage.AC6.5

**Files:**
- Create: `tests/test_cases/src/test_snapshot_errors.rs`

**Implementation:**

These are host-only tests (no guest). Each test creates a snapshot directory with a malformed header, calls `context.restore_and_run()`, and verifies the expected error.

**Snapshot directory format (confirmed from `src/vmm/src/snapshot.rs`):**

`create_full_snapshot()` writes exactly two files:
```
snapshot/
  vmstate  ← bincode-serialized VmSnapshot (contains SnapshotHeader + vcpu/device states)
  memory   ← raw RAM dump
```

`restore_from_snapshot` reads `vmstate` first (deserialized into VmSnapshot), then calls `validate_header_for_vm` on `vmstate.header`, then reads `memory`. For AC6.3/6.4/6.5 error tests, the error occurs during header validation — before `memory` is read — so `memory` can be an empty file.

**Creating malformed snapshot files:**

Write a complete, valid bincode-encoded `VmSnapshot` to the `vmstate` file with intentionally wrong header values. The `VmSnapshot` type is in `vmm` and not re-exported from `krun`, so use manual bincode encoding:

- bincode 1.3 (default config): little-endian, u64 length prefix for Vec
- `SnapshotHeader`: `magic(u32) + version(u32) + vcpu_count(u32) + ram_regions(Vec<(u64,u64)>) + nested_enabled(bool)`
- After header: `vcpu_states(Vec<Vec<u8>>) + device_states(Vec<(String,Vec<u8>)>) + gic_state(Option<Vec<u8>>) + vm_state(Option<Vec<u8>>)`

For error tests, use empty vcpu_states and device_states (both `0u64` length prefix), and `None` for gic_state and vm_state (both `0x00` byte).

```rust
use macros::host;

pub struct TestSnapshotWrongMagic;   // AC6.3
pub struct TestSnapshotVcpuMismatch; // AC6.4
pub struct TestSnapshotNestedMismatch; // AC6.5

#[host]
mod host {
    use super::*;
    use crate::{Test, TestSetup};
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    /// Write a minimal snapshot directory for error-path testing.
    /// Produces a valid bincode-encoded VmSnapshot at `dir/vmstate` with the
    /// given header fields, and an empty `dir/memory` file (error happens before
    /// memory is read for AC6.3/6.4/6.5).
    fn write_vmstate(dir: &Path, magic: u32, version: u32, vcpu_count: u32, nested: bool) {
        fs::create_dir_all(dir).unwrap();

        // Hand-encode a minimal VmSnapshot in bincode 1.3 format (little-endian,
        // u64 length prefix for collections).
        let mut data = Vec::new();

        // SnapshotHeader
        data.extend_from_slice(&magic.to_le_bytes());       // magic: u32
        data.extend_from_slice(&version.to_le_bytes());      // version: u32
        data.extend_from_slice(&vcpu_count.to_le_bytes());   // vcpu_count: u32
        // ram_regions: Vec<(u64,u64)> with 1 entry: (base=0, size=128MiB)
        // — must match what Builder::build() with vm_config(1, 128) allocates
        data.extend_from_slice(&1u64.to_le_bytes());              // Vec length = 1
        data.extend_from_slice(&0u64.to_le_bytes());              // region base = 0
        data.extend_from_slice(&(128u64 * 1024 * 1024).to_le_bytes()); // region size
        data.push(nested as u8);                             // nested_enabled: bool

        // vcpu_states: empty Vec<Vec<u8>> = length 0
        data.extend_from_slice(&0u64.to_le_bytes());
        // device_states: empty Vec<(String,Vec<u8>)> = length 0
        data.extend_from_slice(&0u64.to_le_bytes());
        // gic_state: None = 0x00 (bincode Option::None discriminant)
        data.push(0u8);
        // vm_state: None = 0x00
        data.push(0u8);

        let mut f = fs::File::create(dir.join("vmstate")).unwrap();
        f.write_all(&data).unwrap();

        // Empty memory file — error occurs before memory is read
        fs::File::create(dir.join("memory")).unwrap();
    }

    fn build_minimal_context(test_setup: &TestSetup) -> anyhow::Result<krun::Context> {
        let mut builder = krun::Builder::new();
        builder.vm_config(1, 128)?;
        // restore_and_run fails on snapshot validation before needing root/exec.
        // If Builder::build() requires root to succeed, add:
        //   use crate::krun_rust::setup_fs_builder;
        //   setup_fs_builder(&mut builder, test_setup)?;
        Ok(builder.build()?)
    }

    fn expect_snapshot_error(result: Result<(), krun::StartError>, expected_msg_fragment: &str) {
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains(expected_msg_fragment),
                    "Expected error containing '{expected_msg_fragment}', got: {msg}"
                );
            }
            Ok(()) => panic!("Expected error but restore_and_run succeeded"),
        }
    }

    impl Test for TestSnapshotWrongMagic {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("bad_snap");
            write_vmstate(&snap_dir, 0xDEADBEEF, 1, 1, false); // wrong magic

            let context = build_minimal_context(&test_setup)?;
            let result = context.restore_and_run(&snap_dir, &[]);
            expect_snapshot_error(result, "InvalidMagic");
            println!("OK");
            Ok(())
        }
    }

    impl Test for TestSnapshotVcpuMismatch {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("bad_snap");
            write_vmstate(&snap_dir, 0x4B52_534E, 1, 4, false); // vcpu_count=4, VM has 1

            let context = build_minimal_context(&test_setup)?;
            let result = context.restore_and_run(&snap_dir, &[]);
            expect_snapshot_error(result, "VcpuCount");
            println!("OK");
            Ok(())
        }
    }

    impl Test for TestSnapshotNestedMismatch {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("bad_snap");
            // nested_enabled=true in header, but VM has nested=false (default)
            write_vmstate(&snap_dir, 0x4B52_534E, 1, 1, true);

            let context = build_minimal_context(&test_setup)?;
            let result = context.restore_and_run(&snap_dir, &[]);
            expect_snapshot_error(result, "NestedEnabled");
            println!("OK");
            Ok(())
        }
    }
}
```

**Important implementation notes:**

1. **Snapshot file format verified:** `create_full_snapshot()` in `src/vmm/src/snapshot.rs` writes `vmstate` (bincode VmSnapshot) and `memory` (raw RAM dump). The `write_vmstate()` helper above matches this format exactly — no `header` file exists.

2. **ram_regions must match for AC6.4/6.5:** For AC6.4 (vcpu mismatch) and AC6.5 (nested mismatch), the wrong field must be the cause — other fields (including ram_regions) must match what the VM sees. A 128 MiB VM (`vm_config(1, 128)`) has a single RAM region at base 0 with size `128 * 1024 * 1024`. If libkrun aligns RAM differently, adjust the encoded region size. The task-implementor should verify by running AC6.1 first and inspecting the snapshot directory.

3. **Error message fragments:** `expect_snapshot_error` checks by string fragment since `SnapshotError` is wrapped inside `StartError`. Verify actual error messages by running a test case that expects a specific error and inspecting the output.

4. **AC6.5 requires Phase 1 fix:** The `nested_enabled` mismatch check only exists after the Phase 1 bug fix is applied. If Phase 1 has not been completed, this test will not return an error. AC6.5 depends on Phase 1 being complete.

**Verification:**

Run: `cargo build --features host -p test_cases`
Expected: Compiles without errors.

**Commit:** `test(snapshot): add snapshot error integration tests for AC6.3-AC6.5`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Register new test cases in lib.rs

**Verifies:** All AC6 tests registered and named uniquely

**Files:**
- Modify: `tests/test_cases/src/lib.rs`

**Implementation:**

Add module declarations and register all new test cases:

```rust
// At the top of lib.rs, with other mod declarations:
mod test_snapshot_restore;
use test_snapshot_restore::{TestSnapshotRestore, TestSnapshotRestoreIncremental};

mod test_snapshot_errors;
use test_snapshot_errors::{TestSnapshotWrongMagic, TestSnapshotVcpuMismatch, TestSnapshotNestedMismatch};
```

In `test_cases()`:
```rust
TestCase::new("snapshot-restore-full", Box::new(TestSnapshotRestore)),
TestCase::new("snapshot-restore-incremental", Box::new(TestSnapshotRestoreIncremental)),
TestCase::new("snapshot-error-wrong-magic", Box::new(TestSnapshotWrongMagic)),
TestCase::new("snapshot-error-vcpu-mismatch", Box::new(TestSnapshotVcpuMismatch)),
TestCase::new("snapshot-error-nested-mismatch", Box::new(TestSnapshotNestedMismatch)),
```

**Verification:**

Run: `cargo build --features host -p test_cases && cargo build --features guest -p test_cases`
Expected: Both compile without errors.

**Commit:** `test(snapshot): register snapshot integration tests in test_cases/lib.rs`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Run full integration test suite and verify

**Files:** None

**Step 1: Run the new snapshot tests**

Run: `make test` (or the equivalent command for this project — check the Makefile)
Expected: The 5 new snapshot tests pass:
- `snapshot-restore-full` (AC6.1)
- `snapshot-restore-incremental` (AC6.2)
- `snapshot-error-wrong-magic` (AC6.3)
- `snapshot-error-vcpu-mismatch` (AC6.4)
- `snapshot-error-nested-mismatch` (AC6.5)

**Step 2: Verify no regressions**

Expected: All 6 original tests still pass.

**Commit:** (only if any fixes were needed to make tests pass)
<!-- END_TASK_5 -->

<!-- END_SUBCOMPONENT_A -->
