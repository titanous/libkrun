# Memory Balloon Device Design

## Summary

This document describes implementing a memory balloon device for libkrun, a lightweight VM library. A memory balloon device allows the host to reclaim guest RAM at runtime: the host requests the guest to "inflate" the balloon (donate pages back to the host), and the host uses `madvise(MADV_DONTNEED)` to release those pages to the OS without destroying the VM. The guest can later "deflate" the balloon to recover the memory. The core use case driving this design is reducing snapshot sizes — inflate the balloon to reclaim idle guest memory, take a snapshot, then deflate. Because the reclaimed pages contain no live data, they can be omitted from the snapshot entirely.

The implementation completes an existing skeleton and adds four interconnected capabilities. First, the virtio balloon device itself is finished: inflate/deflate queues process guest-reported page frame numbers, a stats queue exposes guest memory counters, and two free page reporting mechanisms (proactive hint queue and the modern page-reporting API) allow the guest kernel to proactively donate idle pages without explicit host requests. Second, reclaimed pages are tracked in host-side atomic bitmaps and excluded from both full and incremental snapshots, with `mincore()` used at snapshot time to verify reported-free pages have not been quietly reused by the guest. Third, UFFD demand-paging restore is updated to zero-fill page faults for reclaimed pages rather than reading from the snapshot store, significantly reducing restore latency for reclaimed pages. Fourth, a new Rust API (`BalloonHandle`) is exposed on the running VM handle, providing `resize` and `await_target` methods so callers can programmatically inflate the balloon and wait for the guest to acknowledge the requested reduction.

## Definition of Done

1. **Complete balloon device** — Finish the existing skeleton: inflate/deflate queues, stats queue, free page hint, free page reporting (already partially working), with MADV_DONTNEED to release pages on inflate
2. **Snapshot integration** — Balloon-reclaimed pages excluded from both full and incremental snapshots, reducing snapshot size proportional to unused guest memory
3. **UFFD zero-fill** — Pages not present in snapshot (reclaimed by balloon) are zero-filled on demand during UFFD restore
4. **Rust API for runtime control** — Balloon resize method on the running VM (not just Builder config), enabling the inflate→snapshot→deflate workflow

## Acceptance Criteria

### mem-balloon.AC1: Balloon device processes inflate/deflate and reports stats
- **mem-balloon.AC1.1 Success:** Inflate queue processes PFN array and releases host memory via MADV_DONTNEED for each page
- **mem-balloon.AC1.2 Success:** Deflate queue processes PFN array and guest regains access to pages
- **mem-balloon.AC1.3 Success:** Guest writes to `actual` config field (offset 4) and device stores the updated value
- **mem-balloon.AC1.4 Success:** Stats queue returns valid memory counters (at minimum: MemFree, MemTotal, MemAvailable)
- **mem-balloon.AC1.5 Success:** Free page hint queue processes command ID protocol (START → page blocks → STOP) and calls MADV_DONTNEED on reported blocks
- **mem-balloon.AC1.6 Failure:** Inflate with invalid PFN (outside guest memory) is silently skipped without crashing
- **mem-balloon.AC1.7 Failure:** Stats request before guest driver activates returns None (not panic or stale data)
- **mem-balloon.AC1.8 Edge:** Guest sends duplicate PFN in inflate queue — idempotent (atomic OR on already-set bit, MADV_DONTNEED on already-released page is no-op)

### mem-balloon.AC2: Reclaimed pages excluded from snapshots
- **mem-balloon.AC2.1 Success:** Inflated pages tracked in bitmap; bits set on inflate, cleared on deflate
- **mem-balloon.AC2.2 Success:** Reported-free pages tracked in separate bitmap; bits set on PHQ/FRQ processing
- **mem-balloon.AC2.3 Success:** Full snapshot excludes all inflated pages — snapshot size reduced proportionally
- **mem-balloon.AC2.4 Success:** Full snapshot excludes reported-free pages verified as non-resident via mincore
- **mem-balloon.AC2.5 Success:** Incremental snapshot records newly-reclaimed pages in `reclaimed_pages` field
- **mem-balloon.AC2.6 Success:** Restore from incremental snapshot zero-fills reclaimed pages (does not use stale base data)
- **mem-balloon.AC2.7 Failure:** Reported-free page reused by guest (resident, non-zero data) is NOT excluded from snapshot
- **mem-balloon.AC2.8 Edge:** Page inflated then deflated before snapshot is NOT excluded (deflate clears inflated bit)
- **mem-balloon.AC2.9 Edge:** Snapshot with no balloon enabled or balloon at zero produces identical output to current behavior (no regression)

