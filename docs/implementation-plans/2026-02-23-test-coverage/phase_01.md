# Test Coverage Implementation Plan — Phase 1

**Goal:** Add unit test coverage for `snapshot.rs` and `dirty_bitmap.rs`, and fix three correctness bugs exposed by those tests.

**Architecture:** Pure `#[cfg(test)]` unit tests in two files under `src/vmm/src/`. No KVM, no real VMs. Tests cover serialization roundtrips, all `SnapshotError` failure paths, `validate_header_for_vm` contract, and dirty bitmap edge cases. Three bug fixes: (1) `DirtyBitmap::mark_dirty` debug_assert replaced with silent bounds check; (2) `validate_header_for_vm` gains a `nested_enabled` comparison; (3) `create_incremental_snapshot` gains a guard that returns an error when dirty tracking was never enabled.

**Tech Stack:** Rust, bincode 1.3, vm-memory crate (`GuestMemoryMmap`, `GuestAddress`), tokio (if needed for async context in lib.rs).

**Scope:** Phase 1 of 8 phases

**Codebase verified:** 2026-02-23

---

## Acceptance Criteria Coverage

This phase implements and tests:

### test-coverage.AC1: Snapshot header serialization and validation
- **test-coverage.AC1.1 Success:** Valid `SnapshotHeader` serializes and deserializes to an identical value
- **test-coverage.AC1.2 Failure:** Wrong magic bytes → `SnapshotError::InvalidMagic`
- **test-coverage.AC1.3 Failure:** Version ≠ 1 → `SnapshotError::InvalidVersion(n)`
- **test-coverage.AC1.4 Failure:** vCPU count mismatch between header and current VM → `SnapshotError::VcpuCountMismatch`
- **test-coverage.AC1.5 Failure:** RAM region layout mismatch → `SnapshotError::MemoryLayoutMismatch`
- **test-coverage.AC1.6 Failure:** Memory file size mismatch → `SnapshotError::MemorySizeMismatch`
- **test-coverage.AC1.7 Failure:** Truncated vmstate file → `SnapshotError::Deserialize`
- **test-coverage.AC1.8 Failure:** `nested_enabled` differs between snapshot and current VM → error (new behavior; current code ignores this)
- **test-coverage.AC1.9 Failure:** `create_incremental_snapshot` called without prior `enable_dirty_tracking` → error (new behavior; current code silently produces an empty snapshot)

### test-coverage.AC2: Dirty bitmap edge cases
- **test-coverage.AC2.1 Success:** Page at exactly `num_pages - 1` (last valid index) is tracked and returned by `drain_dirty_pages`
- **test-coverage.AC2.2 Edge:** Page at `num_pages` (out-of-bounds) is silently ignored; no panic
- **test-coverage.AC2.3 Edge:** All pages marked dirty → `drain_dirty_pages` returns the full set
- **test-coverage.AC2.4 Edge:** No pages marked → `drain_dirty_pages` returns empty vec
- **test-coverage.AC2.5 Edge:** Same page marked dirty twice → appears exactly once in drain output

---

## Codebase Findings (Phase 1 Investigation)

### snapshot.rs (`src/vmm/src/snapshot.rs`)

**SnapshotHeader** (lines 112–119):
```rust
pub struct SnapshotHeader {
    pub magic: u32,       // 0x4B52_534E ("KRSN")
    pub version: u32,     // SNAPSHOT_VERSION = 1
    pub vcpu_count: u32,
    pub ram_regions: Vec<(u64, u64)>,
    pub nested_enabled: bool,
}
```

**SnapshotError** (lines 24–41) — variants with structured data:
- `Io(io::Error)`
- `Serialize(String)` / `Deserialize(String)`
- `InvalidMagic`
- `InvalidVersion(u32)`
- `MemorySizeMismatch { expected: u64, got: u64 }`
- `MemoryLayoutMismatch { expected: Vec<(u64, u64)>, got: Vec<(u64, u64)> }`
- `VcpuCountMismatch { expected: usize, got: usize }`

**`validate_header_for_vm`** (lines 84–107):
```rust
pub fn validate_header_for_vm(
    header: &SnapshotHeader,
    guest_memory: &GuestMemoryMmap,
    expected_vcpu_count: usize,
) -> Result<(), SnapshotError>
```
Currently checks: magic, version, ram_regions, vcpu_count. **Does NOT check `nested_enabled`** (bug).

**Serialization**: `save_vmstate()` / `load_vmstate()` / `save_incremental_snapshot()` / `load_incremental_snapshot()` all use `bincode`. No tests exist in this file today.

