# Memory Balloon Device Test Requirements

This document maps every acceptance criterion from the memory balloon device design
(`docs/design-plans/2026-03-02-mem-balloon.md`) to either an automated test or a documented
human verification approach. Tests are organized by AC group and rationalized against
implementation decisions made during planning.

**Conventions:**
- "Unit" tests live in `#[cfg(test)] mod tests` within the implementation file.
- "Integration" (e2e) tests use the `tests/test_cases/` host/guest framework with `#[host]`/`#[guest]` proc macros.
- Feature flags required for compilation are noted in the "Run command" column.
- Guest-side behavior that cannot be unit-tested (virtio negotiation, actual guest balloon driver) is verified via integration tests that boot a real microVM.

---

## mem-balloon.AC1: Balloon device processes inflate/deflate and reports stats

| Criterion | ID | Test Type | Test File | Test Name(s) | Run Command | Notes |
|---|---|---|---|---|---|---|
| Inflate queue processes PFN array and releases host memory via MADV_DONTNEED | AC1.1 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_process_inflate_basic`, `test_inflate_pfn_madvise` | `cargo test -p devices --features net -- balloon` | Construct mock GuestMemory, populate descriptor chain with u32 PFN array, call `process_inflate()`, assert return value true, used ring updated. MADV_DONTNEED verifiable indirectly (no crash; mincore shows page non-resident). |
| Inflate queue processes PFN array and releases host memory via MADV_DONTNEED | AC1.1 | Integration | `tests/test_cases/src/test_balloon_inflate.rs` | `balloon-inflate-basic` | `make test FEATURE_FLAGS="--features embedded_init"` | Host calls `BalloonHandle::resize()`, guest driver inflates. Host verifies `actual()` increases. Proves end-to-end inflate works with real guest driver. |
| Deflate queue processes PFN array and guest regains access | AC1.2 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_process_deflate_basic` | `cargo test -p devices --features net -- balloon` | Populate descriptor chain with PFN array, call `process_deflate()`, assert return value true, used ring updated. |
| Deflate queue processes PFN array and guest regains access | AC1.2 | Integration | `tests/test_cases/src/test_balloon_inflate.rs` | `balloon-inflate-deflate-cycle` | `make test FEATURE_FLAGS="--features embedded_init"` | Host inflates, then resizes to 0 (deflate). Guest writes to previously-inflated pages to prove access restored. Guest signals success over vsock. |
| Guest writes to `actual` config field (offset 4) and device stores updated value | AC1.3 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_write_config_actual`, `test_write_config_num_pages_readonly` | `cargo test -p devices --features net -- balloon` | Call `write_config(offset=4, &[...])`, verify config.actual updated. Call `write_config(offset=0, &[...])`, verify config.num_pages unchanged. |
| Stats queue returns valid memory counters (MemFree, MemTotal, MemAvailable) | AC1.4 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_process_stats_queue`, `test_stats_parse_tags` | `cargo test -p devices --features net -- balloon` | Populate descriptor with BalloonStat entries (MEMFREE, MEMTOT, AVAIL tags with known values), call `process_stats_queue()`, assert `stats()` returns `Some(BalloonStats)` with matching fields. |
| Stats queue returns valid memory counters | AC1.4 | Integration | `tests/test_cases/src/test_balloon_stats.rs` | `balloon-stats` | `make test FEATURE_FLAGS="--features embedded_init"` | Host calls `BalloonHandle::stats()` after guest driver activates. Assert at least `free_memory`, `total_memory`, `available_memory` are `Some` and non-zero. |
| Free page hint queue processes command ID protocol (START -> blocks -> STOP) and calls MADV_DONTNEED | AC1.5 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_process_phq_protocol`, `test_phq_start_stop_transition` | `cargo test -p devices --features net -- balloon` | Build descriptor chain: 4-byte START cmd matching host cmd, memory range descriptors, 4-byte STOP cmd. Call `process_phq()`, verify device transitions to DONE state, return value true. |
| Inflate with invalid PFN (outside guest memory) silently skipped | AC1.6 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_inflate_invalid_pfn_skipped` | `cargo test -p devices --features net -- balloon` | Populate descriptor with PFN far exceeding guest memory size alongside valid PFNs. Call `process_inflate()`. Assert no panic, valid PFNs processed (return true). |
| Stats request before guest driver activates returns None | AC1.7 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_stats_before_activation_none` | `cargo test -p devices --features net -- balloon` | Create `Balloon::new()`, call `stats()` without activating device. Assert returns `None`. |
| Duplicate PFN in inflate queue is idempotent | AC1.8 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_inflate_duplicate_pfn_idempotent` | `cargo test -p devices --features net -- balloon` | Populate descriptor with same PFN twice. Call `process_inflate()`. Assert no panic, return true. MADV_DONTNEED on already-released page is a kernel no-op. |

