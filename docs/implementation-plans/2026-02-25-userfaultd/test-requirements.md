# Userfaultd Test Requirements

Maps every acceptance criterion from the [userfaultd design](../../design-plans/2026-02-25-userfaultd.md) to either an automated test or documented human verification. Rationalized against implementation decisions in phases 1-6.

---

## AC1: SnapshotStore Trait

### userfaultd.AC1.1 -- `SnapshotStore` trait has six methods: `read_vmstate`, `read_page`, `preload`, `write_vmstate`, `write_pages`, `close`

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/snapshot_store.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 1, Task 1 |
| **Criterion** | A mock struct implements all six methods of `SnapshotStore`. Compilation succeeds. |
| **Approach** | Compile-time verification. Test creates a `MockSnapshotStore` struct with all six method implementations. If any method is missing or has the wrong signature, the build fails. |
| **Command** | `cargo test -p vmm --features snapshot` |

### userfaultd.AC1.2 -- All methods return `BoxFuture<'_, io::Result<T>>` (or `BoxStream` for `preload`)

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/snapshot_store.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 1, Task 1 |
| **Criterion** | Mock implementation returns correctly-typed `BoxFuture`/`BoxStream` values. Compilation verifies return types match the trait definition. |
| **Approach** | Compile-time verification via the same mock struct as AC1.1. Each mock method body returns `Box::pin(async { ... })` or `Box::pin(futures::stream::empty())`. Type mismatch would be a compile error. |
| **Command** | `cargo test -p vmm --features snapshot` |

### userfaultd.AC1.3 -- Trait is `Send + Sync + 'static` and object-safe (`Box<dyn SnapshotStore>` compiles)

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/snapshot_store.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 1, Task 1 |
| **Criterion** | Test constructs `Box<dyn SnapshotStore>` from a `MockSnapshotStore`. Static assertions confirm `MockSnapshotStore: Send + Sync + 'static`. |
| **Approach** | Runtime test that boxes the mock as `Box<dyn SnapshotStore>`. Compile-time assertion function `fn assert_send_sync<T: Send + Sync + 'static>() {}` called with `assert_send_sync::<MockSnapshotStore>()`. If trait is not object-safe or bounds are missing, compilation fails. |
| **Command** | `cargo test -p vmm --features snapshot` |

### userfaultd.AC1.4 -- `SnapshotStoreFactory` trait has one method (`create`), returns `BoxFuture<'static, io::Result<Box<dyn SnapshotStore>>>`, is `Send + 'static`

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/snapshot_store.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 1, Task 1 |
| **Criterion** | Test creates a `MockSnapshotStoreFactory`, boxes it as `Box<dyn SnapshotStoreFactory>`, and calls `create()`. Compilation verifies the method signature and bounds. |
| **Approach** | Runtime test that boxes the factory and invokes `create()` inside a tokio block_on. Compile-time assertion `assert_send::<MockSnapshotStoreFactory>()` verifies `Send + 'static`. Return type verified by assigning result to `Box<dyn SnapshotStore>`. |
| **Command** | `cargo test -p vmm --features snapshot` |

### userfaultd.AC1.5 -- No snapshot IDs or lineage concepts in libkrun

| Field | Value |
|-------|-------|
| **Test type** | Human verification |
| **Phase** | 1, Task 1 |
| **Justification** | This is a negative requirement about the absence of concepts in the API surface. No compile-time or runtime test can prove something does not exist across all code. |
| **Verification approach** | Code review of `SnapshotStore` and `SnapshotStoreFactory` trait definitions in `src/vmm/src/snapshot_store.rs`. Verify: (1) no method parameter or return type contains "id", "lineage", "parent", or "chain" fields; (2) no associated types for identity; (3) factory `create` takes `self: Box<Self>` with no identity arguments. Also confirmed by compile-time: the mock implementations in AC1.1-AC1.4 tests have no ID parameters. |

---

## AC2: FsSnapshotStore

### userfaultd.AC2.1 -- `FsSnapshotStore` implements `SnapshotStore` for filesystem-backed snapshots

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/snapshot_store.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 1, Task 2 |
| **Criterion** | `FsSnapshotStore` is used as `Box<dyn SnapshotStore>`. Compilation succeeds. |
| **Approach** | Test constructs an `FsSnapshotStore` and assigns it to `Box<dyn SnapshotStore>`. If any trait method is missing or has the wrong signature, the build fails. |
| **Command** | `cargo test -p vmm --features snapshot` |

