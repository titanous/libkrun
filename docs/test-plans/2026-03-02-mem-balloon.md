# Human Test Plan: Memory Balloon Device

**Feature:** virtio memory balloon device with snapshot/UFFD integration
**Design plan:** `docs/design-plans/2026-03-02-mem-balloon.md`
**Implementation plan:** `docs/implementation-plans/2026-03-02-mem-balloon/`
**Branch:** `mem-balloon`

## Scope

This plan covers tests that require a full guest VM with a Linux balloon driver. Unit tests
(all 6 missing unit tests added, all AC criteria with `cargo test` commands) are automated and
pass on CI. The tests below require manual execution.

**Automated test coverage status:** PASS (all unit criteria covered)
**Integration tests:** 9 scenarios below require full VM boot with guest balloon driver.

---

## Prerequisites

- libkrun built with `embedded_init` feature: `make FEATURE_FLAGS="--features embedded_init"`
- Guest kernel 7.0+ with `virtio_balloon` module loaded
- Sufficient host RAM for VM + balloon inflation headroom
- Debug logging enabled: `RUST_LOG=debug` for performance measurements

---

## Phase 1: Balloon Inflate/Deflate Correctness (AC1.1, AC1.2, AC1.4, AC1.5)

### Test 1.1: Basic inflate/deflate cycle (`balloon-inflate-basic`, `balloon-inflate-deflate-cycle`)

**Goal:** Verify host `BalloonHandle::resize()` causes guest driver to inflate, and `actual()` reflects it.

**Setup:**
1. Build a 256MB VM with `Builder::enable_balloon()`
2. Boot the VM and wait for guest to be ready

**Steps:**
1. Call `balloon_handle.resize(64)` (inflate to 64MB)
2. Call `balloon_handle.await_target(64, Duration::from_secs(5), Some(Duration::from_secs(30)))`
3. Assert result is `Ok(BalloonResult::Reached(actual))` where `actual >= 60` (within 4MB tolerance)
4. Call `balloon_handle.resize(0)` (full deflate)
5. Poll `balloon_handle.actual()` until it drops below 4
6. Guest: write to pages that were previously inflated, verify no crash or SIGBUS

**Expected:** Inflate completes within 30s; deflate restores guest access to memory.

---

### Test 1.2: Stats collection (`balloon-stats`)

**Goal:** Verify `BalloonHandle::stats()` returns valid memory counters after guest driver activates.

**Setup:**
1. Build a 256MB VM with `Builder::enable_balloon()`
2. Boot VM, wait for guest idle

**Steps:**
1. Call `balloon_handle.stats()`; initially returns `None` (stats not yet fetched)
2. Wait up to 5 seconds for guest driver to process stats request
3. Call `balloon_handle.stats()` again
4. Assert result is `Some(BalloonStats)` with `free_memory`, `total_memory`, `available_memory` all `Some` and non-zero
5. Inflate balloon 32MB, request stats again
6. Assert `free_memory` decreased by approximately 32MB

**Expected:** Stats reflect actual guest memory conditions.

---

### Test 1.3: Free page hint queue protocol (`balloon-inflate-deflate-cycle`)

**Goal:** Verify PHQ (page hinting queue) cycles complete correctly with START→data→STOP protocol.

**Setup:**
1. Build VM with `Builder::enable_balloon()` and free page hint support
2. Boot VM

**Steps:**
1. Enable free page reporting in guest (`echo 1 > /proc/sys/vm/page_reporting`)
2. Observe `RUST_LOG=debug` output for `balloon: process_phq` log entries
3. Verify PHQ cycles: device logs `START cmd`, processes memory range descriptors, logs `DONE`
4. Verify host memory decreases (check via PHQ completion count in logs)

**Expected:** PHQ cycles complete without errors; logged command ID matches expected protocol.

---

## Phase 2: Snapshot with Balloon (AC2.3, AC2.4, AC2.5)

