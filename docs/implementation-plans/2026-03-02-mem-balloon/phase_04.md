# Memory Balloon Device Implementation Plan — Phase 4: Snapshot Integration

**Goal:** Exclude reclaimed pages from full and incremental snapshots, reducing snapshot size proportional to unused guest memory.

**Architecture:** Add `excluded_pages` metadata to `VmSnapshot` and `reclaimed_pages` to `IncrementalSnapshot`. Change `SnapshotStore::read_page` to return `Option<Vec<u8>>` (None for absent pages). Update `FsSnapshotStore` with a page presence index. Modify full snapshot path to query balloon bitmaps and verify reported-free pages via `mincore()`. Modify incremental snapshot path to cross-reference dirty log with inflated pages. Add `balloon` field to `Vmm` for snapshot-time access.

**Tech Stack:** Rust, bincode/serde, libc (mincore), vm_memory 0.18

**Scope:** 7 phases from original design (phase 4 of 7)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### mem-balloon.AC2: Reclaimed pages excluded from snapshots
- **mem-balloon.AC2.3 Success:** Full snapshot excludes all inflated pages — snapshot size reduced proportionally
- **mem-balloon.AC2.4 Success:** Full snapshot excludes reported-free pages verified as non-resident via mincore
- **mem-balloon.AC2.5 Success:** Incremental snapshot records newly-reclaimed pages in `reclaimed_pages` field
- **mem-balloon.AC2.6 Success:** Restore from incremental snapshot zero-fills reclaimed pages (does not use stale base data)
- **mem-balloon.AC2.7 Failure:** Reported-free page reused by guest (resident, non-zero data) is NOT excluded from snapshot
- **mem-balloon.AC2.8 Edge:** Page inflated then deflated before snapshot is NOT excluded (deflate clears inflated bit)
- **mem-balloon.AC2.9 Edge:** Snapshot with no balloon enabled or balloon at zero produces identical output to current behavior (no regression)

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->
<!-- START_TASK_1 -->
### Task 1: Add excluded_pages to VmSnapshot and reclaimed_pages to IncrementalSnapshot

**Verifies:** None (infrastructure prerequisite for AC2.3-AC2.6)

**Files:**
- Modify: `src/vmm/src/snapshot.rs:154-168` (add `excluded_pages` field to `VmSnapshot`)
- Modify: `src/vmm/src/snapshot.rs:305-318` (add `reclaimed_pages` field to `IncrementalSnapshot`)
- Modify: `src/vmm/src/snapshot.rs` (add `apply_reclaimed_pages` function)

**Implementation:**

Add to `VmSnapshot` struct:
```rust
#[cfg_attr(feature = "snapshot", serde(default))]
pub excluded_pages: Vec<u64>,  // Guest addresses of pages excluded from snapshot (balloon-reclaimed)
```

Add to `IncrementalSnapshot` struct:
```rust
#[cfg_attr(feature = "snapshot", serde(default))]
pub reclaimed_pages: Vec<u64>,  // Guest addresses that should be zero-filled on restore
```

Both use `#[serde(default)]` for forward compatibility — old snapshots without balloon will have empty vecs.

Add `apply_reclaimed_pages(mem: &GuestMemoryMmap, pages: &[u64])` function:
- For each guest address in `pages`, write 4096 zeros to guest memory at that address
- Uses `mem.write_slice(&[0u8; 4096], GuestAddress(addr))` for each
- Called during incremental restore to zero-fill reclaimed pages

**Verification:**
Run: `cargo check -p vmm --features snapshot`
Expected: Compiles without errors

**Commit:** `feat(snapshot): add excluded_pages and reclaimed_pages fields for balloon integration`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Change SnapshotStore::read_page to return Option

**Verifies:** None (trait change prerequisite for AC2.3, Phase 5)

**Files:**
- Modify: `src/vmm/src/snapshot_store.rs:60-61` (change `read_page` signature in trait)
- Modify: `src/vmm/src/snapshot_store.rs` (update `FsSnapshotStore::read_page` implementation)
- Modify: `src/vmm/src/snapshot_store.rs` (add `set_excluded_pages` method to `FsSnapshotStore`)
- Modify: all callers of `read_page` (uffd.rs — Phase 5 will handle the behavioral change)

**Implementation:**