### dirty_bitmap.rs (`src/vmm/src/dirty_bitmap.rs`)

**DirtyBitmap** (lines 18–25):
```rust
pub struct DirtyBitmap {
    base_addr: u64,
    num_pages: usize,
    bitmap: Vec<AtomicU64>,
}
```
- `new(base_addr: u64, size: u64)` — `num_pages = size / PAGE_SIZE`
- `mark_dirty(&self, guest_addr: u64)` — **uses `debug_assert!(self.contains(guest_addr))`** (panics in debug mode on out-of-bounds; UB in release)
- `drain_dirty_pages(&self) -> Vec<u64>` — returns guest addresses, resets bitmap. Already has `if page_idx < self.num_pages` guard internally (line 98).
- Two existing tests: `test_basic_dirty_tracking`, `test_drain_dirty_pages`

**Important**: The ACs use "page index" language but `mark_dirty` accepts `guest_addr`. Conversion: last valid page address = `base_addr + (num_pages - 1) * PAGE_SIZE`; out-of-bounds = `base_addr + num_pages * PAGE_SIZE`.

### lib.rs (`src/vmm/src/lib.rs`)

- `enable_dirty_tracking(&mut self) -> Result<()>`: macOS lines 714–756, Linux lines 925–944. Populates `self.dirty_bitmaps`.
- `create_incremental_snapshot(...)`: macOS lines 781–858, Linux lines 997–1044. Calls `bitmap.drain_dirty_pages()` over `self.dirty_bitmaps` — **no guard** for whether tracking was enabled (bug).
- Guard approach: check `self.dirty_bitmaps.is_empty()` at the start of `create_incremental_snapshot` and return a new `SnapshotError` variant (e.g., `DirtyTrackingNotEnabled`).

---

## Tasks

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Fix `DirtyBitmap::mark_dirty` — replace `debug_assert!` with silent bounds check

**Verifies:** test-coverage.AC2.2 (prerequisite — test would panic without this fix)

**Files:**
- Modify: `src/vmm/src/dirty_bitmap.rs` (around line 50, `mark_dirty` function body)

**Implementation:**

Replace the `debug_assert!(self.contains(guest_addr))` line with a runtime bounds check that returns early when out of bounds. Keep the existing atomics-based dirty marking logic.

Before (current):
```rust
pub fn mark_dirty(&self, guest_addr: u64) {
    debug_assert!(self.contains(guest_addr));
    // ... compute bit position and set it atomically
}
```

After (fixed):
```rust
pub fn mark_dirty(&self, guest_addr: u64) {
    if !self.contains(guest_addr) {
        return;
    }
    // ... compute bit position and set it atomically
}
```

The `contains` method (already present) checks whether `guest_addr` falls within `[base_addr, base_addr + num_pages * PAGE_SIZE)`. The task-implementor should verify the exact method name and use it consistently.

**Verification:**

Run: `cargo test -p vmm`
Expected: Existing `test_basic_dirty_tracking` and `test_drain_dirty_pages` tests still pass; no regressions.

**Commit:** `fix(dirty_bitmap): replace debug_assert with silent bounds check in mark_dirty`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add dirty bitmap unit tests (AC2.1–AC2.5)

**Verifies:** test-coverage.AC2.1, test-coverage.AC2.2, test-coverage.AC2.3, test-coverage.AC2.4, test-coverage.AC2.5

**Platform note:** `dirty_bitmap.rs` is gated on `#[cfg(target_os = "macos")]` in `src/vmm/src/lib.rs` (lines 20–21). The `dirty_bitmaps` field on `Vmm` is similarly macOS-only (line 218–219). On Linux, this module does not exist and the AC2.x tests will not compile or run. These tests are macOS-only by design, reflecting the architecture where dirty tracking uses platform-specific DirtyBitmap on macOS and KVM ioctl on Linux.

**Files:**
- Modify: `src/vmm/src/dirty_bitmap.rs` — extend the existing `#[cfg(test)] mod tests` block

**Implementation:**

Add the following test functions to the existing test module. `DirtyBitmap::new` takes `(base_addr: u64, size: u64)`. Use the module's `PAGE_SIZE` constant (import with `use super::PAGE_SIZE`; currently `16384` on Apple Silicon). With `size = N * PAGE_SIZE`, the bitmap covers pages 0..N with `num_pages = N`.

In the test code, import the constant: `use super::PAGE_SIZE;`. Do not hardcode `4096` or `16384` — always use `PAGE_SIZE` so the tests are correct on any Apple Silicon page size.