### userfaultd.AC2.2 -- Write path produces files compatible with current snapshot format (`vmstate` + `memory` files)

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/snapshot_store.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 1, Task 2 |
| **Criterion** | Write vmstate + memory via `FsSnapshotStore`, read back with existing `load_vmstate`/`load_memory` functions, data matches. |
| **Approach** | Test creates a temp directory, constructs `FsSnapshotStore`, calls `write_vmstate(known_bytes)` + `write_pages(known_pages)` + `close()`. Reads files back using existing snapshot loading functions. Asserts byte-for-byte equality. This proves format compatibility: if the existing functions can parse the output, the format is unchanged. |
| **Command** | `cargo test -p vmm --features snapshot` |

### userfaultd.AC2.3 -- Read path supports base + N incremental overlays; `read_page` resolves latest version

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/snapshot_store.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 2, Task 1 |
| **Criterion** | With base + 2 incrementals, `read_page` returns data from the latest incremental for multiply-dirty pages, and base data for clean pages. |
| **Approach** | Test creates a temp directory with a synthetic base snapshot (vmstate + memory) and two incremental snapshots with known dirty pages. Page X dirty in inc1 only: `read_page(X)` returns inc1 data. Page Y dirty in both inc1 and inc2: `read_page(Y)` returns inc2 data (newest wins). Page Z clean: `read_page(Z)` returns base data. Implementation decision: `FsSnapshotStoreFactory::create()` builds a `dirty_page_index` HashMap with newest-first resolution (phase_02.md Task 1). |
| **Command** | `cargo test -p vmm --features snapshot` |

### userfaultd.AC2.4 -- `preload` yields memory in sequential chunks (4MB default)

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/snapshot_store.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 2, Task 1 |
| **Criterion** | `preload()` stream yields chunks covering the full memory range, in sequential order, with 4MB chunk size (except possibly the last chunk). |
| **Approach** | Test creates a synthetic base snapshot with known memory layout. Calls `preload(regions)` and collects all yielded `(guest_addr, data)` tuples. Asserts: (1) concatenated guest_addrs are monotonically increasing; (2) each `data.len()` is 4MB except possibly the last; (3) total bytes yielded equals total memory size. Implementation decision: FsSnapshotStore reads base memory file in 4MB sequential chunks with dirty page overlay applied inline (phase_02.md Task 1). |
| **Command** | `cargo test -p vmm --features snapshot` |

### userfaultd.AC2.5 -- Existing `restore_and_run(path, incrementals)` delegates to `restore_and_run_with_store` using `FsSnapshotStoreFactory`

| Field | Value |
|-------|-------|
| **Test type** | Integration (e2e) |
| **Test file** | `tests/test_cases/src/test_snapshot_restore.rs` (existing) |
| **Phase** | 2, Task 2 |
| **Criterion** | Existing `snapshot-restore-full` and `snapshot-restore-incremental` integration tests pass without modification after the delegation change. |
| **Approach** | Run existing integration tests. `Context::restore_and_run(path, incrementals)` now internally constructs `FsSnapshotStoreFactory` and calls `restore_and_run_with_store`. If the delegation breaks anything, existing tests fail. No new test code needed -- existing tests serve as regression coverage. Implementation decision: `restore_and_run` becomes a thin wrapper per phase_02.md Task 2. |
| **Command** | `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-restore-full` and `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-restore-incremental` |

---

## AC3: UFFD Handler

### userfaultd.AC3.1 -- `UffdHandler` creates UFFD fd, registers guest memory regions, runs on dedicated thread with tokio runtime

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/uffd.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 3, Task 2 and Task 4 |
| **Criterion** | Construct `UffdHandler` with anonymous mmap'd memory. Verify UFFD fd is created, region is registered (no error), and handler runs on a named thread with a tokio runtime. |
| **Approach** | Test mmaps an anonymous region, creates `UffdHandler::new()` with the region, and calls `run()`. Verifies: (1) no panic or error from UFFD creation; (2) no panic or error from region registration; (3) handler thread is spawned (JoinHandle is valid). Task 4 wires this into builder.rs with vmstate exchange via oneshot channel. Requires Linux with UFFD support. Implementation decision: UFFD created with `close_on_exec(true)`, `non_blocking(true)`, `user_mode_only(true)` per phase_03.md Task 2. Thread uses `tokio::runtime::Builder::new_current_thread()` per same task. |
| **Command** | `cargo test -p vmm --features uffd` |

