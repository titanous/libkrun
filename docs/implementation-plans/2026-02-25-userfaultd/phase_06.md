# Userfaultd Implementation Plan — Phase 6: Integration Tests

**Goal:** Write integration tests that verify demand-paging works end-to-end with real VMs, covering empty preload, full preload, partial preload, incremental chains, error handling, and parallel faults.

**Architecture:** Tests use the existing host/guest proc macro pattern (`#[host]`/`#[guest]`). Each test creates a VM, takes a snapshot, then cold-restores via `Context::restore_and_run_with_store()` with a custom `SnapshotStoreFactory`. Mock stores wrap `FsSnapshotStore` to control preload behavior. The guest verifies memory state survived the demand-paged restore.

**Tech Stack:** Rust, libkrun test framework (host/guest macros), mock SnapshotStore/Factory implementations

**Scope:** 6 phases from original design (phase 6 of 6)

**Codebase verified:** 2026-02-25

---

## Acceptance Criteria Coverage

This phase implements and tests:

### userfaultd.AC5: Integration tests with demand-paging
- **userfaultd.AC5.1 Success:** Test with empty-preload store: all pages loaded via UFFD faults, guest runs correctly
- **userfaultd.AC5.2 Success:** Test with FsSnapshotStore: preload loads everything, near-zero faults
- **userfaultd.AC5.3 Success:** Test with partial-preload store: both preload and fault paths exercise
- **userfaultd.AC5.4 Success:** Test incremental chain: base + 2 incrementals via demand-paging, latest dirty pages win
- **userfaultd.AC5.5 Success:** Test error handling: store fails `read_page` for specific address, VM gets clean `VmExit::Error`
- **userfaultd.AC5.6 Success:** Parallel fault test: store with artificial 50ms delay per `read_page`, multiple vCPUs. Verify total restore time is significantly less than sequential (num_faults * 50ms), confirming faults are resolved concurrently

---

## Reference Files

- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/tests/CLAUDE.md` — Test workspace contracts
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/tests/test_cases/src/lib.rs` — Test case registration (add new tests to `test_cases()` vec)
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/tests/test_cases/src/test_snapshot_restore.rs` — Existing snapshot test pattern (host: setup VM, snapshot, restore, verify; guest: set state, signal, verify after restore)
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/tests/test_cases/src/mem_block_backend.rs` — Mock backend factory pattern (MemBlockBackend/MemBlockBackendFactory)
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/tests/test_cases/src/krun_rust.rs` — `setup_fs_builder()` helper, TestSetup struct
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/tests/test_cases/Cargo.toml` — Test workspace dependencies (add `uffd` feature to libkrun dep)

---

<!-- START_TASK_1 -->
### Task 1: Add uffd feature to test workspace and create mock SnapshotStore infrastructure

**Verifies:** None (infrastructure)

**Files:**
- Modify: `tests/test_cases/Cargo.toml` (add `uffd` feature to libkrun dependency)
- Create: `tests/test_cases/src/mock_snapshot_store.rs` (mock SnapshotStore implementations)
- Modify: `tests/test_cases/src/lib.rs` (add `mod mock_snapshot_store;` and register new test cases)

**Implementation:**

Add `uffd` to the libkrun features in `tests/test_cases/Cargo.toml`:
```toml
libkrun = { path = "../../src/libkrun", optional = true, features = ["embedded_init", "net", "blk", "snapshot", "vhost-user", "uffd"] }
```

Also add `tokio` with sleep feature for the delay store:
```toml
tokio = { version = "1", features = ["sync", "time"] }
```

Create `tests/test_cases/src/mock_snapshot_store.rs` with several mock store implementations that wrap `FsSnapshotStore`:

1. **EmptyPreloadStore** — `read_vmstate` and `read_page` delegate to FsSnapshotStore, `preload` returns an empty stream. Forces all pages through fault handler.

2. **PartialPreloadStore** — `preload` yields only the first N% of memory, rest served by faults.

3. **ErrorStore** — `read_page` returns `Err` for a specific guest address, delegates everything else to FsSnapshotStore.

4. **DelayStore** — `read_page` adds artificial 50ms sleep before delegating to FsSnapshotStore. Used for parallel fault testing.

All mock stores wrap an inner `FsSnapshotStore` (created from the same snapshot files). The factory creates the inner store and wraps it.

Register all new test cases in `test_cases()`:
```rust
TestCase::new("uffd-demand-page-only", Box::new(TestUffdDemandPageOnly)),
TestCase::new("uffd-preload-full", Box::new(TestUffdPreloadFull)),
TestCase::new("uffd-preload-partial", Box::new(TestUffdPreloadPartial)),
TestCase::new("uffd-incremental-chain", Box::new(TestUffdIncrementalChain)),
TestCase::new("uffd-error-handling", Box::new(TestUffdErrorHandling)),
TestCase::new("uffd-parallel-faults", Box::new(TestUffdParallelFaults)),
```

**Verification:**
Run: `cd tests && cargo check -p test_cases --features host`
Expected: Compiles without errors