Change the trait method signature:
```rust
// Before:
fn read_page(&self, guest_addr: u64) -> SendBoxFuture<'_, io::Result<Vec<u8>>>;
// After:
fn read_page(&self, guest_addr: u64) -> SendBoxFuture<'_, io::Result<Option<Vec<u8>>>>;
```

Update `FsSnapshotStore`:
- Add field: `excluded_pages: HashSet<u64>` (initialized empty)
- Add method: `pub fn set_excluded_pages(&mut self, pages: Vec<u64>)` — builds HashSet from the vec
- In `read_page` implementation: before reading from file, check `self.excluded_pages.contains(&guest_addr)`. If so, return `Ok(None)`. Otherwise, read from file and return `Ok(Some(data))`.

Update `preload` stream in `FsSnapshotStore`:
- When yielding chunks, skip pages that are in `excluded_pages`. The preload stream naturally skips these since they weren't written to the memory file.

Update all `read_page` callers to handle `Option`:
- In `src/vmm/src/uffd.rs`: wrap `Ok(data)` → `Ok(Some(data))` where needed. The full `None` handling for zero-fill is Phase 5.
- Any test mocks: update return types.

**Verification:**
Run: `cargo check -p vmm --features snapshot,uffd`
Expected: Compiles without errors

**Commit:** `feat(snapshot): change read_page to return Option<Vec<u8>> for absent page support`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Update FsSnapshotStore write_pages for sparse output

**Verifies:** mem-balloon.AC2.3

**Files:**
- Modify: `src/vmm/src/snapshot_store.rs` (update `FsSnapshotStore::write_pages` to handle sparse layout)

**Implementation:**

Currently `write_pages` receives `Vec<(u64, Vec<u8>)>` as region-sized chunks and writes them sequentially. With balloon exclusion, pages may be missing from within a region.

**Approach: Sparse file with holes.** The memory file retains the same layout where file offset = `guest_addr - base_addr`. Excluded pages are simply not written, creating file holes. On Linux, reading through a file hole returns zeros natively (no physical disk allocation). This means:

- `read_page` offset calculation is unchanged: `file_offset = guest_addr - base_addr`. For excluded pages, the `excluded_pages` HashSet (Task 2) returns `None` before hitting the file.
- `preload` stream: When reading 4MB chunks, holes within the chunk return zeros in the read buffer. This is correct — reclaimed pages ARE zeros. However, the preload should skip excluded page regions entirely for efficiency (avoid copying zeros). The preload stream already skips pages in `excluded_pages` since they aren't included in chunk construction.
- No file compaction or reindexing needed — file offsets are stable.

Update `write_pages`:
- Continue accepting `Vec<(u64, Vec<u8>)>` — the caller (`dump_memory_to_store`) provides only present page ranges
- Seek to `guest_addr - base_addr` before writing each chunk (existing offset calculation)
- Chunks for excluded pages are simply not provided by the caller, creating file holes

Also write a `page_index` file alongside the `memory` file:
- Contains the list of excluded page guest addresses (serialized as bincode `Vec<u64>`)
- Read by `FsSnapshotStore` during restore initialization to populate `excluded_pages` HashSet
- This is needed so the store knows which pages to return `None` for during restore (file holes return zeros, but the store needs to distinguish "absent page" from "page that happens to be zeros")

**Verification:**
Run: `cargo check -p vmm --features snapshot`
Expected: Compiles without errors

**Commit:** `feat(snapshot): support sparse memory writes with page index`
<!-- END_TASK_3 -->
<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 4-5) -->
<!-- START_TASK_4 -->
### Task 4: Add balloon reference to Vmm and mincore helper

**Verifies:** None (infrastructure for AC2.3, AC2.4)

**Files:**
- Modify: `src/vmm/src/lib.rs:215-253` (add `balloon` field to `Vmm` struct)
- Modify: `src/vmm/src/builder.rs` (set balloon field during VM construction)
- Create or modify: `src/vmm/src/lib.rs` (add `mincore_check` helper function)

**Implementation:**

Add to `Vmm` struct:
```rust
#[cfg(not(feature = "tee"))]
pub(crate) balloon: Option<std::sync::Arc<std::sync::Mutex<devices::virtio::balloon::Balloon>>>,
```

Feature-gated with `not(tee)` matching the balloon device's feature gate per design.

Set during VM construction in `builder.rs` when the balloon device is created. If no balloon device is configured, set to `None`.

**Note:** This is the canonical location for the `balloon` field on Vmm. Phase 6 Task 4 also references this field — if Phase 4 executes first, Phase 6 Task 4 should skip adding it (field already exists).