### userfaultd.AC3.2 -- Fault loop uses `AsyncFd<Uffd>` (non-blocking) and `tokio::spawn` per fault for parallel resolution

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/uffd.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 3, Task 3 |
| **Criterion** | Set up UFFD with anonymous mmap'd memory and a mock SnapshotStore. Trigger a page fault from another thread. Verify the mock store's `read_page` was called and the page contains expected data. |
| **Approach** | Test mmaps anonymous memory, registers with UFFD, starts handler with a mock store that returns known page data. A second thread reads from the mmap'd region (triggering a page fault). The fault handler calls `store.read_page()` via `tokio::spawn`, copies data via `uffd.copy()`. Test verifies: (1) `read_page` was called (tracked by mock); (2) the page contains the expected data pattern. Implementation decision: `AsyncFd::new(UffdFd(...))` wraps `Arc<Uffd>` via newtype for `AsRawFd` per phase_03.md Task 3. |
| **Command** | `cargo test -p vmm --features uffd` |

### userfaultd.AC3.3 -- Each fault task calls `store.read_page(guest_addr)` then `uffd.copy()`; EEXIST return is silently ignored

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/uffd.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 3, Task 3 |
| **Criterion** | EEXIST from `uffd.copy()` does not cause panic or error propagation. No `VmExit::Error` is signaled. |
| **Approach** | Test pre-populates a page (via direct mmap write or a first successful `uffd.copy()`), then triggers a second fault resolution for the same page. The second `uffd.copy()` returns EEXIST. Verify: (1) no panic; (2) `SharedVmExit` remains `None`. Implementation decision: EEXIST detected via `userfaultfd::Error::SystemError(errno) if *errno == libc::EEXIST` per phase_03.md Task 3. |
| **Command** | `cargo test -p vmm --features uffd` |

### userfaultd.AC3.4 -- Preload task runs concurrently: consumes `store.preload()` stream, UFFDIO_COPY per chunk, EEXIST ignored

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/uffd.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 4, Task 1 |
| **Criterion** | Mock store with a preload stream yielding known data. Pages are populated via preload (not fault handler). Preload stream is fully consumed. EEXIST between preload and concurrent fault does not cause errors. |
| **Approach** | Test creates mock store whose `preload()` yields known chunks. Starts UFFD handler. After handler drains preload, verifies pages contain expected data. Also tests EEXIST race: store yields a chunk that overlaps with a page concurrently faulted -- neither path errors. Implementation decision: preload and fault loop run as concurrent tokio tasks via `tokio::spawn` per phase_04.md Task 1. |
| **Command** | `cargo test -p vmm --features uffd` |

### userfaultd.AC3.5 -- UFFDIO_COPY uses multi-page `len` for preload chunks (not per-page calls)

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/uffd.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 4, Task 1 |
| **Criterion** | Preload task passes the full chunk length to `uffd.copy()`, not page-sized lengths. |
| **Approach** | Test with mock store whose `preload()` yields a multi-page chunk (e.g., 4 pages = 16KB on x86_64). Verify: (1) `uffd.copy()` is called with `len = chunk_data.len()` (full chunk), not `len = PAGE_SIZE`; (2) all pages in the chunk are populated with correct data. Verification can be done by checking `data.len()` passed to the copy call via instrumentation in the mock, or by verifying the result -- all 4 pages contain expected data after a single preload chunk. Implementation decision: `uffd.copy(src, dst, data.len(), true)` per phase_04.md Task 1. |
| **Command** | `cargo test -p vmm --features uffd` |