**Commit:** `feat(tests): add mock SnapshotStore infrastructure for UFFD integration tests`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Test demand-page-only (empty preload)

**Verifies:** userfaultd.AC5.1

**Files:**
- Create: `tests/test_cases/src/test_uffd_demand_page.rs`
- Modify: `tests/test_cases/src/lib.rs` (add module declaration)

**Implementation:**

Test pattern:
1. **Host side:**
   - Build VM with 1 vCPU, 256 MiB RAM
   - Run VM, wait for guest READY signal
   - Take full snapshot to temp dir
   - Build a NEW Context (fresh VM, same config)
   - Create `EmptyPreloadStoreFactory` wrapping the snapshot path
   - Cold restore via `context.restore_and_run_with_store(Box::new(factory))`
   - Guest continues from snapshot state, verifies memory, prints "OK"

2. **Guest side:**
   - Set a static counter to a known value (e.g., 42)
   - Signal READY via vsock
   - After cold restore resumes execution, verify counter is still 42
   - Write a known pattern to a heap allocation to ensure demand-paged memory works
   - Print "OK"

The EmptyPreloadStore's `preload` returns `futures::stream::empty()`. Every page the guest accesses triggers a UFFD fault, which calls `store.read_page()` → FsSnapshotStore reads from the memory file.

**Key detail:** Cold restore with `restore_and_run_with_store` creates a fresh VM and restores into it. The guest code path resumes from where the snapshot was taken (the vCPU state is restored). The guest must verify its state survived.

**Testing:**

The test itself IS the AC verification:
- userfaultd.AC5.1: All pages loaded via faults (empty preload), guest runs correctly and prints "OK"

**Verification:**
Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-demand-page-only`
Expected: Test passes

**Commit:** `feat(tests): add UFFD demand-page-only integration test`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Test FsSnapshotStore full preload

**Verifies:** userfaultd.AC5.2

**Files:**
- Create: `tests/test_cases/src/test_uffd_preload.rs`
- Modify: `tests/test_cases/src/lib.rs` (add module declaration)

**Implementation:**

Test pattern:
1. **Host side:**
   - Same as Task 2 but use `FsSnapshotStoreFactory` (no wrapper)
   - Cold restore via `context.restore_and_run_with_store(Box::new(FsSnapshotStoreFactory::new(snap_dir, &[])))`
   - FsSnapshotStore's `preload` yields all memory in 4MB chunks
   - Guest runs from restored state

2. **Guest side:** Same verification as Task 2 — verify counter, print "OK"

With FsSnapshotStore, the preload stream loads all memory before any faults occur. The preload task races ahead of vCPU execution. Near-zero faults expected.

**Testing:**

- userfaultd.AC5.2: Preload loads everything, guest runs correctly. Near-zero faults (can verify via PageTracker stats if accessible, or just verify guest prints "OK").

**Verification:**
Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-preload-full`
Expected: Test passes

**Commit:** `feat(tests): add UFFD full preload integration test`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Test partial preload (mixed path)

**Verifies:** userfaultd.AC5.3

**Files:**
- Modify: `tests/test_cases/src/test_uffd_preload.rs` (add partial preload test case)

**Implementation:**

Test pattern:
1. **Host side:**
   - Same VM setup and snapshot
   - Use `PartialPreloadStoreFactory` that yields only the first 50% of memory via preload
   - Cold restore via `context.restore_and_run_with_store()`
   - The first 50% of pages are preloaded, the rest are demand-paged on fault
   - Both preload and fault paths exercise

2. **Guest side:** Same verification — counter + "OK"

The PartialPreloadStore wraps FsSnapshotStore and truncates the preload stream after yielding half the memory.

**Testing:**

- userfaultd.AC5.3: Both preload and fault paths exercise. Guest runs correctly with mixed loading.

**Verification:**
Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-preload-partial`
Expected: Test passes

**Commit:** `feat(tests): add UFFD partial preload integration test`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Test incremental chain via demand-paging

**Verifies:** userfaultd.AC5.4

**Files:**
- Create: `tests/test_cases/src/test_uffd_incremental.rs`
- Modify: `tests/test_cases/src/lib.rs` (add module declaration)

**Implementation:**

Test pattern:
1. **Host side:**
   - Build VM with 1 vCPU, 256 MiB
   - Run VM, guest initializes counter to value A, signals READY
   - Take full snapshot (snap_base)
   - Enable dirty tracking
   - Signal guest to mutate: guest sets counter to value B, signals WRITTEN
   - Take incremental snapshot 1 (snap_inc1)
   - Signal guest to mutate again: guest sets counter to value C, signals WRITTEN
   - Take incremental snapshot 2 (snap_inc2)
   - Build NEW Context, create `FsSnapshotStoreFactory::new(snap_base, &[snap_inc1, snap_inc2])`
   - Cold restore via `context.restore_and_run_with_store(factory)`
   - Guest resumes from snap_inc2 state, verifies counter == C (latest dirty page wins)

2. **Guest side:**
   - Static counter
   - Phase 1: Set counter = 100, signal READY
   - Phase 2: Wait for MUTATE1, set counter = 200, signal WRITTEN
   - Phase 3: Wait for MUTATE2, set counter = 300, signal WRITTEN
   - After cold restore: verify counter == 300, print "OK"

This verifies that demand-paging correctly resolves incremental overlays — the latest dirty page (from snap_inc2) wins over snap_inc1 and snap_base.

**Testing:**

- userfaultd.AC5.4: Base + 2 incrementals via demand-paging. Counter == 300 confirms latest dirty pages win.

**Verification:**
Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-incremental-chain`
Expected: Test passes

