# Memory Balloon Device Implementation Plan — Phase 5: UFFD Zero-Fill

**Goal:** Efficiently resolve page faults for reclaimed pages during UFFD restore using zeropage ioctl.

**Architecture:** Update `UffdHandler` fault handler to detect `None` from `store.read_page()` (reclaimed pages absent from store) and resolve via `uffd.zeropage()` instead of `uffd.copy()`. Add `LoadSource::Zero` variant for stats tracking. The preload path naturally skips absent pages (they aren't in the store), so no preload changes needed.

**Tech Stack:** Rust, userfaultfd crate 0.8.1, tokio

**Scope:** 7 phases from original design (phase 5 of 7)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### mem-balloon.AC3: UFFD restore zero-fills reclaimed pages
- **mem-balloon.AC3.1 Success:** Page fault for a reclaimed page resolved via `uffd.zeropage()` — guest sees zeros
- **mem-balloon.AC3.2 Success:** PageTracker records zero-filled pages under `LoadSource::Zero`
- **mem-balloon.AC3.3 Failure:** Page fault for a present page (not reclaimed) still reads from store and uses `uffd.copy()` — no regression
- **mem-balloon.AC3.4 Edge:** `zeropage` EEXIST (race with preload) handled identically to existing `copy` EEXIST (silently ignored)

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->
<!-- START_TASK_1 -->
### Task 1: Update SnapshotStore::read_page callers for Option return type

**Verifies:** None (prerequisite for AC3.1, AC3.3 — adapts fault handler to Phase 4's trait change)

**Files:**
- Modify: `src/vmm/src/uffd.rs:336-377` (fault handler in `fault_loop` — update `store_clone.read_page(guest_addr)` to handle `Option<Vec<u8>>`)
- Modify: `src/vmm/src/uffd.rs:596-603` (MockSnapshotStore `read_page` — update return type to `Option<Vec<u8>>`)

**Implementation:**

Phase 4 Task 2 changed `SnapshotStore::read_page` to return `io::Result<Option<Vec<u8>>>`. This task updates the fault handler and test mock to compile with the new signature.

In the fault handler at line 337, the current code is:
```rust
match store_clone.read_page(guest_addr).await {
    Ok(data) => {
        // ... uffd.copy with data ...
    }
    Err(e) => { ... }
}
```

Update to handle `Option`:
```rust
match store_clone.read_page(guest_addr).await {
    Ok(Some(data)) => {
        // Existing uffd.copy path — unchanged
        let result = unsafe {
            uffd_clone.copy(
                data.as_ptr() as *const _,
                host_addr as *mut _,
                data.len(),
                true,
            )
        };
        // ... existing match on result ...
    }
    Ok(None) => {
        // Reclaimed page — resolve with zeropage (Task 2)
        // For now, fall through to avoid compilation error.
        // Task 2 will add the zeropage call here.
        log::warn!("read_page returned None for 0x{guest_addr:x}, zeropage not yet implemented");
    }
    Err(e) => {
        // Existing error path — unchanged
        signal_error(&vm_exit, format!("demand page read failed at 0x{guest_addr:x}: {e}"));
    }
}
```

Update `MockSnapshotStore::read_page` to return `Ok(Some(...))`:
```rust
fn read_page(&self, _guest_addr: u64) -> SendBoxFuture<'_, std::io::Result<Option<Vec<u8>>>> {
    self.page_reads.fetch_add(1, Ordering::SeqCst);
    Box::pin(async { Ok(Some(vec![0u8; 4096])) })
}
```

**Verification:**
Run: `cargo check -p vmm --features uffd`
Expected: Compiles without errors

**Commit:** `feat(uffd): adapt fault handler for Option<Vec<u8>> read_page return type`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add LoadSource::Zero variant and zeropage fault resolution

**Verifies:** mem-balloon.AC3.1, mem-balloon.AC3.2, mem-balloon.AC3.3, mem-balloon.AC3.4

**Files:**
- Modify: `src/vmm/src/uffd.rs:418-425` (add `Zero` variant to `LoadSource` enum)
- Modify: `src/vmm/src/uffd.rs:427-442` (add `zero_pages` field to `PageTrackerStats`)
- Modify: `src/vmm/src/uffd.rs:450-461` (add `zero_count` to `PageTracker`)
- Modify: `src/vmm/src/uffd.rs:485-507` (update `mark_loaded` match to handle `Zero`)
- Modify: `src/vmm/src/uffd.rs:531-557` (update `stats()` to include `zero_pages`)
- Modify: `src/vmm/src/uffd.rs` (fault handler `Ok(None)` arm — add `uffd.zeropage()` call)

**Implementation:**

Add `Zero` variant to `LoadSource`:
```rust
pub enum LoadSource {
    Preload,
    Fault,
    Zero,
}
```

Add field to `PageTrackerStats`:
```rust
pub struct PageTrackerStats {
    pub total_pages: usize,
    pub loaded_pages: usize,
    pub preload_pages: usize,
    pub fault_pages: usize,
    pub zero_pages: usize,
    pub total_faults: usize,
    pub progress_pct: f64,
}
```

Add counter to `PageTracker`:
```rust
zero_count: AtomicUsize,
```
Initialize to `AtomicUsize::new(0)` in `PageTracker::new()`.

Update `mark_loaded` match arm:
```rust
LoadSource::Zero => {
    self.zero_count.fetch_add(1, Ordering::Relaxed);
}
```

Update `stats()` to include `zero_pages: self.zero_count.load(Ordering::Relaxed)`.

In the fault handler, replace the `Ok(None)` placeholder from Task 1 with the zeropage call:
```rust
Ok(None) => {
    // Reclaimed page — resolve via zeropage ioctl.
    // Maps the kernel shared zero page — no data copy, no physical allocation.
    let result = unsafe {
        uffd_clone.zeropage(host_addr as *mut _, 4096, true)
    };
    match result {
        Ok(_) => {
            if let Some(page_index) =
                guest_addr_to_page_index(&regions_clone, guest_addr)
            {
                tracker_clone.mark_loaded(page_index, LoadSource::Zero);
            }
        }
        Err(e) => {
            if !is_eexist(&e) {
                signal_error(
                    &vm_exit,
                    format!("uffd zeropage failed: {e:?}"),
                );
            }
            // Silently ignore EEXIST — race with preload or another fault (AC3.4)
        }
    }
}
```

The `is_eexist` helper at line 405-407 currently only checks for `userfaultfd::Error::CopyFailed`. The `zeropage()` method returns `userfaultfd::Error::ZeropageFailed(Errno)` on failure (confirmed from userfaultfd 0.8.1 source at `src/error.rs:47`; both variants wrap `nix::errno::Errno`). Update `is_eexist` to handle both:
```rust
fn is_eexist(e: &userfaultfd::Error) -> bool {
    match e {
        userfaultfd::Error::CopyFailed(errno) if *errno as i32 == libc::EEXIST => true,
        userfaultfd::Error::ZeropageFailed(errno) if *errno as i32 == libc::EEXIST => true,
        _ => false,
    }
}
```

**Testing:**
Tests must verify:
- mem-balloon.AC3.1: MockSnapshotStore returns `Ok(None)` for specific pages; fault handler calls `uffd.zeropage()` and page resolves successfully
- mem-balloon.AC3.2: After zero-fill resolution, PageTracker has the page marked as loaded, `stats().zero_pages > 0`
- mem-balloon.AC3.3: MockSnapshotStore returns `Ok(Some(data))` for present pages; fault handler uses `uffd.copy()` as before (no regression)
- mem-balloon.AC3.4: If `uffd.zeropage()` returns EEXIST, it is silently ignored (no signal_error call)

Note: Full end-to-end UFFD tests require a registered memory region and real userfaultfd. Unit tests for PageTracker stats can verify the counter logic directly. The fault handler flow can be tested via the existing mock infrastructure if mocks support `None` return.

**Verification:**
Run: `cargo check -p vmm --features uffd`
Expected: Compiles without errors

**Commit:** `feat(uffd): resolve reclaimed page faults via zeropage with LoadSource::Zero tracking`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Add PageTracker unit tests for LoadSource::Zero

**Verifies:** mem-balloon.AC3.2

**Files:**
- Modify: `src/vmm/src/uffd.rs` (add tests to existing `#[cfg(test)] mod tests` block at line 560)

**Implementation:**

Add unit tests to the existing test module:

Test `page_tracker_zero_source`:
- Create `PageTracker::new(128)`
- Call `mark_loaded(0, LoadSource::Zero)` — marks page 0 as zero-filled
- Call `mark_loaded(1, LoadSource::Fault)` — marks page 1 as faulted
- Call `mark_loaded(2, LoadSource::Preload)` — marks page 2 as preloaded
- Assert `tracker.is_loaded(0)` is true
- Assert `tracker.stats().zero_pages == 1`
- Assert `tracker.stats().fault_pages == 1`
- Assert `tracker.stats().preload_pages == 1`
- Assert `tracker.stats().loaded_pages == 3`

Test `page_tracker_zero_duplicate_ignored`:
- Create `PageTracker::new(64)`
- Call `mark_loaded(5, LoadSource::Zero)` twice
- Assert `tracker.stats().zero_pages == 1` (duplicate does not double-count)

Test `mock_store_returns_none`:
- Create a `MockSnapshotStore` variant that returns `Ok(None)` for specific guest addresses (add a `HashSet<u64>` of absent pages to MockSnapshotStore, or create a `MockSnapshotStoreWithAbsent` struct)
- Call `read_page` on an absent address, assert it returns `Ok(None)`
- Call `read_page` on a present address, assert it returns `Ok(Some(...))`
- This verifies the mock infrastructure supports the `None` path for integration tests

**Verification:**
Run: `cargo test -p vmm --features uffd -- page_tracker_zero`
Expected: New tests pass

**Commit:** `test(uffd): add PageTracker tests for LoadSource::Zero variant`
<!-- END_TASK_3 -->
<!-- END_SUBCOMPONENT_A -->

<!-- START_TASK_4 -->
### Task 4: Verify full phase builds and tests

**Verifies:** None (verification)

**Files:** None

**Verification:**
Run: `cargo build -p vmm --features uffd`
Expected: Builds without errors

Run: `cargo test -p vmm --features uffd`
Expected: All tests pass including new PageTracker tests

**Commit:** Not needed if previous tasks committed individually.
<!-- END_TASK_4 -->