- **AC2.1 — last valid page:** Create a bitmap with 4 pages (`size = 4 * PAGE_SIZE`). Call `mark_dirty(base_addr + 3 * PAGE_SIZE)` (last valid index). Call `drain_dirty_pages()`. Assert the returned vec contains the address `base_addr + 3 * PAGE_SIZE` (or equivalent page address). The test-implementor should verify what address value `drain_dirty_pages` actually returns (the base of the page frame, not necessarily the exact byte address passed in).

- **AC2.2 — out-of-bounds ignored:** Create a bitmap with 4 pages. Call `mark_dirty(base_addr + 4 * PAGE_SIZE)` (one past the end). Assert no panic. Call `drain_dirty_pages()`. Assert the returned vec is empty.

- **AC2.3 — all pages dirty:** Create a bitmap with 4 pages. Call `mark_dirty` for all 4 page addresses (`base_addr + 0`, `base_addr + PAGE_SIZE`, `base_addr + 2*PAGE_SIZE`, `base_addr + 3*PAGE_SIZE`). Call `drain_dirty_pages()`. Assert returned vec has exactly 4 elements.

- **AC2.4 — no pages marked:** Create a bitmap with 4 pages. Call `drain_dirty_pages()` without any `mark_dirty` calls. Assert returned vec is empty.

- **AC2.5 — deduplication:** Create a bitmap with 4 pages. Call `mark_dirty(base_addr + PAGE_SIZE)` twice. Call `drain_dirty_pages()`. Assert returned vec has exactly 1 element (the atomics-based bitmap naturally deduplicates).

Look at the existing `test_basic_dirty_tracking` and `test_drain_dirty_pages` for the correct import paths and `DirtyBitmap::new` call pattern.

**Verification:**

Run (macOS only): `cargo test -p vmm dirty_bitmap`
Expected: All 5 new tests pass alongside the 2 existing tests. This command is a no-op on Linux since the module is macOS-only.

**Commit:** `test(dirty_bitmap): add edge case tests for AC2.1-AC2.5`
<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-7) -->

<!-- START_TASK_3 -->
### Task 3: Fix `validate_header_for_vm` — add `nested_enabled` check

**Verifies:** test-coverage.AC1.8 (prerequisite — no error is returned without this fix)

**Files:**
- Modify: `src/vmm/src/snapshot.rs` — `validate_header_for_vm` function (lines 84–107) and its call sites

**Implementation:**

The current function signature is:
```rust
pub fn validate_header_for_vm(
    header: &SnapshotHeader,
    guest_memory: &GuestMemoryMmap,
    expected_vcpu_count: usize,
) -> Result<(), SnapshotError>
```

Add an `expected_nested_enabled: bool` parameter. Inside the function body, after the existing checks, add:

```rust
if header.nested_enabled != expected_nested_enabled {
    return Err(SnapshotError::NestedEnabledMismatch);
}
```

Also add `NestedEnabledMismatch` to the `SnapshotError` enum (no fields needed).

**Threading `nested_enabled` into `Vmm`:** The `Vmm` struct does NOT currently store `nested_enabled` — it lives in `VmResources` (`src/vmm/src/resources.rs` line 196). Three snapshot calls in `src/vmm/src/lib.rs` (lines 640, 848, 1035) currently hardcode `nested_enabled: false` with a `TODO` comment at line 640.

To fix this:
1. Add a `nested_enabled: bool` field to the `Vmm` struct (`src/vmm/src/lib.rs`).
2. Populate it during Vmm construction — search for where `Vmm` is instantiated in `src/vmm/src/lib.rs` and pass the value from `VmResources.nested_enabled`.
3. Replace the three hardcoded `nested_enabled: false` occurrences at lines 640, 848, 1035 with `self.nested_enabled`.
4. Update all 4 call sites of `validate_header_for_vm` (lines 446, 651, 867, 1054 in `src/vmm/src/lib.rs`) to pass `self.nested_enabled` as the new 4th argument.

**Verification:**

Run: `cargo build -p vmm --features snapshot`
Expected: Builds without errors (all call sites updated).

**Commit:** `fix(snapshot): validate nested_enabled field in validate_header_for_vm`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Fix `create_incremental_snapshot` — guard for dirty tracking not enabled

**Verifies:** test-coverage.AC1.9 (prerequisite — no error is returned without this fix)

**Files:**
- Modify: `src/vmm/src/snapshot.rs` — add `DirtyTrackingNotEnabled` variant to `SnapshotError`
- Modify: `src/vmm/src/lib.rs` — `create_incremental_snapshot` function (macOS lines 781–858, Linux lines 997–1044)

**Implementation:**

Add to `SnapshotError`:
```rust
DirtyTrackingNotEnabled,
```