**Commit:** `feat(tests): add UFFD incremental chain integration test`
<!-- END_TASK_5 -->

<!-- START_TASK_6 -->
### Task 6: Test error handling (read_page failure)

**Verifies:** userfaultd.AC5.5

**Files:**
- Create: `tests/test_cases/src/test_uffd_error.rs`
- Modify: `tests/test_cases/src/lib.rs` (add module declaration)

**Implementation:**

Test pattern:
1. **Host side:**
   - Build VM, take snapshot
   - Build NEW Context with `ErrorStoreFactory` — this store returns `Err(io::Error)` when `read_page` is called for any address (or for a specific address known to be accessed early in boot)
   - Cold restore via `context.restore_and_run_with_store(factory)`
   - Expected: `restore_and_run_with_store` returns `Ok(VmExit::Error { message })` containing the error message from the failing `read_page`
   - Verify the VmExit is Error variant with a meaningful message

2. **Guest side:** Not used — the VM never gets to execute guest code because the first page fault fails

The ErrorStore's `read_page` returns `Err(io::Error::new(ErrorKind::Other, "simulated read_page failure"))`. The UFFD handler signals `VmExit::Error`, the main event loop returns it.

**Custom check:** Override the default `check()` method on the Test trait to NOT expect "OK\n" output (since the guest never runs). Instead, check the `VmExit` return value.

Actually, looking at the test framework more carefully: `start_vm()` runs in the host process and returns `anyhow::Result<()>`. The test framework calls `check()` on the child process. For this test, we need `start_vm` to verify the VmExit itself:

```rust
fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
    // ... setup, snapshot ...
    let result = context.restore_and_run_with_store(Box::new(error_factory));
    match result {
        Ok(VmExit::Error { message }) => {
            assert!(message.contains("read_page"), "Expected read_page error, got: {message}");
            println!("OK");
            Ok(())
        }
        other => panic!("Expected VmExit::Error, got: {other:?}"),
    }
}
```

**Testing:**

- userfaultd.AC5.5: Store fails `read_page`, VM gets clean `VmExit::Error`. Test verifies the error variant and message.

**Verification:**
Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-error-handling`
Expected: Test passes

**Commit:** `feat(tests): add UFFD error handling integration test`
<!-- END_TASK_6 -->

<!-- START_TASK_7 -->
### Task 7: Test parallel fault resolution

**Verifies:** userfaultd.AC5.6

**Files:**
- Create: `tests/test_cases/src/test_uffd_parallel.rs`
- Modify: `tests/test_cases/src/lib.rs` (add module declaration)

**Implementation:**

Test pattern:
1. **Host side:**
   - Build VM with **2+ vCPUs** (e.g., 2 vCPUs, 256 MiB RAM)
   - Run VM, guest initializes memory, signals READY
   - Take snapshot
   - Build NEW Context with `DelayStoreFactory` — this store adds 50ms sleep before each `read_page` response
   - Record wall-clock time before cold restore
   - Cold restore via `context.restore_and_run_with_store(factory)`
   - Record wall-clock time after VM exits
   - Guest verifies state, prints "OK"
   - Calculate: if faults were sequential, total time ≈ num_faults × 50ms. With parallel resolution, total time should be significantly less.
   - Assert: wall_time < (estimated_sequential_time / 2) — confirming concurrent fault resolution

2. **Guest side:**
   - Access memory from multiple addresses spread across the address space to trigger faults on different pages
   - Use 2+ vCPUs to fault concurrently
   - Verify memory state, print "OK"

The DelayStore wraps FsSnapshotStore with empty preload (forces all faults through `read_page`). Each `read_page` calls `tokio::time::sleep(Duration::from_millis(50))` before returning.

**Approach:** Use a `PartialDelayStore` that preloads everything except ~100 specific pages, and returns those pages with a 50ms `tokio::time::sleep` delay on `read_page`. The guest accesses those specific pages from multiple vCPUs, triggering ~100 faults. Sequential time would be 100 × 50ms = 5s. With parallel resolution via `tokio::spawn`, total time should be well under 3s. Assert: `wall_time < 3s`.

**Testing:**

- userfaultd.AC5.6: With 50ms delay per `read_page` and 2 vCPUs, verify wall-clock time confirms concurrent resolution. Total time significantly less than num_faults × 50ms.

**Verification:**
Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-parallel-faults`
Expected: Test passes, wall-clock time confirms parallel execution

**Commit:** `feat(tests): add UFFD parallel fault resolution integration test`
<!-- END_TASK_7 -->