### mem-balloon.AC3: UFFD restore zero-fills reclaimed pages
- **mem-balloon.AC3.1 Success:** Page fault for a reclaimed page resolved via `uffd.zeropage()` — guest sees zeros
- **mem-balloon.AC3.2 Success:** PageTracker records zero-filled pages under `LoadSource::Zero`
- **mem-balloon.AC3.3 Failure:** Page fault for a present page (not reclaimed) still reads from store and uses `uffd.copy()` — no regression
- **mem-balloon.AC3.4 Edge:** `zeropage` EEXIST (race with preload) handled identically to existing `copy` EEXIST (silently ignored)

### mem-balloon.AC4: Rust API enables inflate→snapshot workflow
- **mem-balloon.AC4.1 Success:** `Builder::enable_balloon()` creates balloon device during build
- **mem-balloon.AC4.2 Success:** `VmHandle::balloon()` returns `Some(&BalloonHandle)` when enabled, `None` when not
- **mem-balloon.AC4.3 Success:** `BalloonHandle::resize(target_mb)` triggers guest inflation (guest `actual` increases toward target)
- **mem-balloon.AC4.4 Success:** `BalloonHandle::await_target(target, stall_timeout, max_timeout)` returns `Reached` when target met, `Stalled` when guest stops progressing, `Err(Timeout)` when max_timeout exceeded
- **mem-balloon.AC4.5 Success:** Balloon device state survives snapshot/restore — after restore, balloon retains inflation target and actual count
- **mem-balloon.AC4.6 Failure:** `resize` on inactive device returns `Err(DeviceNotActive)`
- **mem-balloon.AC4.7 Failure:** `await_target` with `max_timeout = None` and stalled guest returns `Stalled` (does not hang)
- **mem-balloon.AC4.8 Edge:** Concurrent resize updates target; guest inflates toward new target; `await_target` waiters see new target

## Glossary

- **Memory balloon device**: A paravirtual device that lets the host adjust the amount of RAM the guest can use at runtime. The host "inflates" the balloon (taking pages from the guest) or "deflates" it (returning pages to the guest) without rebooting the VM.
- **PFN (Page Frame Number)**: An index identifying a physical 4KB page of memory. The guest balloon driver sends arrays of PFNs to tell the host which specific pages are being donated or reclaimed.
- **MADV_DONTNEED**: A Linux `madvise` flag that instructs the kernel to discard the backing physical memory for a virtual address range. Subsequent reads return zeros; the pages are not removed from the virtual address space but their physical memory is freed.
- **Free Page Hint (PHQ, F_FREE_PAGE_HINT)**: A virtio balloon feature where the guest kernel proactively reports free pages using a command ID handshake protocol (START → page blocks → STOP) so the host can release them.
- **Free Page Reporting (FRQ, F_REPORTING)**: A newer free page reporting mechanism using the Linux `page_reporting` kernel API. The guest sends scatter-gather lists of free pages directly; no command ID handshake needed.
- **mincore()**: A Linux syscall that returns one byte per page indicating whether each page is resident in physical memory. Used here to verify reported-free pages have not been silently reused by the guest before excluding them from snapshots.
- **UFFD (userfaultfd)**: A Linux mechanism that delivers page faults to userspace for handling. Used in libkrun's snapshot restore to demand-page guest memory from a snapshot store.
- **zeropage ioctl**: A userfaultfd operation that resolves a page fault by mapping the kernel's shared zero page, without allocating physical memory or copying data.
- **SnapshotStore**: A trait in the VMM crate representing an abstract snapshot data store. Implementations include `FsSnapshotStore` (filesystem) and a production CAS store.
- **CAS store (Content-Addressable Storage)**: A storage backend where data is addressed by content hash. Naturally represents absent pages as missing keys.
- **Incremental snapshot**: A snapshot recording only pages changed since a base snapshot. Extended here with a `reclaimed_pages` list for pages that should be zero-filled on restore.
- **KVM dirty log**: A KVM feature tracking which guest memory pages have been written to since the log was last cleared. Used by incremental snapshots to identify changed pages.
- **DirtyBitmap**: An existing libkrun type — a `Vec<AtomicU64>` where each bit represents one 4KB page. The reclaimed page bitmaps follow this same lock-free atomic pattern.
- **Snapshottable**: A trait implemented by virtio devices to serialize and restore internal state across snapshot/restore cycles.
- **Condvar**: A synchronization primitive that lets one thread wait for a condition and another signal it. Used in `await_target` to block until the guest updates the `actual` field, avoiding busy-polling.
- **Config space (`actual` field)**: A memory-mapped region in the virtio balloon device that the guest driver writes to report its current balloon size in pages.