### Test 2.1: Snapshot size reduction proportional to inflation (`balloon-snapshot-size-reduction`)

**Goal:** Full snapshot of a VM with inflated balloon is smaller than without balloon.

**Setup:**
1. Boot two identical 256MB VMs — one with balloon at 0MB, one with balloon at 128MB
2. Let both reach idle state

**Steps:**
1. Take baseline snapshot (no balloon inflation): measure size of `memory` file
2. Inflate balloon in second VM to 128MB, await target reached
3. Take snapshot: measure size of `memory` file
4. Compare sizes: inflated snapshot should be measurably smaller (expect ~50% reduction for 50% inflation)
5. Restore from inflated snapshot
6. Verify guest boots and runs correctly

**Expected:** Snapshot with 128MB inflation is noticeably smaller than baseline. Restored guest operates correctly.

---

### Test 2.2: Free page reporting excluded from snapshot (`balloon-snapshot-free-page-reporting`)

**Goal:** Pages reported free via FRQ (free page reporting queue) are excluded from snapshot when non-resident.

**Setup:**
1. Boot 256MB VM with balloon enabled
2. Enable kernel free page reporting: `echo 1 > /sys/kernel/mm/page_reporting/page_reporting_order`

**Steps:**
1. Wait for FRQ cycle to complete (observe `balloon: should release guest_addr=` in debug logs)
2. Take snapshot
3. Verify snapshot size is reduced (pages freed by FRQ are excluded via mincore)
4. Restore from snapshot
5. Guest: write to pages that were previously freed — pages should now be zeros

**Expected:** Guest sees zeros for previously-freed pages after restore; no crash.

---

### Test 2.3: Incremental snapshot with reclaimed pages (`balloon-incremental-snapshot-reclaimed`)

**Goal:** Incremental snapshot records newly-reclaimed pages; restore zeros those pages.

**Setup:**
1. Boot 256MB VM
2. Take base snapshot (no balloon)

**Steps:**
1. Inflate balloon 64MB
2. Take incremental snapshot
3. Verify incremental snapshot is smaller than base (reclaimed pages excluded)
4. Restore from base + incremental chain
5. Guest: verify pages previously inflated are zeros after restore
6. Guest: verify non-inflated pages retain correct data

**Expected:** Incremental restore correctly zeros reclaimed pages while preserving non-reclaimed data.

---

## Phase 3: UFFD Restore with Balloon (AC3.1)

### Test 3.1: UFFD zero-fill for reclaimed pages (`balloon-uffd-zero-fill`)

**Goal:** UFFD restore resolves faults for reclaimed pages by zero-filling (not reading from store).

**Setup:**
1. Build 256MB VM with balloon + `uffd` feature
2. Boot VM, inflate balloon 64MB, await target

**Steps:**
1. Take snapshot with balloon inflated
2. Restore VM from snapshot using UFFD path (`restore_from_store_with_uffd`)
3. In guest: access pages that were previously inflated (should be accessible and zero)
4. Observe `RUST_LOG=debug` logs for `LoadSource::Zero` pages vs `LoadSource::Store` pages
5. Verify `PageTrackerStats.zero_pages > 0` and equals approximately inflation count
6. Guest: write to previously-inflated pages — verify write succeeds (no SIGBUS)

**Expected:** Reclaimed pages resolved via zeropage (faster than store read). Guest sees zeros, not stale data.

---

## Phase 4: Rust API Integration (AC4.1, AC4.2, AC4.3, AC4.4 integration)

### Test 4.1: Enable/disable balloon API (`balloon-api-enable`, `balloon-api-none-when-disabled`)

**Goal:** `VmHandle::balloon()` returns correct value based on whether `enable_balloon()` was called.

**Steps:**
1. Build VM with `enable_balloon()` — assert `vm_handle.balloon()` returns `Some`
2. Build VM without `enable_balloon()` — assert `vm_handle.balloon()` returns `None`
3. In case 1: verify guest boots successfully with balloon device on MMIO bus (`dmesg | grep balloon`)