### userfaultd.AC3.6 -- Fatal `read_page` error signals VMM stop via `VmExit::Error`; preload stream errors are non-fatal

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/uffd.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 3 (fatal) + 4 (preload non-fatal) |
| **Criterion** | (a) When `read_page` returns `Err`, `SharedVmExit` is set to `VmExit::Error` with a message. (b) When preload stream yields `Err`, preload stops but fault handler continues serving pages. |
| **Approach** | Two separate tests: (a) Fatal: mock store's `read_page` returns `Err(io::Error)`. Trigger a page fault. Verify `SharedVmExit` contains `VmExit::Error` with the error message. (b) Non-fatal: mock store's `preload()` yields an error after a few good chunks. Verify preload stops (log output or mock tracking). Then trigger a page fault on an un-preloaded page. Verify `read_page` is called and the page is served successfully. Implementation decision: fatal errors use `signal_error()` helper that locks `SharedVmExit` per phase_03.md Task 3; preload errors logged with `log::warn!` and `break` per phase_04.md Task 1. |
| **Command** | `cargo test -p vmm --features uffd` |

---

## AC4: Backward Compatibility

### userfaultd.AC4.1 -- `Context::restore_and_run(path, incrementals)` still works, delegates internally

| Field | Value |
|-------|-------|
| **Test type** | Integration (e2e) |
| **Test file** | `tests/test_cases/src/test_snapshot_restore.rs` (existing) |
| **Phase** | 2, Task 2 |
| **Criterion** | Existing `snapshot-restore-full` integration test passes without modification. |
| **Approach** | Run existing integration test. `restore_and_run` now internally delegates to `restore_and_run_with_store` via `FsSnapshotStoreFactory`. If delegation breaks behavior, the test fails. No new test code needed. Implementation decision: `restore_and_run` is a thin wrapper per phase_02.md Task 2. |
| **Command** | `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-restore-full` |

### userfaultd.AC4.2 -- `VmHandle::snapshot(path)` and `incremental_snapshot(path)` still work, delegate internally

| Field | Value |
|-------|-------|
| **Test type** | Integration (e2e) |
| **Test file** | `tests/test_cases/src/test_snapshot_restore.rs` (existing) |
| **Phase** | 1, Task 3 |
| **Criterion** | Existing `snapshot-restore-full` and `snapshot-restore-incremental` integration tests pass without modification. |
| **Approach** | Run existing integration tests. `VmHandle::snapshot(path)` now delegates to `snapshot_to_store` via `FsSnapshotStore`. If delegation breaks snapshot creation, the tests fail. Implementation decision: existing `create_snapshot` constructs `FsSnapshotStore` internally per phase_01.md Task 3. |
| **Command** | `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-restore-full` and `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-restore-incremental` |

### userfaultd.AC4.3 -- Existing snapshot integration tests pass without modification

| Field | Value |
|-------|-------|
| **Test type** | Integration (e2e) |
| **Test file** | `tests/test_cases/src/test_snapshot_restore.rs`, `test_snapshot_serial.rs`, `test_snapshot_block.rs`, `test_snapshot_incremental_state.rs`, `test_snapshot_net.rs`, `test_snapshot_errors.rs` (all existing) |
| **Phase** | 2, Task 2 |
| **Criterion** | All existing snapshot-related integration tests pass without any source modifications. |
| **Approach** | Run the full integration test suite. Every existing snapshot test exercises the path-based API which now delegates through `SnapshotStore`. Any regression in the delegation layer surfaces as a test failure. |
| **Command** | `make test FEATURE_FLAGS="--features embedded_init"` |

### userfaultd.AC4.4 -- No changes to snapshot file format

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/snapshot_store.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 1, Task 2 |
| **Criterion** | Files written by `FsSnapshotStore` are readable by existing `load_vmstate`/`load_memory` functions. |
| **Approach** | Covered by the AC2.2 roundtrip test: write via `FsSnapshotStore`, read back via existing snapshot functions. If the format changed, the existing functions would fail to parse. Additionally, AC4.3 (existing integration tests passing) provides end-to-end format compatibility proof. |
| **Command** | `cargo test -p vmm --features snapshot` |

---

## AC5: Integration Tests with Demand-Paging

### userfaultd.AC5.1 -- Test with empty-preload store: all pages loaded via UFFD faults, guest runs correctly