## Architecture

### Balloon Device

Complete the existing skeleton at `src/devices/src/virtio/balloon/` with full virtio balloon feature support. The device advertises all modern features assuming a recent kernel (7.0+):

- **F_MUST_TELL_HOST** — guest waits for host ACK before freeing deflated pages
- **F_DEFLATE_ON_OOM** — guest auto-deflates under memory pressure
- **F_STATS_VQ** — host requests memory stats (MemAvailable, MemFree, swap, OOM kills)
- **F_FREE_PAGE_HINT** — guest proactively reports free page blocks via command ID protocol
- **F_PAGE_POISON** — guest guarantees freed pages are initialized (required by F_REPORTING)
- **F_REPORTING** — modern free page reporting via kernel page_reporting API (partially implemented)

Five virtqueues (already defined in `src/devices/src/virtio/balloon/mod.rs`):

| Queue | Index | Function |
|-------|-------|----------|
| IFQ (inflate) | 0 | Guest sends 4KB PFN arrays. Host calls `madvise(MADV_DONTNEED)`, sets inflated bitmap. |
| DFQ (deflate) | 1 | Guest sends PFN arrays for pages returned. Host clears inflated bitmap. |
| STQ (stats) | 2 | Host pushes empty buffer, guest fills with 16 memory counters. Device stores latest for API. |
| PHQ (page hint) | 3 | Command ID protocol: host sets `free_page_report_cmd_id` in config, guest sends START → page blocks → STOP. Host calls MADV_DONTNEED, sets reported-free bitmap. |
| FRQ (reporting) | 4 | Guest sends scatter-gather lists of free pages. Host calls MADV_DONTNEED (existing), adds reported-free bitmap tracking. |

Config space (`VirtioBalloonConfig`): fix `write_config` to handle guest writes to the `actual` field at offset 4. Currently a no-op that logs a warning. The guest writes `actual` to report inflation/deflation progress.

### Reclaimed Page Tracking

Two lock-free atomic bitmaps, same design as existing `DirtyBitmap` (`src/vmm/src/dirty_bitmap.rs`) — `Vec<AtomicU64>` where each bit represents one 4KB page:

- **Inflated bitmap** — set on inflate queue processing, cleared on deflate. Authoritative: inflated pages cannot be accessed by the guest, so always safe to exclude from snapshots.
- **Reported-free bitmap** — set on PHQ/FRQ processing. Never cleared by a guest notification (guest can silently reallocate reported-free pages). Verified at snapshot time via `mincore()`.

Memory overhead: ~128KB per GB of guest RAM (~2MB for a 16GB VM).

Both bitmaps are host-side transient state. They are NOT serialized into snapshot state — after restore, both start empty (no pages are reclaimed in a freshly-restored VM).

### Snapshot Integration

Changes span the `SnapshotStore` trait (not FsSnapshotStore-specific) for compatibility with both the filesystem test store and the production CAS store.

**SnapshotStore trait changes** (`src/vmm/src/snapshot_store.rs`):

- `read_page` returns `Option<Vec<u8>>` instead of `Vec<u8>`. `None` means the page was not stored — the caller should zero-fill. For CAS stores, a missing key naturally returns `None`. For FsSnapshotStore, absent pages return `None` via a presence index.
- `write_pages` receives only present pages. Reclaimed pages are simply not included.
- Excluded page set stored in `VmSnapshot`/`IncrementalSnapshot` metadata so restore knows which pages are absent.

**Full snapshots** — at snapshot time (vCPUs paused):

