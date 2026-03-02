# Memory Balloon Device Implementation Plan — Phase 3: Reclaimed Page Bitmaps

**Goal:** Track inflated and reported-free pages in lock-free atomic bitmaps for snapshot exclusion.

**Architecture:** Create a `ReclaimedBitmap` type following the `DirtyBitmap` pattern (`Vec<AtomicU64>` with atomic operations) but with 4KB page granularity and per-page clear support. Two bitmap instances on the Balloon device: one for inflated pages (set on inflate, cleared on deflate) and one for reported-free pages (set on PHQ/FRQ, never cleared by guest).

**Tech Stack:** Rust, std::sync::atomic

**Scope:** 7 phases from original design (phase 3 of 7)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### mem-balloon.AC2: Reclaimed pages excluded from snapshots
- **mem-balloon.AC2.1 Success:** Inflated pages tracked in bitmap; bits set on inflate, cleared on deflate
- **mem-balloon.AC2.2 Success:** Reported-free pages tracked in separate bitmap; bits set on PHQ/FRQ processing

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->
<!-- START_TASK_1 -->
### Task 1: Create ReclaimedBitmap type

**Verifies:** mem-balloon.AC2.1, mem-balloon.AC2.2

**Files:**
- Create: `src/devices/src/virtio/balloon/reclaimed_bitmap.rs`
- Modify: `src/devices/src/virtio/balloon/mod.rs` (add `mod reclaimed_bitmap; pub use ...`)

**Implementation:**

Create `reclaimed_bitmap.rs` following the `DirtyBitmap` pattern at `src/vmm/src/dirty_bitmap.rs`.

Key differences from `DirtyBitmap`:
- **4KB page size** (virtio balloon PFN granularity) instead of DirtyBitmap's 16KB (Apple Silicon)
- **Per-page clear** via atomic AND (DirtyBitmap only has mark and drain-all)
- **Non-destructive query** methods (iterate set bits without clearing; DirtyBitmap's `drain_dirty_pages` resets)
- **Page-frame-number indexing** (PFN is just the page index; address = PFN << 12)

```rust
pub const BALLOON_PAGE_SIZE: u64 = 4096;
pub const BALLOON_PAGE_SHIFT: u32 = 12;

pub struct ReclaimedBitmap {
    num_pages: usize,
    bitmap: Vec<AtomicU64>,
}
```

Methods:
- `new(num_pages: usize) -> Self` — allocate `(num_pages + 63) / 64` words, all zero
- `mark(&self, pfn: u32)` — atomic `fetch_or(1 << bit_idx, Ordering::Relaxed)` on the appropriate word. Silently ignore if pfn >= num_pages (same pattern as DirtyBitmap::mark_dirty bounds check)
- `clear(&self, pfn: u32)` — atomic `fetch_and(!(1 << bit_idx), Ordering::Relaxed)`. Silently ignore out of bounds.
- `is_set(&self, pfn: u32) -> bool` — load word with `Ordering::Relaxed`, check bit
- `mark_range(&self, start_pfn: u32, count: u32)` — mark a contiguous range of PFNs (for PHQ/FRQ which report ranges). Can iterate and call `mark()` for each, or optimize with word-level atomic OR for aligned ranges.
- `iter_set_pages(&self) -> Vec<u32>` — return all set PFN indices. Iterate words, extract set bits. Used by snapshot code to get the exclusion set.
- `count(&self) -> usize` — count total set bits (for stats/debugging)

The bitmap is constructed with `num_pages` derived from guest memory size: `(total_guest_bytes / 4096) as usize`. PFN 0 maps to guest physical address 0.

Add `pub(crate) use reclaimed_bitmap::ReclaimedBitmap;` to `mod.rs`.

**Testing:**
Tests must verify:
- mem-balloon.AC2.1: `mark(pfn)` sets the bit, `clear(pfn)` clears it, `is_set(pfn)` returns correct state
- mem-balloon.AC2.2: `mark_range` sets all PFNs in the range, `iter_set_pages` returns them

Include unit tests in `#[cfg(test)]` module within `reclaimed_bitmap.rs`:
- Test mark/clear/is_set roundtrip
- Test mark_range for contiguous PFNs
- Test duplicate mark is idempotent
- Test clear on unset bit is no-op
- Test out-of-bounds mark/clear silently ignored
- Test iter_set_pages returns all set PFNs in order

**Verification:**
Run: `cargo test -p devices --features net`
Expected: New bitmap tests pass

**Commit:** `feat(balloon): add ReclaimedBitmap for tracking reclaimed pages`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add bitmap fields to Balloon and create during activation

**Verifies:** None (infrastructure for AC2.1, AC2.2)

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs:48-55` (add bitmap fields to Balloon struct)
- Modify: `src/devices/src/virtio/balloon/device.rs:162-186` (create bitmaps in `activate()`)

**Implementation:**

Add fields to the `Balloon` struct:
```rust
pub(crate) inflated_bitmap: Option<ReclaimedBitmap>,
pub(crate) reported_free_bitmap: Option<ReclaimedBitmap>,
```

Initialize both as `None` in `Balloon::new()`.

In `activate()`, after storing `self.device_state`, create both bitmaps sized to cover all guest memory:
1. Calculate total guest address space: iterate `mem.iter()` regions, find the highest end address (`region.start_addr().raw_value() + region.len()`)
2. Convert to page count: `(max_addr / 4096) as usize`
3. Create both bitmaps: `self.inflated_bitmap = Some(ReclaimedBitmap::new(num_pages))` and same for `reported_free_bitmap`

Add a public query method for snapshot integration (Phase 4):
```rust
pub fn reclaimed_bitmaps(&self) -> (Option<&ReclaimedBitmap>, Option<&ReclaimedBitmap>) {
    (self.inflated_bitmap.as_ref(), self.reported_free_bitmap.as_ref())
}
```

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): create reclaimed page bitmaps during device activation`
<!-- END_TASK_2 -->
<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-4) -->
<!-- START_TASK_3 -->
### Task 3: Integrate inflated bitmap with inflate/deflate processing

