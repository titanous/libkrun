# VMGENID Implementation Plan — Phase 6: Integration Test and Cleanup

**Goal:** Verify the full VMGENID flow end-to-end with the existing snapshot-rng-reseed integration test (ported from the `rng-snapshot-reseed` branch) and confirm cleanup ACs are satisfied.

**Architecture:** The `test_snapshot_rng_reseed.rs` integration test already exists on the `rng-snapshot-reseed` branch (150 lines). It uses vsock-based host/guest communication to snapshot a VM, restore it twice from the same snapshot, and verify the two restores produce different `/dev/urandom` output. This test validates the OUTCOME (entropy divergence) regardless of the mechanism (VMGENID replaces the earlier `on_restore_complete()` approach). Port this test to the vmgenid branch with updated comments reflecting the VMGENID mechanism. The cleanup items from the design plan (removing `on_restore_complete()`) are already done — this method does not exist on the base branch.

**Tech Stack:** Rust, integration test framework (host/guest proc macros), vsock, snapshot feature

**Scope:** 6 phases from original design (phase 6 of 6)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vmgenid.AC4: Snapshot restore triggers reseed
- **vmgenid.AC4.4 Success:** Two VMs restored from the same snapshot produce different 32-byte `/dev/urandom` output and different GUIDs

### vmgenid.AC6: Cleanup
- **vmgenid.AC6.1 Success:** `on_restore_complete()` method removed from `VirtioDevice` trait
- **vmgenid.AC6.2 Success:** No snapshot-specific additions remain in the Rng device that were added by the rng worktree

**Note on AC6.1 and AC6.2:** The `on_restore_complete()` method does NOT exist on the base branch. These ACs are already satisfied — there is nothing to remove. The investigation confirmed: `on_restore_complete` only appears in design docs, not in any `.rs` file.

---

## Reference Files

- **Existing test to port:** `rng-snapshot-reseed` branch, `tests/test_cases/src/test_snapshot_rng_reseed.rs` — 150 lines, vsock-based dual-restore entropy comparison test
- **Test registry:** `tests/test_cases/src/lib.rs:100-170` — `test_cases()` function where tests are registered via `TestCase::new()`
- **Test helpers:** `tests/test_cases/src/krun_rust.rs` — `setup_fs_builder()` for VM creation
- **Mock snapshot store:** `tests/test_cases/src/mock_snapshot_store.rs` — test snapshot store implementations

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->

<!-- START_TASK_1 -->
### Task 1: Port snapshot-rng-reseed integration test from rng branch

**Verifies:** vmgenid.AC4.4

**Files:**
- Create: `tests/test_cases/src/test_snapshot_rng_reseed.rs` (ported from `rng-snapshot-reseed` branch)
- Modify: `tests/test_cases/src/lib.rs` — register the test

**Implementation:**

Cherry-pick or copy `test_snapshot_rng_reseed.rs` from the `rng-snapshot-reseed` branch:

```bash
git show rng-snapshot-reseed:tests/test_cases/src/test_snapshot_rng_reseed.rs > tests/test_cases/src/test_snapshot_rng_reseed.rs
```

Then update the file comments to reflect the VMGENID mechanism:
- Remove references to `on_restore_complete()` in the Rng device
- Update the module doc comment to explain that VMGENID triggers the kernel CSPRNG reseed via platform interrupt (GED on x86_64, SPI on aarch64), not via a virtio device hook
- Keep the test logic unchanged — it validates the outcome (different entropy from `/dev/urandom`), which is the same regardless of mechanism

The test structure (from the rng-snapshot-reseed branch):
- **Guest side:** Connects via vsock, sends "READY", waits for "READ" command, responds with 32 bytes from `/dev/urandom`, accepts "DONE" to exit
- **Host side:** Binds vsock listener before VM start, accepts connection, waits for "READY", takes snapshot with guest idle, restores twice from same snapshot, sends "READ" after each restore, compares the two 32-byte entropy samples
- **Key design:** vsock listener bound BEFORE VM starts, same vsock proxy/stream persists across restore cycles

In `tests/test_cases/src/lib.rs`, register the test (if not already registered):
```rust
TestCase::new("snapshot-rng-reseed", Box::new(test_snapshot_rng_reseed::TestSnapshotRngReseed)),
```

The test should be gated on the `snapshot` feature.

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && make test FEATURE_FLAGS="--features embedded_init,snapshot"`
Expected: The `snapshot-rng-reseed` test passes. Two restores produce different entropy output via VMGENID mechanism.

**Commit:** `test: port snapshot-rng-reseed integration test from rng branch (validates VMGENID entropy divergence)`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Verify AC6.1 and AC6.2 (cleanup already done)

**Verifies:** vmgenid.AC6.1, vmgenid.AC6.2

**Files:** None (verification only)

**Verification:**

Confirm that `on_restore_complete` does not exist anywhere in the source code:

Run: `grep -r "on_restore_complete" src/`
Expected: No matches. (Only design docs in `docs/` should match, not source code.)

Confirm no snapshot-specific Rng additions from the "rng worktree" exist:

Run: `grep -r "on_restore_complete\|restore_rng\|reseed" src/devices/src/virtio/rng/`
Expected: No matches related to snapshot restore hooks in the Rng device.

These ACs are pre-satisfied on the base branch.

**Commit:** No commit (verification only).

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Full test suite verification

**Files:** None (verification only)

**Verification:**

Run the full integration test suite:

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && make test FEATURE_FLAGS="--features embedded_init,snapshot"`
Expected: All tests pass (5-6/6 expected due to known flakiness in vsock/tsi tests). The snapshot-rng-reseed test should pass consistently.

Run unit tests:
Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo test -p devices -- vmgenid`
Expected: All vmgenid unit tests pass.

**Commit:** No commit (verification only).

<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->