1. All inflated pages excluded unconditionally (guest can't touch them).
2. Reported-free pages verified via `mincore()` batch check (~1ms for 14GB). Non-resident pages are definitively zeros — exclude. Resident pages read individually to check for guest reuse — exclude if zeros, include if non-zero.
3. Final exclusion set stored in snapshot metadata.
4. `write_pages` called with only present pages.

**Incremental snapshots** — add `reclaimed_pages: Vec<u64>` to `IncrementalSnapshot`:

1. Collect dirty pages from KVM dirty log as today.
2. Dirty pages that are currently inflated: drop from `dirty_pages`, add to `reclaimed_pages` (their data is now zeros, not the stale dirty data).
3. Pages in the base snapshot that are now inflated or verified-reported-free but NOT dirty: add to `reclaimed_pages` (base has stale data).
4. On restore: `apply_dirty_pages` applies dirty data as today. For `reclaimed_pages`, zero-fill the guest memory at those addresses.

### UFFD Zero-Fill

Changes to `src/vmm/src/uffd.rs`:

**Fault handler:** When `store.read_page(guest_addr)` returns `None`, use `uffd.zeropage(host_addr, len, wake=true)` instead of `uffd.copy()`. The kernel maps the shared zero page — no data copy, no store read, no physical page allocation. ~0.1µs vs ~10µs+ for a store read + copy.

**Preload:** No changes needed. The preload stream from `store.preload(regions)` naturally skips absent pages — they're not in the store. If a fault arrives for a reclaimed page before preload covers it, the fault handler resolves it via `zeropage`.

**PageTracker:** Add `LoadSource::Zero` variant to distinguish zero-filled pages in stats. Reclaimed pages resolved via `zeropage` are marked as loaded with this source.

### Rust API

**Builder** (`src/libkrun/src/lib.rs`):

- `enable_balloon() -> &mut Self` — enables balloon device creation. No configuration parameters (features are fixed for modern kernels).

**VmHandle** (`src/libkrun/src/lib.rs`):

- `balloon() -> Option<&BalloonHandle>` — returns `None` if balloon not enabled. `BalloonHandle` stored as a field on `VmHandle`, populated during `build()`.

**BalloonHandle** (new type):

Holds `Arc<Mutex<Balloon>>` and a condvar `Arc<(Mutex<u64>, Condvar)>` for `actual` change notification. No VMM lock needed for balloon operations.

```rust
pub struct BalloonHandle { /* ... */ }

pub enum BalloonResult {
    Reached(u64),   // actual >= target
    Stalled(u64),   // no progress for stall_timeout
}

pub enum BalloonError {
    Timeout { actual: u64 },
    DeviceNotActive,
}

impl BalloonHandle {
    pub fn resize(&self, target_mb: u64) -> Result<(), BalloonError>;
    pub fn await_target(
        &self,
        target_mb: u64,
        stall_timeout: Duration,
        max_timeout: Option<Duration>,
    ) -> Result<BalloonResult, BalloonError>;
    pub fn actual(&self) -> u64;
    pub fn stats(&self) -> Option<BalloonStats>;
}
```

**Stall detection:** `await_target` waits on the condvar with `stall_timeout`. Each time the guest updates `actual` (via `write_config`), the condvar fires and the stall timer resets. If no progress for `stall_timeout`, returns `Stalled(actual)`. If `max_timeout` exceeded, returns `Err(Timeout)`. Both `Reached` and `Stalled` are non-error outcomes — the caller got as much reclamation as possible.

**Condvar signaling:** `write_config` is modified to detect writes to the `actual` field (offset 4), update the condvar's `Mutex<u64>`, and call `notify_all()`.

**Vmm integration:** `Vmm` struct gets a `balloon: Option<Arc<Mutex<Balloon>>>` field, set during device creation in `builder.rs`. The snapshot code queries the balloon's reclaimed bitmaps via this reference.

## Existing Patterns

**Virtio device structure:** Follows the existing 3-file pattern (`mod.rs`, `device.rs`, `event_handler.rs`) already established in `src/devices/src/virtio/balloon/`. The skeleton already implements `VirtioDevice` and `Subscriber` traits.

**Atomic bitmap:** The reclaimed page bitmaps follow the `DirtyBitmap` pattern in `src/vmm/src/dirty_bitmap.rs` — `Vec<AtomicU64>` with `Relaxed` ordering for mark/clear and `AcqRel` for drain. Adds per-page clear (AND operation) that `DirtyBitmap` lacks.

**VmHandle sub-object:** New pattern. Existing VmHandle has flat methods (pause, resume, snapshot). `BalloonHandle` introduces a sub-object pattern via `balloon() -> Option<&BalloonHandle>`. This is a deliberate divergence to avoid VmHandle method proliferation as more devices gain runtime control.

**SnapshotStore trait:** Modifies the existing trait in `src/vmm/src/snapshot_store.rs`. The `read_page` return type change from `Vec<u8>` to `Option<Vec<u8>>` is a breaking change. No backwards compatibility required — CAS store and FsSnapshotStore both updated.

**Snapshot structs:** Adds `reclaimed_pages: Vec<u64>` to `IncrementalSnapshot` and excluded page metadata to `VmSnapshot` in `src/vmm/src/snapshot.rs`. Follows existing pattern of adding `#[serde(default)]` fields for forward compatibility.

**Feature gating:** Balloon is gated with `#[cfg(not(feature = "tee"))]`, matching existing pattern.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: Balloon Device Core
**Goal:** Complete inflate/deflate queue processing with MADV_DONTNEED and config space handling.

**Components:**
- Inflate queue handler in `src/devices/src/virtio/balloon/event_handler.rs` — process PFN arrays, call MADV_DONTNEED
- Deflate queue handler in `src/devices/src/virtio/balloon/event_handler.rs` — process PFN arrays
- Config write handler in `src/devices/src/virtio/balloon/device.rs` — handle `actual` field writes at offset 4
- Feature flags in `src/devices/src/virtio/balloon/device.rs` — add F_MUST_TELL_HOST, F_DEFLATE_ON_OOM, F_PAGE_POISON to advertised features

**Dependencies:** None (first phase)

**Done when:** Guest balloon driver can negotiate features, inflate/deflate queues process PFNs, MADV_DONTNEED releases host memory, config space `actual` field updates correctly. Covers mem-balloon.AC1.1, mem-balloon.AC1.2, mem-balloon.AC1.3.
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: Stats and Free Page Reporting
**Goal:** Complete stats queue, free page hint queue, and enhance existing free page reporting queue.

**Components:**
- Stats queue handler in `src/devices/src/virtio/balloon/event_handler.rs` — process stats buffers, store latest counters
- BalloonStats struct in `src/devices/src/virtio/balloon/device.rs` — parsed memory statistics
- Free page hint handler in `src/devices/src/virtio/balloon/event_handler.rs` — command ID protocol (START/STOP), MADV_DONTNEED on page blocks
- Free page hint config in `src/devices/src/virtio/balloon/device.rs` — `free_page_report_cmd_id` management, config change signaling

**Dependencies:** Phase 1 (core queue processing pattern)

**Done when:** Stats queue returns valid memory counters, free page hint protocol works with command IDs, existing FRQ continues to function. Covers mem-balloon.AC1.4, mem-balloon.AC1.5.
<!-- END_PHASE_2 -->

<!-- START_PHASE_3 -->
### Phase 3: Reclaimed Page Bitmaps
**Goal:** Track inflated and reported-free pages for snapshot exclusion.

**Components:**
- ReclaimedBitmap in `src/devices/src/virtio/balloon/` — lock-free atomic bitmap with mark, clear, and drain operations
- Inflated bitmap integration — inflate handler sets bits, deflate handler clears bits
- Reported-free bitmap integration — PHQ and FRQ handlers set bits
- Balloon query interface — method to retrieve current inflated and reported-free page sets for snapshot use

**Dependencies:** Phase 1 (inflate/deflate), Phase 2 (PHQ/FRQ)

**Done when:** Bitmaps accurately track inflated and reported-free pages, inflated bits cleared on deflate, bitmap queryable from outside the device. Covers mem-balloon.AC2.1, mem-balloon.AC2.2.
<!-- END_PHASE_3 -->

<!-- START_PHASE_4 -->
### Phase 4: Snapshot Integration
**Goal:** Exclude reclaimed pages from full and incremental snapshots.

**Components:**
- SnapshotStore trait in `src/vmm/src/snapshot_store.rs` — `read_page` returns `Option<Vec<u8>>`
- FsSnapshotStore in `src/vmm/src/snapshot_store.rs` — implement page presence tracking for absent pages
- VmSnapshot metadata in `src/vmm/src/snapshot.rs` — excluded page set storage
- IncrementalSnapshot in `src/vmm/src/snapshot.rs` — `reclaimed_pages: Vec<u64>` field
- Full snapshot path in `src/vmm/src/lib.rs` — query balloon bitmaps, verify reported-free via mincore, exclude pages from write_pages
- Incremental snapshot path in `src/vmm/src/lib.rs` — cross-reference dirty log with inflated set, build reclaimed_pages list
- Restore path in `src/vmm/src/snapshot.rs` — zero-fill reclaimed pages during incremental apply

**Dependencies:** Phase 3 (reclaimed bitmaps)

**Done when:** Full snapshots exclude inflated pages and verified reported-free pages. Incremental snapshots record newly-reclaimed pages. Restore correctly zero-fills reclaimed pages. Snapshot size proportional to present pages only. Covers mem-balloon.AC2.3, mem-balloon.AC2.4, mem-balloon.AC2.5, mem-balloon.AC2.6.
<!-- END_PHASE_4 -->

<!-- START_PHASE_5 -->
### Phase 5: UFFD Zero-Fill
**Goal:** Efficiently resolve page faults for reclaimed pages during UFFD restore.

**Components:**
- Fault handler in `src/vmm/src/uffd.rs` — handle `None` from `read_page` with `uffd.zeropage()`
- LoadSource enum in `src/vmm/src/uffd.rs` — add `Zero` variant for stats tracking
- PageTracker in `src/vmm/src/uffd.rs` — track zero-filled pages separately

**Dependencies:** Phase 4 (SnapshotStore trait changes)

**Done when:** UFFD restore resolves reclaimed page faults via zeropage ioctl, PageTracker reports zero-fill count separately. Covers mem-balloon.AC3.1, mem-balloon.AC3.2.
<!-- END_PHASE_5 -->

<!-- START_PHASE_6 -->
### Phase 6: Rust API
**Goal:** Expose balloon control through Builder and VmHandle.

**Components:**
- Builder in `src/libkrun/src/lib.rs` — `enable_balloon()` method
- BalloonHandle in `src/libkrun/src/lib.rs` — resize, await_target, actual, stats methods
- BalloonResult/BalloonError in `src/libkrun/src/lib.rs` — result types
- VmHandle in `src/libkrun/src/lib.rs` — `balloon()` accessor, BalloonHandle field
- Vmm in `src/vmm/src/lib.rs` — `balloon: Option<Arc<Mutex<Balloon>>>` field
- Condvar signaling in `src/devices/src/virtio/balloon/device.rs` — write_config triggers condvar on actual update

**Dependencies:** Phase 1 (balloon device), Phase 2 (stats)

**Done when:** Builder can enable balloon, VmHandle exposes BalloonHandle, resize triggers guest inflation, await_target blocks with stall detection, stats returns valid counters. Covers mem-balloon.AC4.1, mem-balloon.AC4.2, mem-balloon.AC4.3, mem-balloon.AC4.4.
<!-- END_PHASE_6 -->

<!-- START_PHASE_7 -->
### Phase 7: Balloon Snapshottable
**Goal:** Balloon device state survives snapshot/restore cycle.

**Components:**
- Snapshottable impl in `src/devices/src/virtio/balloon/device.rs` — serialize avail_features, acked_features, config (including num_pages and actual), free page hint command state
- Restore in `src/devices/src/virtio/balloon/device.rs` — deserialize and re-establish device state

**Dependencies:** Phase 1 (balloon device)

**Done when:** Balloon device state round-trips through snapshot/restore. After restore, balloon retains its inflation target and actual count. Covers mem-balloon.AC4.5.
<!-- END_PHASE_7 -->

## Additional Considerations

**Performance characteristics:**

| Operation | Cost |
|-----------|------|
| Inflate (per GB) | ~0.25-0.5s (one MADV_DONTNEED per 4KB PFN; can improve by merging contiguous ranges) |
| Free page reporting (14GB) | ~7ms (2MB blocks from kernel page_reporting API) |
| Snapshot verification (14GB reported-free) | ~1-10ms with mincore batch check |
| Snapshot write savings | Proportional to reclaimed pages (e.g., 4x smaller for 75% reclaimed) |
| UFFD zero-fill per page | ~0.1µs (vs ~10µs+ for store read + copy) |
| Runtime bitmap overhead | ~128KB per GB guest RAM |

**mincore() for reported-free verification:** After vCPU pause, `mincore()` returns one byte per page for an entire range in a single syscall. Non-resident pages (not faulted since MADV_DONTNEED) are definitively zeros. Resident pages need individual read + memcmp to check for guest reuse. Reading MADV_DONTNEED'd pages maps the kernel zero page without physical allocation, so verification doesn't increase host RSS.

**Contiguous PFN merging:** The inflate queue receives 4KB PFNs individually. Sorting and merging contiguous PFNs into ranges before calling `madvise` reduces syscall count. For a guest that frees memory in large contiguous blocks (common after workload completion), this can reduce inflate time significantly.
