# Snapshot Completeness Implementation Plan — Phase 7

**Goal:** End-to-end verification that device state survives snapshot/restore from the guest perspective.

**Architecture:** Add integration tests in `tests/test_cases/src/` using the existing `#[host]`/`#[guest]` proc macro pattern. Tests verify serial scratch register, block device data, incremental snapshot state, and network connectivity survive snapshot/restore cycles. Follow existing `TestSnapshotRestore` and `TestSnapshotRestoreIncremental` patterns in `test_snapshot_restore.rs`.

**Tech Stack:** Rust (test_cases crate, krun Builder API, vsock host/guest communication)

**Scope:** 7 phases from original design (this is phase 7 of 7)

**Codebase verified:** 2026-02-24

---

## Acceptance Criteria Coverage

This phase implements and tests:

### snapshot-completeness.AC6: Integration tests
- **snapshot-completeness.AC6.1 Success:** Guest reads serial scratch register value that was written before snapshot
- **snapshot-completeness.AC6.2 Success:** Guest reads block device data that was written before snapshot
- **snapshot-completeness.AC6.3 Success:** Incremental snapshot/restore preserves guest state after workload
- **snapshot-completeness.AC6.4 Success:** Guest network connectivity works after snapshot/restore

---

<!-- START_TASK_1 -->
### Task 1: Serial scratch register snapshot test

**Verifies:** snapshot-completeness.AC6.1

**Files:**
- Create: `tests/test_cases/src/test_snapshot_serial.rs`
- Modify: `tests/test_cases/src/lib.rs` (register new test)

**Implementation:**

Create a new test file `test_snapshot_serial.rs` following the pattern in `test_snapshot_restore.rs:1-102`.

**Host side:**
1. Create builder with 1 vCPU, 512 MiB RAM
2. Call `setup_fs_builder()`
3. Add vsock port for host/guest communication
4. Run VM, wait for guest to signal "WRITTEN" (meaning scratch register is set)
5. Take full snapshot via `handle.snapshot(&snap_dir)`
6. Restore via `handle.restore_snapshot(&snap_dir)`
7. Signal guest "RESTORED"
8. Wait for guest response — should be "OK" if scratch register value survived

**Guest side:**
1. Write a known value (e.g., 0x42) to COM1 scratch register (port 0x3f8 + 7) using inline `asm!("out dx, al")`
2. Signal host "WRITTEN" via vsock
3. Wait for "RESTORED" signal from host
4. Read scratch register back (port 0x3f8 + 7) using inline `asm!("in al, dx")`
5. If value == 0x42, signal "OK"; otherwise panic

The guest accesses I/O ports with unsafe inline assembly:
```rust
unsafe fn outb(port: u16, val: u8) {
    std::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack));
}
unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    std::arch::asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack));
    val
}
```

**Register in `lib.rs`:** Add `mod test_snapshot_serial;` and a `TestCase::new("snapshot-serial-scratch", ...)` entry in `test_cases()`.

**Verification:**

Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-serial-scratch`

Expected: Test passes — scratch register value 0x42 survives snapshot/restore.

**Commit:** `test: verify serial scratch register survives snapshot/restore`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Block device data snapshot test

**Verifies:** snapshot-completeness.AC6.2

**Files:**
- Create: `tests/test_cases/src/test_snapshot_block.rs`
- Modify: `tests/test_cases/src/lib.rs` (register new test)

**Implementation:**

Create a new test file `test_snapshot_block.rs` following the existing block test patterns.

**Host side:**
1. Create builder with 1 vCPU, 512 MiB RAM
2. Call `setup_fs_builder()`
3. Add `MemBlockBackend` with known initial fill (e.g., 0xFF), get data handle
4. Add vsock port
5. Run VM, wait for guest "WRITTEN" signal (guest has written known data to block device)
6. Take full snapshot
7. Restore snapshot
8. Signal guest "RESTORED"
9. Wait for guest "OK" response

**Guest side:**
1. Open block device (e.g., `/dev/vda`)
2. Write a known pattern (e.g., "SNAPSHOT_TEST_DATA") to the first sector
3. Sync/flush
4. Signal host "WRITTEN"
5. Wait for "RESTORED"
6. Read the first sector back from `/dev/vda`
7. Verify the pattern matches; signal "OK" or panic

**Register in `lib.rs`:** Add `mod test_snapshot_block;` and `TestCase::new("snapshot-block-data", ...)`.

**Verification:**

Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-block-data`

