# Snapshot Completeness Implementation Plan — Phase 6

**Goal:** Prevent OOM from corrupted vmstate files by adding a 10MB deserialization size limit.

**Architecture:** Add a file size check before `file.read_to_end()` in both `load_vmstate()` and `load_incremental_snapshot()`. If the file exceeds 10MB, return a `SnapshotError` immediately without allocating memory. Add a new error variant for the size limit.

**Tech Stack:** Rust (vmm crate, snapshot feature)

**Scope:** 7 phases from original design (this is phase 6 of 7)

**Codebase verified:** 2026-02-24

---

## Acceptance Criteria Coverage

This phase implements and tests:

### snapshot-completeness.AC5: Deserialization size limit
- **snapshot-completeness.AC5.1 Success:** vmstate file under 10MB loads normally
- **snapshot-completeness.AC5.2 Failure:** vmstate file over 10MB returns SnapshotError without allocating unbounded memory
- **snapshot-completeness.AC5.3 Success:** Incremental snapshot file under 10MB loads normally
- **snapshot-completeness.AC5.4 Failure:** Incremental snapshot file over 10MB returns SnapshotError

---

<!-- START_TASK_1 -->
### Task 1: Add size limit to load_vmstate and load_incremental_snapshot

**Verifies:** snapshot-completeness.AC5.1, snapshot-completeness.AC5.2, snapshot-completeness.AC5.3, snapshot-completeness.AC5.4

**Files:**
- Modify: `src/vmm/src/snapshot.rs:23-44` (SnapshotError enum)
- Modify: `src/vmm/src/snapshot.rs:216-230` (load_vmstate)
- Modify: `src/vmm/src/snapshot.rs:319-333` (load_incremental_snapshot)

**Implementation:**

1. Add a constant for the size limit:

```rust
const VMSTATE_MAX_SIZE: u64 = 10 * 1024 * 1024; // 10MB
```

2. Add a `FileSizeExceeded` variant to `SnapshotError` after `DirtyTrackingNotEnabled`:

```rust
FileSizeExceeded { size: u64, limit: u64 },
```

3. Add `Display` arm for the new variant (in the existing `fmt::Display` impl):

```rust
SnapshotError::FileSizeExceeded { size, limit } => {
    write!(f, "Snapshot file size ({size} bytes) exceeds limit ({limit} bytes)")
}
```

4. In `load_vmstate()`, add size check before `file.read_to_end()` (between line 217 `File::open` and line 218 `Vec::new()`):

```rust
let file_size = file.metadata()?.len();
if file_size > VMSTATE_MAX_SIZE {
    return Err(SnapshotError::FileSizeExceeded {
        size: file_size,
        limit: VMSTATE_MAX_SIZE,
    });
}
```

5. Apply the same check to `load_incremental_snapshot()` (between line 320 `File::open` and line 321 `Vec::new()`).

**Verification:**

Build: `cargo build -p vmm --features snapshot`

**Commit:** Do not commit yet — continue to Task 2.
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Unit tests for size limit

**Verifies:** snapshot-completeness.AC5.2, snapshot-completeness.AC5.4

**Files:**
- Modify: `src/vmm/src/snapshot.rs` (add to existing `#[cfg(test)]` module)

**Implementation:**

Add tests to the existing snapshot test module (starting at line 354):

**Testing:**

Tests must verify:
- snapshot-completeness.AC5.2: Create a temp file larger than 10MB (e.g. 11MB of zeros). Call `load_vmstate()`. Assert it returns `Err(SnapshotError::FileSizeExceeded { .. })`.
- snapshot-completeness.AC5.4: Create a temp file larger than 10MB. Call `load_incremental_snapshot()`. Assert same error.
- Also verify a valid-sized file (created by normal snapshot) still loads successfully (AC5.1, AC5.3 — covered by existing round-trip tests).

Run: `cargo test -p vmm --features snapshot`

Expected: All tests pass, including existing snapshot tests.

**Commit:** `feat: add 10MB deserialization size limit for vmstate files`
<!-- END_TASK_2 -->