**Expected:** API correctly reflects device presence; guest driver negotiates features.

---

### Test 4.2: Resize and await_target integration (`balloon-api-resize`, `balloon-api-await-reached`)

**Goal:** End-to-end: host resize triggers guest inflation, `await_target` returns `Reached`.

**Setup:**
1. Boot 256MB VM with balloon enabled

**Steps:**
1. Call `balloon_handle.resize(32)`
2. Call `balloon_handle.await_target(32, Duration::from_secs(2), Some(Duration::from_secs(30)))`
3. Assert result is `Ok(BalloonResult::Reached(actual))` where `actual >= 28`
4. Log actual value and time taken

**Expected:** Balloon inflates within 30s; await_target returns Reached, not Stalled or Timeout.

---

## End-to-End Scenarios

### E2E-1: Full lifecycle — inflate → snapshot → UFFD restore → deflate

**Goal:** Validate the complete workflow described in the design plan.

**Steps:**
1. Boot 512MB VM with balloon + UFFD support
2. Inflate balloon to 256MB, await target reached
3. Take snapshot
4. Stop VM
5. Restore from snapshot using UFFD
6. Verify guest boots correctly
7. Verify `balloon_handle.actual() >= 248` (state preserved)
8. Deflate balloon: `resize(0)`, await target
9. Guest: perform memory-intensive workload — verify no crashes
10. Take second snapshot (no balloon inflation) — verify size larger than step 3 snapshot

**Expected:** Entire workflow completes without errors; snapshot size correctly reflects balloon state.

---

### E2E-2: Free page reporting — PHQ/FRQ verification

**Goal:** Verify PHQ (page hinting) and FRQ (free page reporting) both reduce snapshot size.

**Steps:**
1. Boot 1GB VM with balloon and free page reporting
2. Enable free page reporting: `echo 1 > /sys/kernel/mm/page_reporting/page_reporting_order`
3. Run guest workload that allocates then frees memory
4. Wait for PHQ/FRQ cycles to complete (observe debug logs)
5. Take baseline snapshot with PHQ/FRQ inactive
6. Let PHQ/FRQ complete, take snapshot
7. Compare sizes: PHQ/FRQ snapshot should be significantly smaller
8. Restore from PHQ/FRQ snapshot, verify guest integrity

**Expected:** PHQ/FRQ reduces snapshot size; restored guest correctly sees zeros for reported-free pages.

---

## Human Verification Criteria

These aspects require manual observation and cannot be reliably automated in CI.

### HV-1: MADV_DONTNEED actually releases physical memory (AC1.1)

**Justification:** Unit tests verify `madvise()` is called; RSS reduction requires human observation.

**Verification:**
1. Run manual test with a 1GB+ VM
2. Inflate balloon to 75%
3. Observe host RSS via `htop` or `cat /proc/<libkrun-pid>/status | grep VmRSS`
4. Assert RSS decreases by approximately the inflated amount
5. Deflate and observe RSS increase as guest re-faults pages

**Pass criterion:** RSS decrease visible and roughly proportional to inflation amount.

---

### HV-2: Free page reporting performance (AC1.5, AC2.4)

**Justification:** Design specifies ~7ms for 14GB free page reporting and ~1-10ms for mincore. CI timing varies.

**Verification:**
1. Run 16GB VM with balloon + free page reporting
2. Enable `RUST_LOG=debug` logging
3. Time PHQ/FRQ cycle from first `process_phq` log to `DONE` log
4. Time mincore verification from `mincore_check start` to completion
5. Assert PHQ/FRQ completes in <100ms for 16GB; mincore in <50ms

**Pass criterion:** Performance within design envelope (7ms per 14GB FRQ, 1-10ms mincore).

---