---

## mem-balloon.AC2: Reclaimed pages excluded from snapshots

| Criterion | ID | Test Type | Test File | Test Name(s) | Run Command | Notes |
|---|---|---|---|---|---|---|
| Inflated pages tracked in bitmap; bits set on inflate, cleared on deflate | AC2.1 | Unit | `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` | `test_mark_clear_roundtrip`, `test_mark_then_clear` | `cargo test -p devices --features net -- reclaimed_bitmap` | `mark(pfn)` then `is_set(pfn)` returns true; `clear(pfn)` then `is_set(pfn)` returns false. |
| Inflated pages tracked in bitmap; integration with inflate/deflate | AC2.1 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_inflate_sets_bitmap`, `test_deflate_clears_bitmap` | `cargo test -p devices --features net -- balloon` | After `process_inflate()`, `inflated_bitmap.is_set(pfn)` returns true. After `process_deflate()` with same PFNs, `is_set(pfn)` returns false. |
| Reported-free pages tracked in separate bitmap; bits set on PHQ/FRQ | AC2.2 | Unit | `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` | `test_mark_range`, `test_iter_set_pages` | `cargo test -p devices --features net -- reclaimed_bitmap` | `mark_range(start, count)` sets all PFNs in range; `iter_set_pages()` returns them. |
| Reported-free pages tracked; integration with FRQ | AC2.2 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_frq_sets_reported_free_bitmap` | `cargo test -p devices --features net -- balloon` | After `process_frq()`, `reported_free_bitmap.is_set(pfn)` returns true for the reported range. |
| Full snapshot excludes all inflated pages -- size reduced proportionally | AC2.3 | Unit | `src/vmm/src/snapshot_store.rs` | `test_fs_store_excluded_pages_return_none` | `cargo test -p vmm --features snapshot -- snapshot_store` | Create FsSnapshotStore, call `set_excluded_pages()`, verify `read_page()` returns `Ok(None)` for excluded addresses and `Ok(Some(...))` for present addresses. |
| Full snapshot excludes all inflated pages | AC2.3 | Integration | `tests/test_cases/src/test_balloon_snapshot.rs` | `balloon-snapshot-size-reduction` | `make test FEATURE_FLAGS="--features embedded_init"` | Host inflates balloon (e.g., 128MB of 256MB), takes snapshot. Compare snapshot file size to a no-balloon snapshot. Inflated snapshot should be measurably smaller. |
| Full snapshot excludes reported-free pages verified non-resident via mincore | AC2.4 | Unit | `src/vmm/src/lib.rs` | `test_mincore_check_after_madvise` | `cargo test -p vmm --features snapshot -- mincore` | Allocate anonymous mmap, `madvise(MADV_DONTNEED)` a page, call `mincore_check()`, assert that page is non-resident. |
| Full snapshot excludes reported-free pages via mincore | AC2.4 | Integration | `tests/test_cases/src/test_balloon_snapshot.rs` | `balloon-snapshot-free-page-reporting` | `make test FEATURE_FLAGS="--features embedded_init"` | Boot VM with balloon, let guest free page reporting run (kernel 7.0+ sends FRQ), snapshot. Restore and verify guest runs correctly (reported-free pages are zeros, which is correct). |
| Incremental snapshot records newly-reclaimed pages in `reclaimed_pages` field | AC2.5 | Unit | `src/vmm/src/snapshot.rs` | `test_incremental_snapshot_reclaimed_pages_serde` | `cargo test -p vmm --features snapshot -- incremental` | Create `IncrementalSnapshot` with non-empty `reclaimed_pages`, serialize/deserialize via bincode, verify field survives round-trip. |
| Incremental snapshot records reclaimed pages | AC2.5 | Integration | `tests/test_cases/src/test_balloon_snapshot.rs` | `balloon-incremental-snapshot-reclaimed` | `make test FEATURE_FLAGS="--features embedded_init"` | Take base snapshot, inflate balloon, take incremental snapshot. Restore from incremental chain, verify guest runs correctly. |
| Restore from incremental snapshot zero-fills reclaimed pages | AC2.6 | Unit | `src/vmm/src/snapshot.rs` | `test_apply_reclaimed_pages_zeros` | `cargo test -p vmm --features snapshot -- apply_reclaimed` | Allocate GuestMemory, write non-zero data at an address, call `apply_reclaimed_pages()` with that address, verify memory is now zeros. |
| Reported-free page reused by guest (resident, non-zero data) NOT excluded | AC2.7 | Unit | `src/vmm/src/lib.rs` | `test_mincore_reused_page_not_excluded` | `cargo test -p vmm --features snapshot -- mincore` | Allocate mmap, `madvise(MADV_DONTNEED)`, write non-zero data (making page resident), call `mincore_check()`, assert page IS resident. Logic: resident + non-zero -> not excluded. |
| Page inflated then deflated before snapshot is NOT excluded | AC2.8 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_inflate_deflate_clears_bitmap` | `cargo test -p devices --features net -- balloon` | Inflate PFN, deflate same PFN, assert `inflated_bitmap.is_set(pfn)` returns false. (Already partially covered by AC2.1 deflate test, but explicit inflate-then-deflate sequence.) |
| Snapshot with no balloon or balloon at zero: identical output (no regression) | AC2.9 | Unit | `src/vmm/src/lib.rs` | `test_snapshot_no_balloon_no_regression` | `cargo test -p vmm --features snapshot -- snapshot` | Take snapshot with `balloon: None` on Vmm. Assert `excluded_pages` is empty, snapshot output is identical to pre-balloon baseline. |
| No balloon: no regression | AC2.9 | Integration | Existing snapshot tests (`test_snapshot_restore.rs`, `test_uffd_demand_page.rs`, etc.) | All existing snapshot/UFFD tests | `make test FEATURE_FLAGS="--features embedded_init"` | Existing tests do NOT enable balloon. If they still pass unchanged, AC2.9 is proven. No new test needed -- regression suite is the test. |

---

## mem-balloon.AC3: UFFD restore zero-fills reclaimed pages

| Criterion | ID | Test Type | Test File | Test Name(s) | Run Command | Notes |
|---|---|---|---|---|---|---|
| Page fault for reclaimed page resolved via `uffd.zeropage()` -- guest sees zeros | AC3.1 | Integration | `tests/test_cases/src/test_balloon_uffd.rs` | `balloon-uffd-zero-fill` | `make test FEATURE_FLAGS="--features embedded_init"` | Host inflates balloon, snapshots, restores with UFFD. Guest reads previously-inflated pages, verifies they are zeros. Signals success over vsock. |
| PageTracker records zero-filled pages under `LoadSource::Zero` | AC3.2 | Unit | `src/vmm/src/uffd.rs` | `test_page_tracker_zero_source`, `test_page_tracker_zero_duplicate` | `cargo test -p vmm --features uffd -- page_tracker_zero` | Create PageTracker, call `mark_loaded(idx, LoadSource::Zero)`, assert `stats().zero_pages == 1`. Call twice for same page, assert still 1 (no double-count). |
| Page fault for present page still reads from store and uses `uffd.copy()` -- no regression | AC3.3 | Unit | `src/vmm/src/uffd.rs` | `test_mock_store_some_uses_copy` | `cargo test -p vmm --features uffd -- mock_store` | MockSnapshotStore returns `Ok(Some(data))` for present pages. Verify the fault handler path uses `uffd.copy()` (existing tests validate this implicitly; add explicit assertion that `store.page_reads > 0`). |
| Present page: no regression | AC3.3 | Integration | Existing UFFD tests (`test_uffd_demand_page.rs`, `test_uffd_preload.rs`, etc.) | All existing UFFD tests | `make test FEATURE_FLAGS="--features embedded_init"` | Existing UFFD tests do NOT enable balloon. Their continued passing proves the `Ok(Some(...))` path is unbroken. No new test needed. |
| `zeropage` EEXIST handled identically to `copy` EEXIST (silently ignored) | AC3.4 | Unit | `src/vmm/src/uffd.rs` | `test_is_eexist_zeropage_variant` | `cargo test -p vmm --features uffd -- is_eexist` | Construct `userfaultfd::Error::ZeropageFailed(nix::errno::Errno::EEXIST)`, call `is_eexist()`, assert returns true. Also verify non-EEXIST errno returns false. |

---

## mem-balloon.AC4: Rust API enables inflate->snapshot workflow

| Criterion | ID | Test Type | Test File | Test Name(s) | Run Command | Notes |
|---|---|---|---|---|---|---|
| `Builder::enable_balloon()` creates balloon device during build | AC4.1 | Integration | `tests/test_cases/src/test_balloon_api.rs` | `balloon-api-enable` | `make test FEATURE_FLAGS="--features embedded_init"` | Call `builder.enable_balloon()`, build VM, verify `vm_handle.balloon()` returns `Some`. Guest boots and runs successfully (balloon device present on MMIO bus). |
| `VmHandle::balloon()` returns `Some` when enabled, `None` when not | AC4.2 | Integration | `tests/test_cases/src/test_balloon_api.rs` | `balloon-api-none-when-disabled` | `make test FEATURE_FLAGS="--features embedded_init"` | Build VM WITHOUT calling `enable_balloon()`. Assert `vm_handle.balloon()` returns `None`. |
| `BalloonHandle::resize(target_mb)` triggers guest inflation | AC4.3 | Unit | `src/libkrun/src/lib.rs` (or dedicated module) | `test_balloon_resize_sets_num_pages` | `cargo test -p libkrun` | Create BalloonHandle with mock Balloon (activated), call `resize(128)`, verify `config.num_pages == 128 * 256` (128MB / 4KB). |
| `BalloonHandle::resize(target_mb)` triggers guest inflation | AC4.3 | Integration | `tests/test_cases/src/test_balloon_api.rs` | `balloon-api-resize` | `make test FEATURE_FLAGS="--features embedded_init"` | Host calls `resize(64)` on a 256MB VM. Poll `actual()` until it increases above 0, or use `await_target`. Guest confirms memory pressure via guest-side stats. |
| `await_target` returns `Reached` when met, `Stalled` when stuck, `Err(Timeout)` when exceeded | AC4.4 | Unit | `src/libkrun/src/lib.rs` (or dedicated module) | `test_await_target_reached`, `test_await_target_stalled`, `test_await_target_timeout` | `cargo test -p libkrun` | **Reached**: spawn thread that updates condvar to target value, assert `Ok(BalloonResult::Reached(...))`. **Stalled**: no condvar update, stall_timeout fires, assert `Ok(BalloonResult::Stalled(...))`. **Timeout**: set short max_timeout, no progress, assert `Err(BalloonError::Timeout{..})`. |
| `await_target` returns `Reached` | AC4.4 | Integration | `tests/test_cases/src/test_balloon_api.rs` | `balloon-api-await-reached` | `make test FEATURE_FLAGS="--features embedded_init"` | Host calls `resize(32)` then `await_target(32, 2s, Some(30s))`. Guest inflates. Assert returns `BalloonResult::Reached`. |
| Balloon device state survives snapshot/restore | AC4.5 | Unit | `src/devices/src/virtio/balloon/device.rs` | `test_balloon_snapshot_roundtrip`, `test_balloon_snapshot_empty_data_no_panic`, `test_balloon_snapshot_no_bitmaps` | `cargo test -p devices --features net,snapshot -- balloon` | Save state, create new Balloon, restore state. Verify `num_pages`, `actual`, hinting fields match. Verify `stats_desc_index` reset to None. Verify bitmaps remain None. |
| Balloon state survives snapshot/restore | AC4.5 | Integration | `tests/test_cases/src/test_balloon_snapshot.rs` | `balloon-snapshot-restore-state` | `make test FEATURE_FLAGS="--features embedded_init"` | Inflate to 64MB, snapshot, restore. After restore, assert `actual()` shows ~64MB (retained from pre-snapshot). Guest continues operating. |
| `resize` on inactive device returns `Err(DeviceNotActive)` | AC4.6 | Unit | `src/libkrun/src/lib.rs` (or dedicated module) | `test_resize_inactive_device_error` | `cargo test -p libkrun` | Create BalloonHandle with Balloon in `DeviceState::Inactive`. Call `resize(64)`. Assert `Err(BalloonError::DeviceNotActive)`. |
| `await_target` with `max_timeout = None` and stalled guest returns `Stalled` | AC4.7 | Unit | `src/libkrun/src/lib.rs` (or dedicated module) | `test_await_target_no_max_timeout_stalled` | `cargo test -p libkrun` | Call `await_target(target, stall_timeout=100ms, max_timeout=None)` with no condvar updates. Assert returns `Ok(BalloonResult::Stalled(...))` within ~100ms (does not hang). |
| Concurrent resize updates target; waiters see new target | AC4.8 | Unit | `src/libkrun/src/lib.rs` (or dedicated module) | `test_concurrent_resize_updates_target` | `cargo test -p libkrun` | Thread A calls `await_target(100, ...)`. Thread B calls `resize(200)`. Simulate condvar updates reaching 200. Assert Thread A's `await_target` eventually sees the new target (returns `Reached` at 200, or `Stalled` if it was waiting for 100). The key invariant: no deadlock, no panic. |

---

## Human Verification

The following aspects require human verification because they depend on runtime conditions, kernel behavior, or performance characteristics that cannot be reliably automated in CI.

### HV-1: MADV_DONTNEED actually releases physical memory (AC1.1)

**Justification:** Unit tests verify the `madvise()` syscall is called and does not error, but verifying that host RSS actually decreases requires reading `/proc/self/status` or `smaps` before/after, which is sensitive to kernel memory accounting timing, transparent huge pages, and other host-side memory management. Integration tests can observe `actual()` increasing, but not host-side RSS reduction.

**Verification approach:**
1. Run a manual test with a 1GB+ VM
2. Inflate balloon to 75%
3. Observe host RSS (via `htop` or `/proc/<pid>/status` VmRSS) decrease by approximately the inflated amount
4. Deflate and observe RSS increase as guest re-faults pages

### HV-2: Free page reporting performance (AC1.5, AC2.4)

**Justification:** The design specifies ~7ms for 14GB free page reporting and ~1-10ms for mincore verification. These are performance targets, not correctness criteria. CI timing is too variable for performance assertions.

**Verification approach:**
1. Run a manual test with a 16GB VM
2. Enable balloon with free page reporting
3. Time the PHQ/FRQ cycle and mincore verification via debug logging
4. Verify performance is within the design envelope (7ms for reporting, 1-10ms for mincore)

### HV-3: Snapshot size reduction proportional to reclaimed pages (AC2.3)

**Justification:** While the integration test (`balloon-snapshot-size-reduction`) checks that the snapshot is smaller, the exact proportionality depends on filesystem block allocation, sparse file behavior, and compression (if any). The test verifies "smaller" but not "proportional".

**Verification approach:**
1. Run manual tests with varying balloon inflation levels (25%, 50%, 75%)
2. Measure snapshot file sizes at each level
3. Verify approximately linear relationship between inflation percentage and size reduction
4. Document the observed ratio for future reference

### HV-4: UFFD zeropage performance advantage (AC3.1)

**Justification:** The design specifies ~0.1us for zeropage vs ~10us+ for store read + copy. This is a performance target. The correctness (guest sees zeros) is tested automatically, but the latency improvement requires profiling.

**Verification approach:**
1. Run UFFD restore with and without balloon inflation
2. Compare page fault resolution times from PageTracker stats
3. Verify zero-filled pages are resolved significantly faster than store-read pages

### HV-5: Guest balloon driver feature negotiation (AC1.1, AC1.2)

**Justification:** Feature negotiation depends on the specific Linux kernel version in the guest. The design assumes kernel 7.0+ for full feature support. Unit tests verify the host advertises correct features, but cannot verify guest driver acceptance.

**Verification approach:**
1. Boot VM with guest kernel 7.0+
2. Check `dmesg | grep balloon` for successful feature negotiation
3. Verify `/sys/bus/virtio/devices/virtio*/features` shows expected negotiated features
4. Verify `/sys/devices/system/memory/` reflects balloon inflation/deflation

### HV-6: macOS snapshot path no-regression (AC2.9, platform-specific)

**Justification:** Balloon snapshot integration is gated with `#[cfg(target_os = "linux")]`. The macOS path must be verified to produce identical output (empty excluded_pages) since CI may not test macOS. This is effectively verified by the `#[cfg]` gate making the code unreachable on macOS, but a human should confirm the gate is correct.

**Verification approach:**
1. On macOS: build with snapshot feature, verify compilation succeeds
2. Take a snapshot, verify `excluded_pages` is empty in vmstate
3. Restore, verify identical behavior to pre-balloon baseline