| Field | Value |
|-------|-------|
| **Test type** | Integration (e2e) |
| **Test file** | `tests/test_cases/src/test_uffd_demand_page.rs` (new) |
| **Phase** | 6, Task 2 |
| **Criterion** | VM snapshot restored with `EmptyPreloadStore` (preload returns empty stream). All pages demand-paged via UFFD faults. Guest verifies memory state survived restore and prints "OK". |
| **Approach** | Host: build VM, run guest, take snapshot, build new Context with `EmptyPreloadStoreFactory` wrapping snapshot path, cold restore via `restore_and_run_with_store`. Guest: set static counter to 42 before snapshot, verify counter is 42 after restore, write known pattern to heap allocation, print "OK". The `EmptyPreloadStore` wraps `FsSnapshotStore` and returns `futures::stream::empty()` for `preload()`, forcing every page through the fault handler. Implementation decision: mock stores are defined in `tests/test_cases/src/mock_snapshot_store.rs` per phase_06.md Task 1. |
| **Command** | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-demand-page-only` |

### userfaultd.AC5.2 -- Test with FsSnapshotStore: preload loads everything, near-zero faults

| Field | Value |
|-------|-------|
| **Test type** | Integration (e2e) |
| **Test file** | `tests/test_cases/src/test_uffd_preload.rs` (new) |
| **Phase** | 6, Task 3 |
| **Criterion** | VM snapshot restored with `FsSnapshotStoreFactory` (no wrapper). Preload stream loads all memory in 4MB chunks. Guest verifies memory state. Near-zero faults expected. |
| **Approach** | Host: build VM, run guest, take snapshot, restore via `FsSnapshotStoreFactory` and `restore_and_run_with_store`. Guest: same verification as AC5.1. "Near-zero faults" verified via PageTracker stats if accessible, or implicitly by test passing (preload races ahead of vCPU execution). Implementation decision: PageTracker from Phase 5 supports verification of near-zero faults via `stats().fault_pages` per phase_05.md. |
| **Command** | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-preload-full` |

### userfaultd.AC5.3 -- Test with partial-preload store: both preload and fault paths exercise

| Field | Value |
|-------|-------|
| **Test type** | Integration (e2e) |
| **Test file** | `tests/test_cases/src/test_uffd_preload.rs` (new) |
| **Phase** | 6, Task 4 |
| **Criterion** | VM snapshot restored with `PartialPreloadStore` yielding only first 50% of memory. Remaining pages served by fault handler. Both code paths exercised. Guest runs correctly. |
| **Approach** | Host: build VM, take snapshot, restore via `PartialPreloadStoreFactory`. Guest: same verification. The `PartialPreloadStore` wraps `FsSnapshotStore` and truncates the preload stream after yielding half the memory. Both preload and fault code paths must work for the guest to run correctly. Implementation decision: `PartialPreloadStore` defined in `mock_snapshot_store.rs` per phase_06.md Task 1. |
| **Command** | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-preload-partial` |

### userfaultd.AC5.4 -- Test incremental chain: base + 2 incrementals via demand-paging, latest dirty pages win

| Field | Value |
|-------|-------|
| **Test type** | Integration (e2e) |
| **Test file** | `tests/test_cases/src/test_uffd_incremental.rs` (new) |
| **Phase** | 6, Task 5 |
| **Criterion** | Base snapshot + 2 incrementals restored via demand-paging. Guest static counter was 100 at base, 200 at inc1, 300 at inc2. After restore, counter == 300 (latest dirty page wins). |
| **Approach** | Host: build VM, guest sets counter=100 and signals. Take base snapshot. Enable dirty tracking. Guest sets counter=200, take inc1. Guest sets counter=300, take inc2. Restore via `FsSnapshotStoreFactory::new(base, &[inc1, inc2])`. Guest verifies counter == 300 and prints "OK". Implementation decision: `FsSnapshotStore::read_page` resolves via `dirty_page_index` HashMap with newest-first insertion per phase_02.md Task 1. Demand-paging via UFFD serves the correctly-resolved page. |
| **Command** | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-incremental-chain` |

### userfaultd.AC5.5 -- Test error handling: store fails `read_page` for specific address, VM gets clean `VmExit::Error`