**Verifies:** mem-balloon.AC2.1

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs` (update `process_inflate()` and `process_deflate()` from Phase 1)

**Implementation:**

In `process_inflate()` (implemented in Phase 1), after each successful `madvise(MADV_DONTNEED)` call for a PFN:
```rust
if let Some(ref bitmap) = self.inflated_bitmap {
    bitmap.mark(pfn);
}
```

In `process_deflate()` (implemented in Phase 1), after popping each descriptor chain, read the PFN values and clear the corresponding bits. Deflate needs to actually read the PFN arrays (same pattern as inflate) to know which pages to clear:
1. For each descriptor in the chain, iterate PFN values (same loop as inflate)
2. For each PFN: `if let Some(ref bitmap) = self.inflated_bitmap { bitmap.clear(pfn); }`

Note: Phase 1's deflate implementation just acknowledged descriptors without reading PFNs (like Firecracker). This task extends it to read PFNs for bitmap clearing. The deflate handler doesn't need madvise (guest regains pages by re-accessing them), but it does need to read the PFN values to clear bitmap bits.

**Testing:**
Tests must verify:
- mem-balloon.AC2.1: After inflate, inflated bitmap has bits set for the inflated PFNs. After deflate of the same PFNs, bits are cleared.

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): track inflated pages in bitmap on inflate/deflate`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Integrate reported-free bitmap with PHQ/FRQ processing

**Verifies:** mem-balloon.AC2.2

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs` (update `process_phq()` from Phase 2, and `process_frq()`)

**Implementation:**

In `process_phq()` (implemented in Phase 2), after each successful `madvise(MADV_DONTNEED)` call for a page range:
```rust
if let Some(ref bitmap) = self.reported_free_bitmap {
    let start_pfn = (desc.addr.raw_value() >> 12) as u32;
    let count = desc.len / 4096;
    bitmap.mark_range(start_pfn, count);
}
```

In `process_frq()` (existing code at `device.rs:74-112`), after the existing `madvise(MADV_DONTNEED)` call:
```rust
if let Some(ref bitmap) = self.reported_free_bitmap {
    let start_pfn = (desc.addr.raw_value() >> 12) as u32;
    let count = desc.len / 4096;
    bitmap.mark_range(start_pfn, count);
}
```

Both PHQ and FRQ report ranges of guest physical addresses (scatter-gather), so `desc.addr` is the guest physical address and `desc.len` is the byte count. Converting to PFN range: `start_pfn = addr >> 12`, `count = len / 4096`.

The reported-free bitmap is never cleared by guest notifications. Guest can silently reallocate pages. Verification at snapshot time (Phase 4) uses `mincore()` to check if reported-free pages are still actually free.

**Testing:**
Tests must verify:
- mem-balloon.AC2.2: After FRQ or PHQ processing, reported-free bitmap has bits set for the reported ranges

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

Run: `cargo build -p devices`
Expected: Full build succeeds

**Commit:** `feat(balloon): track reported-free pages in bitmap on PHQ/FRQ`
<!-- END_TASK_4 -->
<!-- END_SUBCOMPONENT_B -->

<!-- START_TASK_5 -->
### Task 5: Verify full phase builds and tests

**Verifies:** None (verification)

**Files:** None

**Verification:**
Run: `cargo build -p devices`
Expected: Builds without errors

Run: `cargo test -p devices --features net`
Expected: All tests pass including new ReclaimedBitmap unit tests

**Commit:** Not needed if previous tasks committed individually.
<!-- END_TASK_5 -->
