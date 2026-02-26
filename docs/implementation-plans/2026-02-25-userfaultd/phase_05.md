# Userfaultd Implementation Plan — Phase 5: PageTracker + Stats

**Goal:** Add an atomic bitmap for tracking which guest pages have been loaded (via preload or fault), and expose stats for monitoring restore progress.

**Architecture:** `PageTracker` uses `AtomicU64` words (one bit per page, same pattern as existing `DirtyBitmap` in `dirty_bitmap.rs`). Integrated into the UFFD handler to mark pages as loaded after each successful `uffd.copy()`. Exposes counters for pages loaded via preload vs fault, total fault count, and restore progress percentage.

**Tech Stack:** Rust, std::sync::atomic (AtomicU64, AtomicUsize)

**Scope:** 6 phases from original design (phase 5 of 6)

**Codebase verified:** 2026-02-25

---

## Acceptance Criteria Coverage

This phase implements and tests:

**Verifies: None directly** — This is internal monitoring infrastructure. PageTracker is described in the design's Architecture section as "Used for stats and monitoring, not correctness". The UFFD handler works correctly without it.

**Supports verification of (tested in Phase 6):**
- **userfaultd.AC5.2:** PageTracker stats can verify "near-zero faults" when full preload is used
- **userfaultd.AC5.6:** PageTracker stats confirm concurrent fault resolution counts

---

## Reference Files

- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/src/dirty_bitmap.rs` — Existing `DirtyBitmap` using `AtomicU64` words: `new(base_addr, size)`, `mark_dirty(guest_addr)`, `drain_dirty_pages()`, `contains()`
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/src/uffd.rs` — UFFD handler from Phases 3-4

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Implement PageTracker atomic bitmap

**Verifies:** None (internal monitoring)

**Files:**
- Modify: `src/vmm/src/uffd.rs` (add `PageTracker` struct)

**Implementation:**

Add `PageTracker` struct to `uffd.rs` (or as a sub-module). Follow the same pattern as `DirtyBitmap` in `src/vmm/src/dirty_bitmap.rs:18-25`:

```rust
pub struct PageTracker {
    total_pages: usize,
    bitmap: Vec<AtomicU64>,           // One bit per page, packed into u64 words
    preload_count: AtomicUsize,       // Pages loaded via preload
    fault_count: AtomicUsize,         // Pages loaded via fault handler
    total_faults: AtomicUsize,        // Total faults received (including EEXIST)
}
```

Methods:
- `new(total_pages: usize) -> Self` — allocate bitmap with `(total_pages + 63) / 64` AtomicU64 words, all zeroed
- `mark_loaded(&self, page_index: usize, source: LoadSource)` — set bit using `fetch_or` with `Ordering::Relaxed`. Increment the appropriate counter (`preload_count` or `fault_count`). If the bit was already set, don't increment (page was already loaded — this handles EEXIST race tracking).
- `record_fault(&self)` — increment `total_faults` counter (called on every fault event, regardless of outcome)
- `is_loaded(&self, page_index: usize) -> bool` — read bit with `Ordering::Relaxed`
- `stats(&self) -> PageTrackerStats` — return a snapshot of current counters

```rust
pub enum LoadSource {
    Preload,
    Fault,
}

pub struct PageTrackerStats {
    pub total_pages: usize,
    pub loaded_pages: usize,          // Count of set bits
    pub preload_pages: usize,
    pub fault_pages: usize,
    pub total_faults: usize,
    pub progress_pct: f64,            // loaded_pages / total_pages * 100.0
}
```

The `stats()` method counts set bits across all words (population count). This is O(n/64) where n = total_pages. For 1GB RAM with 4KB pages, that's ~4K AtomicU64 reads — fast enough for periodic stat queries.

**Page index calculation:** The UFFD handler needs to convert guest addresses to page indices. This can be done by having `PageTracker` know the guest memory layout (base addresses and sizes) or by having the caller compute the index. Simpler approach: flat indexing based on total memory — the caller computes `page_index = (guest_addr - region_base) / PAGE_SIZE + region_page_offset`.

**Testing:**

Unit tests for PageTracker bitmap operations:
- Mark page as loaded (preload), verify `is_loaded` returns true
- Mark same page twice, verify counter only increments once
- Mark pages from both sources, verify stats reflect correct counts
- Verify progress percentage calculation
- Verify concurrent marking from multiple threads doesn't corrupt

**Verification:**
Run: `cargo test -p vmm --features uffd`
Expected: Tests pass

**Commit:** `feat(vmm): implement PageTracker atomic bitmap for restore progress monitoring`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Integrate PageTracker into UFFD handler

**Verifies:** None (internal monitoring)

**Files:**
- Modify: `src/vmm/src/uffd.rs` (wire PageTracker into fault loop and preload task)

**Implementation:**

Add `PageTracker` as a shared field in `UffdHandler` (wrapped in `Arc` for sharing between fault and preload tasks):

1. In `UffdHandler::new()`, compute total pages across all registered regions and create `PageTracker::new(total_pages)`
2. In the fault loop (`fault_loop`):
   - After receiving a `Pagefault` event, call `tracker.record_fault()`
   - After successful `uffd.copy()` in the spawned task, call `tracker.mark_loaded(page_index, LoadSource::Fault)`
3. In the preload task (`preload_task`):
   - After successful `uffd.copy()` for a chunk, call `tracker.mark_loaded()` for each page in the chunk (mark range: `start_page..start_page + chunk_pages`)
   - For multi-page chunks, mark all pages in the range

Expose the `PageTracker` (or its stats) through the `UffdHandler` so the VMM or caller can query restore progress. The simplest approach: store `Arc<PageTracker>` and provide a public method to get stats.

**Logging:** Add periodic log output during restore:
```rust
// After every N faults or preload chunks, log progress
if tracker.stats().loaded_pages % 1000 == 0 {
    let stats = tracker.stats();
    log::info!("restore progress: {:.1}% ({} preload, {} fault, {} total faults)",
        stats.progress_pct, stats.preload_pages, stats.fault_pages, stats.total_faults);
}
```

**Testing:**

Integration-level verification: with a mock store, run the UFFD handler and verify PageTracker stats show the expected number of preload and fault pages.

**Verification:**
Run: `cargo test -p vmm --features uffd`
Expected: Tests pass

**Commit:** `feat(vmm): integrate PageTracker into UFFD handler for restore stats`
<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->