| Field | Value |
|-------|-------|
| **Test type** | Integration (e2e) |
| **Test file** | `tests/test_cases/src/test_uffd_error.rs` (new) |
| **Phase** | 6, Task 6 |
| **Criterion** | `ErrorStore` returns `Err(io::Error)` from `read_page`. `restore_and_run_with_store` returns `Ok(VmExit::Error { message })` containing the error message. No panic, no hang. |
| **Approach** | Host: build VM, take snapshot. Build new Context with `ErrorStoreFactory` whose `read_page` always returns `Err(io::Error::new(ErrorKind::Other, "simulated read_page failure"))`. Cold restore via `restore_and_run_with_store`. Verify return is `Ok(VmExit::Error { message })` with message containing "read_page". Host-side `start_vm()` performs the assertion and prints "OK". Guest side not used (VM never executes guest code). Implementation decision: UFFD handler's `signal_error()` stores `VmExit::Error` in `SharedVmExit` and drops the Uffd fd per phase_03.md Task 3. |
| **Command** | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-error-handling` |

### userfaultd.AC5.6 -- Parallel fault test: store with artificial 50ms delay per `read_page`, multiple vCPUs, verify concurrent resolution

| Field | Value |
|-------|-------|
| **Test type** | Integration (e2e) |
| **Test file** | `tests/test_cases/src/test_uffd_parallel.rs` (new) |
| **Phase** | 6, Task 7 |
| **Criterion** | `DelayStore` adds 50ms sleep per `read_page`. VM has 2+ vCPUs. Wall-clock restore time is significantly less than `num_faults * 50ms`, confirming parallel fault resolution. |
| **Approach** | Host: build VM with 2 vCPUs, 256 MiB RAM. Guest initializes memory, signals. Take snapshot. Build new Context with `DelayStoreFactory` (empty preload, 50ms delay per `read_page`). Implementation uses a `PartialDelayStore` that preloads everything except ~100 specific pages, returns those with 50ms `tokio::time::sleep` delay. Record wall-clock time around `restore_and_run_with_store`. Guest accesses the delayed pages from multiple vCPUs, verifies state, prints "OK". Assert `wall_time < 3s` (sequential would be ~5s for 100 faults at 50ms). Implementation decision: `tokio::spawn` per fault in the fault loop enables parallel I/O per phase_03.md Task 3. The `DelayStore` is defined in `mock_snapshot_store.rs` per phase_06.md Task 1. |
| **Command** | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-parallel-faults` |

---

## Supporting Infrastructure (No Direct AC Mapping)

These components support AC verification but have no direct acceptance criterion. They have their own unit tests.

### PageTracker bitmap operations

| Field | Value |
|-------|-------|
| **Test type** | Unit |
| **Test file** | `src/vmm/src/uffd.rs` (`#[cfg(test)] mod tests`) |
| **Phase** | 5, Task 1 |
| **Criterion** | Bitmap set/get operations are correct. Counters track preload vs fault sources. Duplicate marks do not double-count. Concurrent marking is safe. |
| **Approach** | Tests: (1) mark page loaded, verify `is_loaded()` returns true; (2) mark same page twice, verify counter increments only once; (3) mark pages from both `LoadSource::Preload` and `LoadSource::Fault`, verify `stats()` reflects correct counts; (4) verify `progress_pct` calculation; (5) concurrent marking from multiple threads via `std::thread::spawn`, verify no corruption or panic. |
| **Command** | `cargo test -p vmm --features uffd` |

### Mock SnapshotStore infrastructure

| Field | Value |
|-------|-------|
| **Test type** | Unit (compile check) |
| **Test file** | `tests/test_cases/src/mock_snapshot_store.rs` (new) |
| **Phase** | 6, Task 1 |
| **Criterion** | `EmptyPreloadStore`, `PartialPreloadStore`, `ErrorStore`, and `DelayStore` all implement `SnapshotStore` and compile as `Box<dyn SnapshotStore>`. Factories implement `SnapshotStoreFactory`. |
| **Approach** | Verified by compilation of the test workspace: `cd tests && cargo check -p test_cases --features host`. |
| **Command** | `cd tests && cargo check -p test_cases --features host` |

---

## Summary Matrix