Add `mincore_check` helper function to `lib.rs` (Linux-only — `mincore()` is a Linux syscall):
```rust
/// Check which pages in a range are resident in memory.
/// Returns a Vec<bool> where true = page is resident.
fn mincore_check(host_addr: *const u8, len: usize) -> io::Result<Vec<bool>> {
    let page_count = (len + 4095) / 4096;
    let mut vec = vec![0u8; page_count];
    let ret = unsafe {
        libc::mincore(
            host_addr as *mut libc::c_void,
            len,
            vec.as_mut_ptr(),
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(vec.iter().map(|&v| v & 1 != 0).collect())
}
```

`mincore()` returns one byte per page. Bit 0 = page is resident. After `madvise(MADV_DONTNEED)`, non-resident pages are definitively zeros. Resident reported-free pages may have been silently reused by the guest — these need further verification (check for non-zero data).

**Verification:**
Run: `cargo check -p vmm --features snapshot`
Expected: Compiles without errors

**Commit:** `feat(vmm): add balloon reference to Vmm and mincore helper`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Update full snapshot path to exclude reclaimed pages

**Verifies:** mem-balloon.AC2.3, mem-balloon.AC2.4, mem-balloon.AC2.7, mem-balloon.AC2.8, mem-balloon.AC2.9

**Files:**
- Modify: `src/vmm/src/lib.rs:1095-1128` (update `dump_memory_to_store` to exclude reclaimed pages)
- Modify: `src/vmm/src/lib.rs:1131-1198` (update `snapshot_to_store` to populate `excluded_pages`)

**Implementation:**

**Platform note:** Balloon snapshot integration (querying bitmaps, mincore verification, page exclusion) applies only to the Linux `snapshot_to_store` path. The macOS snapshot path does not have balloon support — `mincore()` is Linux-specific, and UFFD (Phase 5) is also Linux-only. Gate the balloon query code with `#[cfg(target_os = "linux")]` or use the existing `#[cfg(not(feature = "tee"))]` gate (TEE is Linux-only, but the balloon field is already `cfg(not(tee))`). If the balloon field is `None` on macOS, the exclusion set is empty — no regression.

In `snapshot_to_store()`, after saving device states and before calling `dump_memory_to_store`:
1. Query balloon device for reclaimed bitmaps: `self.balloon.as_ref().map(|b| b.lock().unwrap().reclaimed_bitmaps())`
2. If balloon exists and has bitmaps:
   a. Get inflated page set from inflated bitmap's `iter_set_pages()` — these are always excluded (AC2.3)
   b. Get reported-free page set from reported-free bitmap's `iter_set_pages()`
   c. Verify reported-free pages via `mincore()`:
      - For each reported-free PFN, get host address
      - Call `mincore_check` on the page
      - If NOT resident: definitively zeros — exclude (AC2.4)
      - If resident: read the page data. If all zeros: exclude. If non-zero: guest reused the page — do NOT exclude (AC2.7)
   d. Build final excluded set: inflated pages + verified-free reported pages
3. If no balloon or balloon at zero: excluded set is empty (AC2.9 — no regression)
4. Store excluded page addresses in `VmSnapshot.excluded_pages`

In `dump_memory_to_store()`, accept the excluded page set:
1. When collecting memory pages from regions, split regions at 4KB page granularity
2. Skip pages whose guest address is in the excluded set
3. Pass only present page data to `store.write_pages()`
4. Also pass excluded set to store for the page index

**Note on performance:** The design says mincore verification takes ~1-10ms for 14GB. This is because `mincore()` handles an entire range in a single syscall. Use one `mincore_check` call per memory region, then cross-reference with the reported-free bitmap.

**Testing:**
Tests must verify:
- AC2.3: Inflated pages not in snapshot output
- AC2.4: Reported-free non-resident pages not in snapshot output
- AC2.7: Reported-free page with non-zero data IS in snapshot output
- AC2.8: Page inflated then deflated is NOT in excluded set (deflate clears bit in Phase 3)
- AC2.9: No balloon → empty excluded set → identical behavior

**Verification:**
Run: `cargo check -p vmm --features snapshot`
Expected: Compiles without errors

**Commit:** `feat(snapshot): exclude balloon-reclaimed pages from full snapshots`
<!-- END_TASK_5 -->
<!-- END_SUBCOMPONENT_B -->