Expected: Test passes — block device data survives snapshot/restore.

**Commit:** `test: verify block device data survives snapshot/restore`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Incremental snapshot preserves guest state

**Verifies:** snapshot-completeness.AC6.3

**Files:**
- Create: `tests/test_cases/src/test_snapshot_incremental_state.rs`
- Modify: `tests/test_cases/src/lib.rs` (register new test)

**Implementation:**

This extends the pattern from `TestSnapshotRestoreIncremental` (test_snapshot_restore.rs:103-229) but verifies more state.

**Host side:**
1. Create builder with 1 vCPU, 512 MiB RAM, block device, vsock
2. Run VM
3. Take full snapshot (baseline)
4. Enable dirty tracking
5. Signal guest to perform workload (write to block device + set memory state)
6. Wait for guest "WORKLOAD_DONE"
7. Take incremental snapshot
8. Restore incremental snapshot
9. Signal guest "RESTORED"
10. Wait for guest verification result

**Guest side:**
1. Wait for "DO_WORKLOAD" signal
2. Write known data to block device
3. Set static variable to known value (e.g., counter = 99)
4. Signal "WORKLOAD_DONE"
5. Wait for "RESTORED"
6. Verify static variable still equals 99
7. Read block device data back, verify match
8. Signal "OK" or panic

**Register in `lib.rs`:** Add `mod test_snapshot_incremental_state;` and `TestCase::new("snapshot-incremental-state", ...)`.

**Verification:**

Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-incremental-state`

Expected: Test passes — guest state survives full → workload → incremental → restore cycle.

**Commit:** `test: verify incremental snapshot preserves guest state after workload`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Network connectivity after snapshot/restore

**Verifies:** snapshot-completeness.AC6.4

**Files:**
- Create: `tests/test_cases/src/test_snapshot_net.rs`
- Modify: `tests/test_cases/src/lib.rs` (register new test)

**Implementation:**

Follow the existing network test pattern from `test_net_async_loopback.rs` combined with the snapshot restore pattern.

**Host side:**
1. Create builder with 1 vCPU, 512 MiB RAM
2. Call `setup_fs_builder()`
3. Add `LoopbackFactory` network backend with known MAC
4. Add vsock port
5. Run VM, wait for guest "NET_OK" (confirms networking works pre-snapshot)
6. Take full snapshot
7. Restore snapshot
8. Signal guest "RESTORED"
9. Wait for guest "NET_OK_AFTER_RESTORE" (confirms networking works post-restore)

**Guest side:**
1. Configure eth0 with IP 192.168.100.2/24
2. Ping 192.168.100.1 (loopback backend) — verify connectivity
3. Signal "NET_OK"
4. Wait for "RESTORED"
5. Ping 192.168.100.1 again — verify connectivity after restore
6. Signal "NET_OK_AFTER_RESTORE" or panic

The guest uses raw socket ICMP (SOCK_RAW) to send ping and verify reply, following the pattern in the existing loopback net test.

**Register in `lib.rs`:** Add `mod test_snapshot_net;` and `TestCase::new("snapshot-net-connectivity", ...)`.

**Verification:**

Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-net-connectivity`

Expected: Test passes — network ping works before and after snapshot/restore.

**Commit:** `test: verify network connectivity after snapshot/restore`
<!-- END_TASK_4 -->