| AC | Automated? | Test Type | Test File | Phase |
|----|-----------|-----------|-----------|-------|
| userfaultd.AC1.1 | Yes | Unit | `src/vmm/src/snapshot_store.rs` | 1 |
| userfaultd.AC1.2 | Yes | Unit | `src/vmm/src/snapshot_store.rs` | 1 |
| userfaultd.AC1.3 | Yes | Unit | `src/vmm/src/snapshot_store.rs` | 1 |
| userfaultd.AC1.4 | Yes | Unit | `src/vmm/src/snapshot_store.rs` | 1 |
| userfaultd.AC1.5 | No | Human | Code review of trait definition | 1 |
| userfaultd.AC2.1 | Yes | Unit | `src/vmm/src/snapshot_store.rs` | 1 |
| userfaultd.AC2.2 | Yes | Unit | `src/vmm/src/snapshot_store.rs` | 1 |
| userfaultd.AC2.3 | Yes | Unit | `src/vmm/src/snapshot_store.rs` | 2 |
| userfaultd.AC2.4 | Yes | Unit | `src/vmm/src/snapshot_store.rs` | 2 |
| userfaultd.AC2.5 | Yes | Integration | `tests/test_cases/src/test_snapshot_restore.rs` (existing) | 2 |
| userfaultd.AC3.1 | Yes | Unit | `src/vmm/src/uffd.rs` | 3 |
| userfaultd.AC3.2 | Yes | Unit | `src/vmm/src/uffd.rs` | 3 |
| userfaultd.AC3.3 | Yes | Unit | `src/vmm/src/uffd.rs` | 3 |
| userfaultd.AC3.4 | Yes | Unit | `src/vmm/src/uffd.rs` | 4 |
| userfaultd.AC3.5 | Yes | Unit | `src/vmm/src/uffd.rs` | 4 |
| userfaultd.AC3.6 | Yes | Unit | `src/vmm/src/uffd.rs` | 3+4 |
| userfaultd.AC4.1 | Yes | Integration | `tests/test_cases/src/test_snapshot_restore.rs` (existing) | 2 |
| userfaultd.AC4.2 | Yes | Integration | `tests/test_cases/src/test_snapshot_restore.rs` (existing) | 1 |
| userfaultd.AC4.3 | Yes | Integration | All existing `test_snapshot_*.rs` files | 2 |
| userfaultd.AC4.4 | Yes | Unit | `src/vmm/src/snapshot_store.rs` | 1 |
| userfaultd.AC5.1 | Yes | Integration | `tests/test_cases/src/test_uffd_demand_page.rs` (new) | 6 |
| userfaultd.AC5.2 | Yes | Integration | `tests/test_cases/src/test_uffd_preload.rs` (new) | 6 |
| userfaultd.AC5.3 | Yes | Integration | `tests/test_cases/src/test_uffd_preload.rs` (new) | 6 |
| userfaultd.AC5.4 | Yes | Integration | `tests/test_cases/src/test_uffd_incremental.rs` (new) | 6 |
| userfaultd.AC5.5 | Yes | Integration | `tests/test_cases/src/test_uffd_error.rs` (new) | 6 |
| userfaultd.AC5.6 | Yes | Integration | `tests/test_cases/src/test_uffd_parallel.rs` (new) | 6 |

**Totals:** 26 acceptance criteria. 25 automated tests. 1 human verification (userfaultd.AC1.5 -- negative requirement about API absence).

---

## Test Execution Order

Tests are gated by feature flags and build in dependency order matching the implementation phases:

1. **Phase 1-2 unit tests:** `cargo test -p vmm --features snapshot`
   - AC1.1-AC1.4, AC2.1-AC2.4, AC4.4
2. **Phase 2 regression tests:** `make test FEATURE_FLAGS="--features embedded_init"`
   - AC2.5, AC4.1, AC4.2, AC4.3
3. **Phase 3-5 unit tests:** `cargo test -p vmm --features uffd`
   - AC3.1-AC3.6, PageTracker
4. **Phase 6 integration tests:** `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-*`
   - AC5.1-AC5.6
5. **Human verification:** Code review during Phase 1 PR
   - AC1.5

## Platform Constraints

- All UFFD tests (AC3.x unit tests and AC5.x integration tests) require **Linux** with `userfaultfd` kernel support. They are gated with `#[cfg(all(target_os = "linux", feature = "uffd"))]`.
- AC1.x and AC2.x unit tests run on all platforms (no UFFD dependency).
- AC4.x backward compatibility tests run on all platforms.
- Integration tests require `libkrunfw` available at runtime (handled by test infrastructure).