**Platform difference for dirty tracking:** `dirty_bitmaps: Vec<DirtyBitmap>` is a macOS-only field on `Vmm` (conditionally compiled). On Linux, `enable_dirty_tracking()` sets `KVM_MEM_LOG_DIRTY_PAGES` flags directly on KVM memory slots — there is no equivalent `dirty_bitmaps` field. Checking `self.dirty_bitmaps.is_empty()` only works on macOS.

**Cross-platform fix:** Add a `dirty_tracking_enabled: bool` field to `Vmm` (with no platform gate — both platforms get it):
```rust
struct Vmm {
    // ... existing fields ...
    dirty_tracking_enabled: bool,  // set to true by enable_dirty_tracking()
}
```
Initialize it to `false` in the Vmm constructor. In `enable_dirty_tracking()` (both the macOS path around line 714 and the Linux path around line 925), add `self.dirty_tracking_enabled = true;` at the end of the function body.

At the top of `create_incremental_snapshot` (before dirty page collection, on both platforms), add:
```rust
if !self.dirty_tracking_enabled {
    return Err(SnapshotError::DirtyTrackingNotEnabled);
}
```

This single cross-platform field replaces the macOS-only `dirty_bitmaps.is_empty()` check.

**Verification:**

Run: `cargo build -p vmm --features snapshot`
Expected: Builds without errors on both Linux and macOS feature paths.

**Commit:** `fix(vmm): create_incremental_snapshot returns error if dirty tracking not enabled`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Add snapshot.rs unit tests (AC1.1–AC1.8)

**Verifies:** test-coverage.AC1.1, test-coverage.AC1.2, test-coverage.AC1.3, test-coverage.AC1.4, test-coverage.AC1.5, test-coverage.AC1.6, test-coverage.AC1.7, test-coverage.AC1.8

**Files:**
- Modify: `src/vmm/src/snapshot.rs` — add `#[cfg(test)] mod tests { ... }` at the bottom

**Implementation:**

Create a test module with the following functions. Each test constructs the inputs it needs directly without mocking external subsystems.

**Test helper — `make_memory(regions: &[(u64, u64)]) -> GuestMemoryMmap`**:
Build a `GuestMemoryMmap` from a list of `(guest_addr_start, size_bytes)` pairs. Look at the existing vm-memory usage in the codebase for the correct import (`vm_memory::GuestMemoryMmap`, `vm_memory::GuestAddress`). Typically:
```rust
GuestMemoryMmap::from_ranges(&[(GuestAddress(start), size)]).unwrap()
```

**Test helper — `valid_header(mem: &GuestMemoryMmap, vcpu_count: u32, nested: bool) -> SnapshotHeader`**:
Build a `SnapshotHeader` that matches the given memory's regions:
```rust
SnapshotHeader {
    magic: 0x4B52_534E,
    version: 1,
    vcpu_count,
    ram_regions: mem.iter().map(|r| (r.start_addr().0, r.len())).collect(),
    nested_enabled: nested,
}
```

- **AC1.1 — roundtrip:** Call `save_vmstate` writing to a `tempfile::NamedTempFile` (or `std::io::Cursor`). Read back with `load_vmstate`. Assert the loaded struct equals the original. (Investigate how `save_vmstate`/`load_vmstate` are called in the codebase — they may take a path or a writer. Adapt accordingly.)

- **AC1.2 — wrong magic:** Build a valid header, then set `magic = 0xDEAD_BEEF`. Serialize it to bytes using `bincode::serialize`. Prepend those bytes to a temp file and call the load/validate path. Assert `Err(SnapshotError::InvalidMagic)`. *Alternative if serialization isn't easily separable*: call `validate_header_for_vm` with a header that has `magic != 0x4B52_534E` — check whether the function validates the magic or whether that's done on deserialization. Adapt to match the actual code path.

- **AC1.3 — wrong version:** Same approach with `version = 99`. Assert `Err(SnapshotError::InvalidVersion(99))`.

- **AC1.4 — vCPU count mismatch:** Create memory + matching header with `vcpu_count = 2`. Call `validate_header_for_vm` with `expected_vcpu_count = 4`. Assert `Err(SnapshotError::VcpuCountMismatch { expected: 4, got: 2 })`.

- **AC1.5 — layout mismatch:** Create a header whose `ram_regions` list differs from the actual memory (e.g., header has one region, memory has two). Call `validate_header_for_vm`. Assert `Err(SnapshotError::MemoryLayoutMismatch { .. })`.