<!-- START_SUBCOMPONENT_C (tasks 6-7) -->
<!-- START_TASK_6 -->
### Task 6: Update incremental snapshot path with reclaimed_pages

**Verifies:** mem-balloon.AC2.5

**Files:**
- Modify: `src/vmm/src/lib.rs:1251-1356` (update `incremental_snapshot_to_store` to build reclaimed_pages)

**Implementation:**

**Platform note:** Same Linux-only scope as Task 5. The macOS incremental snapshot path does not query balloon bitmaps. If `self.balloon` is `None` (macOS or balloon not enabled), `reclaimed_pages` is empty — no regression (AC2.9).

In `incremental_snapshot_to_store()`, after collecting dirty pages at line ~1302:

1. Query balloon for inflated and reported-free bitmaps (same as Task 5)
2. Build reclaimed page set:
   a. Dirty pages that are currently inflated: **remove from dirty_pages** (their data is now zeros, not the stale dirty data), **add to reclaimed_pages**
   b. Pages in the base snapshot that are now inflated or verified-reported-free but NOT in dirty_pages: add to reclaimed_pages (base has stale data for these)
3. Set `IncrementalSnapshot.reclaimed_pages` to the final list
4. If no balloon: reclaimed_pages is empty vec (backward compatible)

The key insight: dirty pages that are also inflated should NOT be stored with their dirty data (which is stale — the page was given back to the host). Instead, they go into `reclaimed_pages` to be zero-filled on restore.

**Testing:**
Tests must verify:
- AC2.5: After balloon inflation, incremental snapshot's reclaimed_pages contains the inflated page addresses

**Verification:**
Run: `cargo check -p vmm --features snapshot`
Expected: Compiles without errors

**Commit:** `feat(snapshot): record reclaimed pages in incremental snapshots`
<!-- END_TASK_6 -->

<!-- START_TASK_7 -->
### Task 7: Update incremental restore to zero-fill reclaimed pages

**Verifies:** mem-balloon.AC2.6

**Files:**
- Modify: `src/vmm/src/lib.rs:1059-1091` (update `restore_incremental_snapshot` to zero-fill reclaimed pages)

**Implementation:**

In `restore_incremental_snapshot()`, after `apply_dirty_pages()` (line ~1079):

1. If `incremental.reclaimed_pages` is non-empty:
   - Call `snapshot::apply_reclaimed_pages(&self.guest_memory, &incremental.reclaimed_pages)`
   - This writes 4096 zeros to each reclaimed page address in guest memory
2. If `reclaimed_pages` is empty (old snapshot without balloon): no-op (backward compatible)

The order matters: dirty pages are applied first (restoring modified data), then reclaimed pages are zero-filled (overwriting any stale base data at those addresses).

**Testing:**
Tests must verify:
- AC2.6: After restoring from incremental snapshot with reclaimed_pages, those pages contain zeros in guest memory

**Verification:**
Run: `cargo check -p vmm --features snapshot`
Expected: Compiles without errors

**Commit:** `feat(snapshot): zero-fill reclaimed pages during incremental restore`
<!-- END_TASK_7 -->
<!-- END_SUBCOMPONENT_C -->

<!-- START_TASK_8 -->
### Task 8: Update full restore to pass excluded pages to store

**Verifies:** mem-balloon.AC2.3 (restore side)

**Files:**
- Modify: `src/vmm/src/lib.rs:517-568` (update `restore_from_store` to handle excluded pages)

**Implementation:**

In `restore_from_store()`, after deserializing vmstate:

1. Extract `excluded_pages` from `vmstate.excluded_pages`
2. If the store is an `FsSnapshotStore`, call `set_excluded_pages()` to configure the presence index
3. During eager preload: pages in excluded set are naturally absent from the memory file. After preload completes, zero-fill excluded pages using `apply_reclaimed_pages()`

For UFFD restore (`restore_from_store_with_uffd`):
1. Pass excluded pages to the store (same as above)
2. The UFFD fault handler will be updated in Phase 5 to use `uffd.zeropage()` for `None` pages

**Verification:**
Run: `cargo build -p vmm --features snapshot`
Expected: Full build succeeds

Run: `cargo test -p vmm --features snapshot`
Expected: Existing snapshot tests pass (no regressions — excluded_pages defaults to empty)

**Commit:** `feat(snapshot): handle excluded pages during full restore`
<!-- END_TASK_8 -->