### HV-3: Snapshot size reduction proportional to reclaimed pages (AC2.3)

**Justification:** Exact proportionality depends on filesystem sparse file behavior.

**Verification:**
1. Run tests with 25%, 50%, 75% balloon inflation of a 1GB VM
2. Measure snapshot `memory` file sizes at each level
3. Verify approximately linear relationship

**Pass criterion:** ~25% size reduction at 25% inflation, ~50% at 50%, ~75% at 75% (±5%).

---

### HV-4: UFFD zeropage performance advantage (AC3.1)

**Justification:** Design specifies ~0.1µs for zeropage vs ~10µs+ for store read+copy.

**Verification:**
1. Run UFFD restore with 50% balloon inflation of a 256MB VM
2. Check `PageTrackerStats` at restore completion: log `zero_pages` and `store_pages` counts
3. Compare total time to restore with vs without balloon inflation (zeropage path should be faster)

**Pass criterion:** Zeropage pages resolve meaningfully faster per-page than store pages in logs.

---

### HV-5: Guest balloon driver feature negotiation (AC1.1, AC1.2)

**Justification:** Feature negotiation depends on the specific Linux kernel version in the guest.

**Verification:**
1. Boot VM with guest kernel 7.0+
2. Check `dmesg | grep -i balloon` for successful feature negotiation
3. Verify `/sys/bus/virtio/devices/virtio*/features` shows expected features:
   - `VIRTIO_BALLOON_F_STATS_VQ` (bit 5)
   - `VIRTIO_BALLOON_F_FREE_PAGE_HINT` (bit 7)
   - `VIRTIO_BALLOON_F_REPORTING` (bit 9)
4. Inflate balloon, verify `/sys/devices/system/memory/` reflects reduction

**Pass criterion:** All 3 features negotiated; guest driver reports successful activation.

---

### HV-6: macOS snapshot path no-regression (AC2.9, platform-specific)

**Justification:** Balloon snapshot integration is `#[cfg(target_os = "linux")]`; macOS path must produce empty `excluded_pages`.

**Verification:**
1. On macOS: `cargo build -p vmm --features snapshot` — assert compilation succeeds
2. Take a snapshot of a running VM
3. Inspect vmstate (deserialize): verify `excluded_pages` field is empty (`[]`)
4. Restore from snapshot: verify identical behavior to pre-balloon baseline

**Pass criterion:** macOS builds and snapshots without regression; `excluded_pages` always empty on macOS.

---

## Deferred Integration Tests

The following integration tests require a full VM with a running guest balloon driver. These are
tracked as follow-up work and require the `tests/test_cases/` host/guest framework with the
relevant test files created:

| Test File (to create) | Test Name | AC |
|---|---|---|
| `tests/test_cases/src/test_balloon_inflate.rs` | `balloon-inflate-basic` | AC1.1 |
| `tests/test_cases/src/test_balloon_inflate.rs` | `balloon-inflate-deflate-cycle` | AC1.2 |
| `tests/test_cases/src/test_balloon_stats.rs` | `balloon-stats` | AC1.4 |
| `tests/test_cases/src/test_balloon_snapshot.rs` | `balloon-snapshot-size-reduction` | AC2.3 |
| `tests/test_cases/src/test_balloon_snapshot.rs` | `balloon-snapshot-free-page-reporting` | AC2.4 |
| `tests/test_cases/src/test_balloon_snapshot.rs` | `balloon-incremental-snapshot-reclaimed` | AC2.5 |
| `tests/test_cases/src/test_balloon_uffd.rs` | `balloon-uffd-zero-fill` | AC3.1 |
| `tests/test_cases/src/test_balloon_api.rs` | `balloon-api-enable`, `balloon-api-none-when-disabled` | AC4.1, AC4.2 |
| `tests/test_cases/src/test_balloon_api.rs` | `balloon-api-resize`, `balloon-api-await-reached` | AC4.3, AC4.4 |