- **AC1.6 — size mismatch:** Create a header whose `ram_regions` sizes differ from the memory (e.g., header says region is 2MB, memory is 1MB). Call `validate_header_for_vm`. Assert `Err(SnapshotError::MemorySizeMismatch { .. })`.

- **AC1.7 — truncated file:** Write a few bytes to a temp file (less than a valid vmstate). Call `load_vmstate` on it. Assert `Err(SnapshotError::Deserialize(_))`.

- **AC1.8 — nested_enabled mismatch:** Create a valid header with `nested_enabled = true`. Call `validate_header_for_vm` with `expected_nested_enabled = false`. Assert `Err(SnapshotError::NestedEnabledMismatch)`.

The task-implementor must look up the actual call signatures for `save_vmstate`/`load_vmstate`/`validate_header_for_vm` in `snapshot.rs` and adapt the tests accordingly. Do not assume specific call patterns.

**Verification:**

Run: `cargo test -p vmm --features snapshot snapshot`
Expected: All 8 tests pass.

**Commit:** `test(snapshot): add unit tests for AC1.1-AC1.8`
<!-- END_TASK_5 -->

<!-- START_TASK_6 -->
### Task 6: Add AC1.9 unit test in lib.rs

**Verifies:** test-coverage.AC1.9

**Files:**
- Modify: `src/vmm/src/lib.rs` — add or extend `#[cfg(test)] mod tests { ... }`

**Implementation:**

AC1.9 tests `create_incremental_snapshot` returning an error when called without `enable_dirty_tracking`. The `Vmm` struct may require significant setup (memory, KVM) to construct. The task-implementor must investigate the `Vmm::new` or equivalent constructor in `lib.rs` and find the minimal setup path.

If constructing a minimal `Vmm` in a unit test proves impossible without KVM (e.g., the constructor opens `/dev/kvm`), use the following fallback: add a small private helper function that encapsulates the guard check, and test that function directly. The guard now uses the cross-platform `dirty_tracking_enabled: bool` field added in Task 4:

```rust
// In lib.rs (outside #[cfg(test)]):
fn check_dirty_tracking_enabled(dirty_tracking_enabled: bool) -> Result<(), SnapshotError> {
    if !dirty_tracking_enabled {
        return Err(SnapshotError::DirtyTrackingNotEnabled);
    }
    Ok(())
}
```

Then in the test:
```rust
#[test]
fn test_incremental_snapshot_requires_dirty_tracking() {
    // Before enable_dirty_tracking(): error
    let result = check_dirty_tracking_enabled(false);
    assert!(matches!(result, Err(SnapshotError::DirtyTrackingNotEnabled)));

    // After enable_dirty_tracking(): no error
    let result = check_dirty_tracking_enabled(true);
    assert!(result.is_ok());
}
```

If constructing a `Vmm` for a unit test IS feasible (e.g., there's a test helper or a way to build without KVM), prefer testing `create_incremental_snapshot` directly.

**Verification:**

Run: `cargo test -p vmm --features snapshot`
Expected: New test passes. All previous tests still pass.

**Commit:** `test(vmm): add unit test for AC1.9 incremental snapshot guard`
<!-- END_TASK_6 -->

<!-- START_TASK_7 -->
### Task 7: Run full vmm test suite and verify

**Files:** None

**Step 1: Run the full vmm test suite**

Run: `cargo test -p vmm --features snapshot`
Expected: All tests pass. Look for these specific test names:
- `dirty_bitmap::tests::test_last_valid_page` (or similar name for AC2.1)
- `dirty_bitmap::tests::test_out_of_bounds_ignored` (AC2.2)
- `dirty_bitmap::tests::test_all_pages_dirty` (AC2.3)
- `dirty_bitmap::tests::test_no_pages_dirty` (AC2.4)
- `dirty_bitmap::tests::test_duplicate_mark` (AC2.5)
- `snapshot::tests::test_header_roundtrip` (AC1.1)
- `snapshot::tests::test_invalid_magic` (AC1.2)
- `snapshot::tests::test_invalid_version` (AC1.3)
- `snapshot::tests::test_vcpu_count_mismatch` (AC1.4)
- `snapshot::tests::test_layout_mismatch` (AC1.5)
- `snapshot::tests::test_size_mismatch` (AC1.6)
- `snapshot::tests::test_truncated_file` (AC1.7)
- `snapshot::tests::test_nested_enabled_mismatch` (AC1.8)
- Test for AC1.9 (name chosen by implementor)

If any test fails, fix it before committing.

**Step 2: Commit if needed**

If all tests pass and nothing was left uncommitted: done.
<!-- END_TASK_7 -->

<!-- END_SUBCOMPONENT_B -->
