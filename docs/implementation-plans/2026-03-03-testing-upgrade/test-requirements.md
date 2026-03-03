# Test Requirements: testing-upgrade

Generated: 2026-03-03
Design: docs/design-plans/2026-03-03-testing-upgrade.md
Implementation: docs/implementation-plans/2026-03-03-testing-upgrade/

## Overview

The design defines **5 AC groups** with **30 individual acceptance criteria**. Of these:
- **17 automated** via test code (unit, proptest, loom, shuttle, fuzz harness, kani proof, integration)
- **13 human-verified** (build/infrastructure checks, tool invocation, documentation)

The automated tests break down as:
- 5 proptest suites (bitmap invariants, GDT, address translation, snapshot round-trips, Builder validation)
- 3 loom suites (DirtyBitmap, ReclaimedBitmap, PageTracker)
- 3 shuttle suites (block quiesce, balloon condvar, DeviceState)
- 6 kani proofs (PageTracker dedup, DirtyBitmap bounds, ReclaimedBitmap consistency, snapshot header, GDT round-trip, address translation)
- 5 fuzz harnesses (FUSE, descriptor chain, snapshot deser, vhost-user msg, block request)
- 9 integration tests (balloon+snapshot+UFFD, block+snapshot+UFFD, virtiofs+DAX+snapshot, full stack, failing backend, slow backend, minimal FS, balloon+snapshot race, UFFD+balloon parallel)

---

## AC1: C API Removed and Rust API Stabilized

### testing-upgrade.AC1.1
- **Criterion:** No `pub extern "C"` functions exist in `src/libkrun/src/lib.rs`
- **Test type:** build
- **Test file:** N/A (verified by build + grep)
- **Test description:** `cargo check -p krun --features embedded_init,snapshot,uffd,blk,vhost-user` succeeds and `grep -r 'pub extern "C"' src/libkrun/src/lib.rs` returns no matches. This is a structural assertion verified at build time, not a runtime test.
- **Verification:** human (Phase 1, Task 1 verification step)

### testing-upgrade.AC1.2
- **Criterion:** `Builder`, `Context`, `VmHandle`, and all trait re-exports compile and are publicly accessible from Rust
- **Test type:** build
- **Test file:** N/A (verified by `cargo check`)
- **Test description:** The main workspace compiles with `cargo check --features embedded_init,snapshot,uffd,blk,vhost-user`. The types are used by existing integration tests (e.g., `test_rust_api.rs`, `test_vm_exit.rs`) which import `krun::Builder`. Compilation success proves public accessibility.
- **Verification:** human (Phase 1, Task 7 verification step: `cargo check` + unit test pass)

### testing-upgrade.AC1.3
- **Criterion:** `just check` passes with no C API symbols in the compiled library
- **Test type:** human
- **Test file:** N/A
- **Test description:** Run `just check` (which runs `cargo fmt --check` + `cargo clippy`) and verify clean exit. The absence of C API symbols is confirmed by AC1.1 (no `pub extern "C"` source) and by the crate-type change from `cdylib` to `lib` (Phase 1, Task 1, Step 3), which stops producing a C shared library entirely.
- **Verification:** human (run `just check`, observe exit code 0)

### testing-upgrade.AC1.4
- **Criterion:** Removing C API does not break integration tests (they use Rust API)
- **Test type:** integration
- **Test file:** `tests/test_cases/src/test_vm_config.rs`, `tests/test_cases/src/test_vsock_guest_connect.rs`, `tests/test_cases/src/test_tsi_tcp_guest_connect.rs`, `tests/test_cases/src/test_tsi_tcp_guest_listen.rs`, `tests/test_cases/src/test_multiport_console.rs` (migrated files) + all existing tests
- **Test description:** All 5 C API tests are migrated to use Rust `Builder` API in Phase 1, Tasks 3-5. Running `just integration` after migration confirms no breakage. The test suite has inherent flakiness (5-6 of 6 tests passing is normal).
- **Verification:** human (run `just integration`, observe pass rate >= 5/6)

---

## AC2: Testing Infrastructure Established

### testing-upgrade.AC2.1
- **Criterion:** `just miri` runs Miri on all pure-logic modules and passes
- **Test type:** human (infrastructure invocation)
- **Test file:** Existing unit tests in: `src/arch/src/x86_64/gdt.rs`, `src/vmm/src/dirty_bitmap.rs`, `src/vmm/src/snapshot.rs`, `src/vmm/src/uffd/page_tracker.rs`, `src/devices/src/virtio/balloon/reclaimed_bitmap.rs`, `src/devices/src/virtio/block/request.rs`
- **Test description:** `just miri` invokes `cargo +nightly miri test` on each pure-logic module. The tests themselves are the existing unit tests plus the proptest and loom tests (Miri-incompatible ones are skipped via `#[cfg_attr(miri, ignore)]`). Miri validates no undefined behavior.
- **Verification:** human (run `just miri`, observe all test suites pass)

### testing-upgrade.AC2.2
- **Criterion:** `just fuzz-list` shows 5 fuzz targets; `just fuzz <target>` builds and runs each
- **Test type:** human (infrastructure invocation)
- **Test file:** `fuzz/fuzz_targets/fuzz_fuse_parsing.rs`, `fuzz/fuzz_targets/fuzz_descriptor_chain.rs`, `fuzz/fuzz_targets/fuzz_snapshot_deser.rs`, `fuzz/fuzz_targets/fuzz_vhost_user_msg.rs`, `fuzz/fuzz_targets/fuzz_block_request.rs`
- **Test description:** `just fuzz-list` lists the 5 targets. Each target must build via `cargo +nightly fuzz build` and run without crashing for a short duration. The fuzz harnesses themselves are the automated component (coverage-guided mutation of inputs), but verifying that they "build and run" requires invoking the tool.
- **Verification:** human (run `just fuzz-list` and check 5 targets listed; run `just fuzz <target>` for each)

### testing-upgrade.AC2.3
- **Criterion:** `just loom` runs exhaustive concurrency tests on DirtyBitmap, ReclaimedBitmap, PageTracker and passes
- **Test type:** loom
- **Test file:** `src/vmm/src/dirty_bitmap.rs` (loom_tests module), `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` (loom_tests module), `src/vmm/src/uffd/page_tracker.rs` (loom_tests module)
- **Test description:** Loom exhaustively explores all thread interleavings for small concurrent scenarios. Tests verify: (1) DirtyBitmap: concurrent mark_dirty + drain_dirty_pages loses no pages; (2) ReclaimedBitmap: concurrent mark + clear keeps count consistent; (3) PageTracker: concurrent mark_loaded from Preload + Fault on same page increments counter exactly once.

### testing-upgrade.AC2.4
- **Criterion:** `just shuttle` runs randomized concurrency tests on block quiesce, balloon resize, device activation and passes
- **Test type:** shuttle
- **Test file:** `src/devices/src/virtio/block/async_worker.rs` (shuttle_tests module), `src/devices/src/virtio/balloon/device.rs` (shuttle_tests module), `src/devices/src/virtio/device.rs` (shuttle_tests module)
- **Test description:** Shuttle samples 1000 random thread interleavings per test. Tests verify: (1) Block quiesce: Mutex<bool>+Condvar handshake has no deadlock or missed wakeup; (2) Balloon: await_target condvar loop terminates when guest signals; (3) DeviceState: Inactive->Activated transition observed atomically by concurrent readers.

### testing-upgrade.AC2.5
- **Criterion:** `just proptest` runs property tests for snapshot round-trips, bitmap invariants, GDT, address translation, builder validation and passes
- **Test type:** proptest
- **Test file:** `src/vmm/src/dirty_bitmap.rs` (proptest_tests module), `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` (proptest_tests module), `src/arch/src/x86_64/gdt.rs` (proptest_tests module), `src/vmm/src/snapshot.rs` (proptest_tests module), `src/vmm/src/uffd/page_tracker.rs` (proptest_tests module), `src/libkrun/src/lib.rs` (proptest_tests module)
- **Test description:** Property-based tests using proptest strategies to generate arbitrary inputs. Verifies: (1) DirtyBitmap: mark N pages then drain returns exactly N; drain empties; mark idempotent; (2) ReclaimedBitmap: mark_range count matches; iter_set_pages correct; mark/clear round-trip; (3) GDT: get_base(gdt_entry(...)) == base; kvm_segment base preserved; (4) Snapshot: VmSnapshot and SnapshotHeader bincode round-trip; invalid magic always fails validation; (5) PageTracker: mark_loaded deduplication; guest_to_host in-range/out-of-range; (6) Builder: zero vCPUs always fails; nonzero vCPUs succeeds.

### testing-upgrade.AC2.6
- **Criterion:** `just kani` runs all bounded proofs and they verify
- **Test type:** kani
- **Test file:** `kani-proofs/src/lib.rs` (or per-proof files in `kani-proofs/src/`)
- **Test description:** Kani bounded model checker proves correctness for all inputs up to specified bounds. 6 proofs: (1) PageTracker mark_loaded deduplication (bound: 128 pages); (2) DirtyBitmap mark_dirty bounds safety (bound: 256 pages); (3) ReclaimedBitmap mark/is_set/clear/count consistency (bound: 256 pages); (4) Snapshot header validation rejects invalid magic/version; (5) GDT get_base/get_limit round-trip for all inputs; (6) Address translation correctness (bound: 4 regions).

### testing-upgrade.AC2.7
- **Criterion:** `just asan` runs unit tests with AddressSanitizer and passes
- **Test type:** human (infrastructure invocation)
- **Test file:** N/A (runs existing unit tests under ASan instrumentation)
- **Test description:** `just asan` runs `RUSTFLAGS="-Zsanitizer=address" cargo +nightly test --target x86_64-unknown-linux-gnu` on `devices` and `vmm` crates. ASan detects buffer overflows, use-after-free, and heap corruption at runtime. No new test code is written; ASan instruments existing tests.
- **Verification:** human (run `just asan`, observe no `ERROR: AddressSanitizer` in output)

### testing-upgrade.AC2.8
- **Criterion:** `just integration-asan` runs integration tests with AddressSanitizer and passes
- **Test type:** human (infrastructure invocation)
- **Test file:** N/A (runs existing integration tests under ASan instrumentation)
- **Test description:** `just integration-asan` builds the runner, test-daemon, and test-vsock-proxy with ASan, pre-builds guest-agent without ASan (musl incompatible), then runs `tests/run.sh`. ASan instruments the host-side VM management code.
- **Verification:** human (run `just integration-asan`, observe pass rate consistent with `just integration`)

### testing-upgrade.AC2.9
- **Criterion:** `just mutants` produces a mutation testing report with scored results
- **Test type:** human (infrastructure invocation + documentation)
- **Test file:** N/A (cargo-mutants mutates existing source and runs existing tests)
- **Test description:** `just mutants` runs cargo-mutants with the full feature set and exclusions, produces `mutants.out/outcomes.json`. Baseline mutation score is documented in `docs/mutation-baseline.md`. Surviving mutants are triaged into accepted gaps (annotated with `#[mutants::skip]`), low-priority gaps, and gaps to address.
- **Verification:** human (run `just mutants`, verify `mutants.out/outcomes.json` exists, verify `docs/mutation-baseline.md` is populated)

### testing-upgrade.AC2.10
- **Criterion:** `just fuzz-all` with a 60-second duration produces no crashes on any target
- **Test type:** human (time-gated infrastructure invocation)
- **Test file:** `fuzz/fuzz_targets/` (all 5 targets)
- **Test description:** `just fuzz-all 60` runs each of the 5 fuzz targets for 60 seconds. Crashes are reported by libFuzzer as non-zero exit codes with a crash artifact saved. No crashes means all 5 targets exit cleanly.
- **Verification:** human (run `just fuzz-all 60`, observe all 5 targets complete with exit code 0)

### testing-upgrade.AC2.11
- **Criterion:** All justfile targets use the same feature set variable (`embedded_init,snapshot,uffd,blk,vhost-user`)
- **Test type:** human (code review)
- **Test file:** `justfile`
- **Test description:** Inspect the justfile to confirm a single `features` variable is defined at the top and referenced by all targets that need feature flags. No hardcoded feature strings should appear in individual targets.
- **Verification:** human (read justfile, verify `features` variable usage)

---

## AC3: Coverage Gaps Filled

### testing-upgrade.AC3.1
- **Criterion:** Combined balloon+snapshot+UFFD test passes: inflate -> snapshot -> restore with UFFD -> reclaimed pages zero-filled -> deflate -> guest reuses memory
- **Test type:** integration
- **Test file:** `tests/test_cases/src/test_balloon_snapshot_uffd.rs`
- **Test description:** Host: boot VM with balloon enabled, inflate balloon, take full snapshot, exit VM, restore with UFFD demand-paging (EmptyPreloadStoreFactory), verify guest reconnects over vsock. Guest: after restore, verify reclaimed pages are zero-filled (read memory that was in the balloon), deflate balloon, write to reclaimed memory to prove reuse.

### testing-upgrade.AC3.2
- **Criterion:** Combined block+snapshot+UFFD test passes: write via custom backend -> snapshot -> restore with UFFD -> read back matches
- **Test type:** integration
- **Test file:** `tests/test_cases/src/test_block_snapshot_uffd.rs`
- **Test description:** Host: boot VM with custom AsyncBlockBackend (MemBlockBackend or similar), write pattern to block device, take snapshot, restore with UFFD. Guest: after restore, read block device and verify data pattern matches pre-snapshot write.

### testing-upgrade.AC3.3
- **Criterion:** Combined virtiofs(DAX)+snapshot test passes: mount custom FileSystem -> write -> snapshot -> restore -> read back matches
- **Test type:** integration
- **Test file:** `tests/test_cases/src/test_virtiofs_dax_snapshot.rs`
- **Test description:** Host: boot VM with custom FileSystem impl and DAX enabled (shm_size=Some(1<<29)), write a file via the FS. Guest: mount virtiofs, read file, signal ready. Host: take snapshot, hot-restore. Guest: after restore, read file again and verify contents match.

### testing-upgrade.AC3.4
- **Criterion:** Full stack test passes: TSI connection + balloon + snapshot -> restore -> TSI resumes, balloon preserved
- **Test type:** integration
- **Test file:** `tests/test_cases/src/test_full_stack.rs`
- **Test description:** Host: boot VM with vhost-user vsock (TSI), balloon, establish TCP connection via TSI, inflate balloon, snapshot, restore. Guest: verify TSI connection resumes after restore and balloon state is preserved. **Note:** Phase 7 implementation plan defers this test as a stretch goal due to complexity of cross-feature vhost-user vsock + balloon + snapshot coordination.
- **Verification:** This may be human-verified as a stretch goal if deferred.

### testing-upgrade.AC3.5
- **Criterion:** Failing block backend test: guest receives I/O errors, device doesn't crash, other requests unaffected
- **Test type:** integration
- **Test file:** `tests/test_cases/src/test_block_backend_errors.rs`
- **Test description:** Host: boot VM with FailingBlockBackend configured to error on specific sectors. Guest: attempt reads/writes on error sectors (expect I/O errors), then read/write non-error sectors (expect success). Verify the block device remains functional and does not crash.

### testing-upgrade.AC3.6
- **Criterion:** Slow block backend test: queue doesn't stall, metrics track correctly
- **Test type:** integration
- **Test file:** `tests/test_cases/src/test_block_backend_slow.rs`
- **Test description:** Host: boot VM with SlowBlockBackend configured with artificial delays. Guest: issue multiple I/O requests concurrently, verify all complete (queue doesn't stall), check that response times are within expected bounds (delays are observable). Host: verify AsyncWorkerMetrics counters are consistent.

### testing-upgrade.AC3.7
- **Criterion:** Minimal FileSystem test: guest gets ENOSYS for unsupported ops, device doesn't crash, DAX enabled
- **Test type:** integration
- **Test file:** `tests/test_cases/src/test_virtiofs_minimal.rs`
- **Test description:** Host: boot VM with MinimalFileSystem (only lookup+read implemented, all other ops return ENOSYS) and DAX enabled. Guest: mount virtiofs, attempt unsupported operations (mkdir, symlink, etc.), verify ENOSYS errors returned, verify device still functional for supported ops (read).

### testing-upgrade.AC3.8
- **Criterion:** Concurrent balloon resize + snapshot stress test: no race in excluded pages collection
- **Test type:** integration
- **Test file:** `tests/test_cases/src/test_balloon_snapshot_race.rs`
- **Test description:** Host: boot VM with balloon enabled, rapidly alternate inflate/deflate while triggering snapshots. Verify snapshot completes without panic or data corruption. The excluded_pages list in the snapshot must be consistent (no partial state from a concurrent resize).

### testing-upgrade.AC3.9
- **Criterion:** Parallel UFFD faults with balloon-reclaimed pages: zero-fill path doesn't race with demand-paging
- **Test type:** integration
- **Test file:** `tests/test_cases/src/test_uffd_balloon_parallel.rs`
- **Test description:** Host: boot multi-vCPU VM with balloon, inflate to reclaim pages, snapshot, restore with UFFD. Multiple vCPUs fault on pages simultaneously, including balloon-reclaimed addresses. Verify zero-fill path for reclaimed pages doesn't race with the UFFD demand-paging path, no SIGBUS or data corruption.

---

## AC4: Refactoring Complete

### testing-upgrade.AC4.1
- **Criterion:** `PageTracker` exists in `src/vmm/src/uffd/page_tracker.rs` with no syscall dependencies
- **Test type:** build
- **Test file:** `src/vmm/src/uffd/page_tracker.rs`
- **Test description:** The file exists and compiles. Verify via `cargo build -p vmm --features uffd,snapshot`. The module contains only pure logic (atomic bitmap ops, address translation), no uffd/KVM/libc syscall calls. The `UffdHandler` (syscall-dependent) is in `src/vmm/src/uffd/handler.rs`.
- **Verification:** human (verify file exists, `cargo build` succeeds, inspect that no syscall imports exist in page_tracker.rs)

### testing-upgrade.AC4.2
- **Criterion:** Block request types exist in `src/devices/src/virtio/block/request.rs` with no async runtime dependency
- **Test type:** build
- **Test file:** `src/devices/src/virtio/block/request.rs`
- **Test description:** The file exists and compiles. Verify via `cargo build -p devices --features blk`. The module contains `RequestHeader`, `DiscardWriteData`, `Request` enum, `AsyncWorkerMetrics`, and related types. No `tokio::` imports, no `async fn`, no runtime dependency.
- **Verification:** human (verify file exists, `cargo build` succeeds, inspect no async runtime imports)

### testing-upgrade.AC4.3
- **Criterion:** FUSE dispatch exists in `src/devices/src/virtio/fs/fuse_dispatch.rs` with no FileSystem backend dependency
- **Test type:** unit
- **Test file:** `src/devices/src/virtio/fs/fuse_dispatch.rs`
- **Test description:** The module contains `is_valid_len()` and `classify_opcode()` as pure functions with no `FileSystem` trait dependency. Unit tests in the module verify: length boundary validation (0, max, max+1, u32::MAX); known opcode classification (Lookup, Init, Destroy); unknown opcode returns None (0, 9999).

### testing-upgrade.AC4.4
- **Criterion:** `#[cfg(loom)]` atomic import shims present in `dirty_bitmap.rs`, `reclaimed_bitmap.rs`, `page_tracker.rs`
- **Test type:** build
- **Test file:** `src/vmm/src/dirty_bitmap.rs`, `src/devices/src/virtio/balloon/reclaimed_bitmap.rs`, `src/vmm/src/uffd/page_tracker.rs`
- **Test description:** Each file has `#[cfg(not(loom))] use std::sync::atomic::{...}` and `#[cfg(loom)] use loom::sync::atomic::{...}` import pairs. Verify via `RUSTFLAGS="--cfg loom" cargo build -p vmm` and `RUSTFLAGS="--cfg loom" cargo build -p devices --features net`.
- **Verification:** human (inspect files for shim presence, run loom-configured build)

### testing-upgrade.AC4.5
- **Criterion:** `just test` passes after all extractions (behavior-preserving refactoring)
- **Test type:** human (infrastructure invocation)
- **Test file:** N/A (runs existing unit tests)
- **Test description:** `just test` runs the full unit test suite. All tests that passed before Phase 2 must still pass after the extraction refactoring. No behavioral changes.
- **Verification:** human (run `just test`, compare pass rate with pre-refactoring baseline)

### testing-upgrade.AC4.6
- **Criterion:** Miri spike result documented: GuestMemoryMmap either works under Miri (descriptor_utils gets Miri coverage) or doesn't (descriptor_utils tested via fuzzing+ASan only)
- **Test type:** human (spike + documentation)
- **Test file:** N/A
- **Test description:** Phase 2, Task 5 writes a canary test using `GuestMemoryMmap::from_ranges` and runs it under `cargo +nightly miri test`. Expected result: Miri fails with "unsupported Miri functionality: can't call foreign function `mmap64`". This determines that descriptor_utils is covered by fuzzing (Phase 4) and ASan (Phase 5) instead of Miri. The outcome is documented in the implementation plan.
- **Verification:** human (run Miri spike, document result)

---

## AC5: Justfile as Test Runner

### testing-upgrade.AC5.1
- **Criterion:** `Makefile` does not exist at project root
- **Test type:** human
- **Test file:** N/A
- **Test description:** `ls Makefile` returns "No such file or directory". The Makefile is deleted in Phase 1, Task 9.
- **Verification:** human (run `ls Makefile`, observe error)

### testing-upgrade.AC5.2
- **Criterion:** `justfile` exists at project root with all targets documented in Section 9
- **Test type:** human
- **Test file:** `justfile`
- **Test description:** The justfile exists and `just --list` shows all targets: check, build, test, integration, miri, proptest, proptest-long, loom, fuzz, fuzz-all, fuzz-list, fuzz-corpus, asan, integration-asan, shuttle, kani, kani-proof, mutants, mutants-diff, all, safety. Phase 1 creates stubs; later phases replace stubs with implementations.
- **Verification:** human (run `just --list`, verify all targets present)

### testing-upgrade.AC5.3
- **Criterion:** `just build` produces the release library (replaces `make`)
- **Test type:** human
- **Test file:** N/A
- **Test description:** `just build` runs `cargo build --release --features embedded_init,snapshot,uffd,blk,vhost-user` and produces the release library artifact.
- **Verification:** human (run `just build`, observe successful compilation)

### testing-upgrade.AC5.4
- **Criterion:** `just integration <name>` runs a single named integration test
- **Test type:** human
- **Test file:** N/A
- **Test description:** `just integration configure-vm-1cpu-256MiB` runs only the named test. Verify the run.sh invocation filters to the single test.
- **Verification:** human (run `just integration configure-vm-1cpu-256MiB`, observe single test runs)

### testing-upgrade.AC5.5
- **Criterion:** `just all` runs test + miri + proptest + loom + shuttle as a compound target
- **Test type:** human
- **Test file:** `justfile`
- **Test description:** The `all` target in justfile has dependencies `check test miri proptest loom shuttle`. Running `just all` invokes all six targets sequentially.
- **Verification:** human (run `just all`, observe all subtargets execute; or inspect justfile `all:` line)

### testing-upgrade.AC5.6
- **Criterion:** `just safety` runs asan + miri + fuzz-all + kani as a compound target
- **Test type:** human
- **Test file:** `justfile`
- **Test description:** The `safety` target in justfile has dependencies `asan miri fuzz-all kani`. Running `just safety` invokes all four targets.
- **Verification:** human (run `just safety` or inspect justfile `safety:` line)

---

## Human Verification Required

| AC | Reason | Verification Approach |
|----|--------|-----------------------|
| testing-upgrade.AC1.1 | Structural assertion (no C API source) -- verified by grep, not a runtime test | Run `grep -r 'pub extern "C"' src/libkrun/src/lib.rs`, confirm empty output |
| testing-upgrade.AC1.2 | Compilation check -- types accessible if code compiles | Run `cargo check --features embedded_init,snapshot,uffd,blk,vhost-user` |
| testing-upgrade.AC1.3 | `just check` is a tool invocation, not a test | Run `just check`, observe exit code 0 |
| testing-upgrade.AC1.4 | Integration test suite run -- flaky by nature, requires VM | Run `just integration`, observe 5-6/6 passing |
| testing-upgrade.AC2.1 | `just miri` is a tool invocation running existing tests under Miri | Run `just miri`, observe all suites pass |
| testing-upgrade.AC2.2 | Fuzz targets build/run verification requires invoking cargo-fuzz | Run `just fuzz-list` (5 targets), `just fuzz <target>` for each |
| testing-upgrade.AC2.7 | ASan is a runtime instrumentation overlay, not a test | Run `just asan`, observe no ASan violations |
| testing-upgrade.AC2.8 | ASan on integration tests requires VM execution | Run `just integration-asan`, observe no ASan violations |
| testing-upgrade.AC2.9 | Mutation testing is a tool invocation producing a report | Run `just mutants`, verify `mutants.out/outcomes.json` + `docs/mutation-baseline.md` |
| testing-upgrade.AC2.10 | Time-gated fuzz run -- cannot assert "no crashes" in a unit test | Run `just fuzz-all 60`, observe all 5 targets exit cleanly |
| testing-upgrade.AC2.11 | Code review of justfile structure | Read justfile, verify single `features` variable referenced everywhere |
| testing-upgrade.AC4.1 | File existence + no-syscall-dependency is a structural property | Inspect `src/vmm/src/uffd/page_tracker.rs`, verify no uffd/KVM syscall imports |
| testing-upgrade.AC4.2 | File existence + no-async-dependency is a structural property | Inspect `src/devices/src/virtio/block/request.rs`, verify no tokio imports |
| testing-upgrade.AC4.4 | Loom shim presence is a structural property | Inspect files for `#[cfg(loom)]` / `#[cfg(not(loom))]` import pairs |
| testing-upgrade.AC4.5 | `just test` is a tool invocation checking behavior preservation | Run `just test`, compare with pre-refactoring baseline |
| testing-upgrade.AC4.6 | Miri spike is a one-time investigation with documented outcome | Run Miri spike canary test, document result |
| testing-upgrade.AC5.1 | File absence check | Run `ls Makefile`, confirm not found |
| testing-upgrade.AC5.2 | Justfile content inspection | Run `just --list`, verify all targets present |
| testing-upgrade.AC5.3 | Build invocation | Run `just build`, observe success |
| testing-upgrade.AC5.4 | Single-test invocation | Run `just integration <name>`, observe single test runs |
| testing-upgrade.AC5.5 | Compound target inspection | Run `just all` or inspect justfile |
| testing-upgrade.AC5.6 | Compound target inspection | Run `just safety` or inspect justfile |

---

## Coverage Summary

| AC | Automated | Human | Notes |
|----|-----------|-------|-------|
| AC1.1 | | X | Structural: grep for `pub extern "C"` |
| AC1.2 | | X | Structural: `cargo check` compilation |
| AC1.3 | | X | Tool invocation: `just check` |
| AC1.4 | | X | Integration suite run (flaky VM tests) |
| AC2.1 | | X | Tool invocation: `just miri` on existing tests |
| AC2.2 | | X | Tool invocation: `just fuzz-list` + `just fuzz <target>` |
| AC2.3 | X | | Loom tests in dirty_bitmap, reclaimed_bitmap, page_tracker |
| AC2.4 | X | | Shuttle tests in async_worker, balloon/device, virtio/device |
| AC2.5 | X | | Proptest suites in 6 modules |
| AC2.6 | X | | 6 Kani proofs in kani-proofs/ |
| AC2.7 | | X | Tool invocation: `just asan` |
| AC2.8 | | X | Tool invocation: `just integration-asan` |
| AC2.9 | | X | Tool invocation + documentation: `just mutants` |
| AC2.10 | | X | Time-gated: `just fuzz-all 60` |
| AC2.11 | | X | Code review of justfile |
| AC3.1 | X | | Integration test: `test_balloon_snapshot_uffd.rs` |
| AC3.2 | X | | Integration test: `test_block_snapshot_uffd.rs` |
| AC3.3 | X | | Integration test: `test_virtiofs_dax_snapshot.rs` |
| AC3.4 | X | | Integration test: `test_full_stack.rs` (stretch goal, may be deferred) |
| AC3.5 | X | | Integration test: `test_block_backend_errors.rs` |
| AC3.6 | X | | Integration test: `test_block_backend_slow.rs` |
| AC3.7 | X | | Integration test: `test_virtiofs_minimal.rs` |
| AC3.8 | X | | Integration test: `test_balloon_snapshot_race.rs` |
| AC3.9 | X | | Integration test: `test_uffd_balloon_parallel.rs` |
| AC4.1 | | X | Structural: file exists, no syscall deps |
| AC4.2 | | X | Structural: file exists, no async runtime deps |
| AC4.3 | X | | Unit tests in fuse_dispatch.rs (is_valid_len, classify_opcode) |
| AC4.4 | | X | Structural: loom shim presence |
| AC4.5 | | X | Tool invocation: `just test` post-refactoring |
| AC4.6 | | X | One-time spike with documented outcome |
| AC5.1 | | X | File absence: Makefile deleted |
| AC5.2 | | X | Tool invocation: `just --list` |
| AC5.3 | | X | Tool invocation: `just build` |
| AC5.4 | | X | Tool invocation: `just integration <name>` |
| AC5.5 | | X | Compound target: `just all` |
| AC5.6 | | X | Compound target: `just safety` |

---

## Automated Test Inventory

This section lists every automated test that is written as part of this project (not tool invocations or structural checks).

### Proptest Suites (Phase 3)

| Module | Test Function | Property Verified |
|--------|--------------|-------------------|
| `src/vmm/src/dirty_bitmap.rs` | `prop_mark_then_drain_returns_all_pages` | mark N pages, drain returns exactly N |
| `src/vmm/src/dirty_bitmap.rs` | `prop_drain_empties_bitmap` | second drain after first returns empty |
| `src/vmm/src/dirty_bitmap.rs` | `prop_mark_idempotent` | marking same page twice yields count 1 |
| `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` | `prop_mark_range_count` | mark_range count matches |
| `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` | `prop_mark_range_iter` | iter_set_pages returns exact PFNs |
| `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` | `prop_mark_clear_roundtrip` | mark then clear then is_set returns false |
| `src/arch/src/x86_64/gdt.rs` | `prop_gdt_base_roundtrip` | get_base(gdt_entry(flags,base,limit)) == base |
| `src/arch/src/x86_64/gdt.rs` | `prop_kvm_segment_base_preserved` | kvm_segment_from_gdt preserves base |
| `src/vmm/src/snapshot.rs` | `prop_vm_snapshot_bincode_roundtrip` | VmSnapshot serialize/deserialize identity |
| `src/vmm/src/snapshot.rs` | `prop_snapshot_header_roundtrip` | SnapshotHeader fields preserved |
| `src/vmm/src/snapshot.rs` | `prop_invalid_magic_always_fails` | wrong magic rejected by validation |
| `src/vmm/src/uffd/page_tracker.rs` | `prop_guest_to_host_in_range` | returns Some for in-range addresses |
| `src/vmm/src/uffd/page_tracker.rs` | `prop_guest_to_host_before_region` | returns None for before-region addresses |
| `src/vmm/src/uffd/page_tracker.rs` | `prop_guest_to_host_after_region` | returns None for after-region addresses |
| `src/vmm/src/uffd/page_tracker.rs` | `prop_mark_loaded_deduplication` | same page marked twice counts as 1 |
| `src/libkrun/src/lib.rs` | `prop_zero_vcpus_always_fails` | vm_config(0, N) -> ZeroVcpus |
| `src/libkrun/src/lib.rs` | `prop_nonzero_vcpus_succeeds_validation` | vm_config(N>0, M) does not return ZeroVcpus |
| `src/libkrun/src/lib.rs` | `tag_too_long_boundary` | tag >36 bytes -> TagTooLong |

### Loom Tests (Phase 3)

| Module | Test Function | Property Verified |
|--------|--------------|-------------------|
| `src/vmm/src/dirty_bitmap.rs` | `loom_mark_and_drain_no_page_lost` | concurrent mark + drain: no page lost |
| `src/vmm/src/dirty_bitmap.rs` | `loom_two_markers_both_present` | two markers on different pages: both present |
| `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` | `loom_mark_clear_consistency` | concurrent mark + clear: count in [0,1] |
| `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` | `loom_concurrent_distinct_marks` | distinct marks: both set, count = 2 |
| `src/vmm/src/uffd/page_tracker.rs` | `loom_mark_loaded_dedup_concurrent` | Preload + Fault on same page: exactly 1 loaded |
| `src/vmm/src/uffd/page_tracker.rs` | `loom_mark_loaded_different_pages` | different pages: both tracked, counts correct |

### Shuttle Tests (Phase 5)

| Module | Test Function | Property Verified |
|--------|--------------|-------------------|
| `src/devices/src/virtio/block/async_worker.rs` | `shuttle_quiesce_ack_no_deadlock` | quiesce handshake: no deadlock, no missed wakeup |
| `src/devices/src/virtio/block/async_worker.rs` | `shuttle_quiesce_two_cycles_no_deadlock` | two quiesce/resume cycles: no deadlock |
| `src/devices/src/virtio/balloon/device.rs` | `shuttle_balloon_condvar_no_deadlock` | balloon await_target: no deadlock |
| `src/devices/src/virtio/balloon/device.rs` | `shuttle_balloon_incremental_updates_no_deadlock` | incremental updates: VMM loop terminates |
| `src/devices/src/virtio/device.rs` | `shuttle_device_state_transition_no_torn_read` | Inactive->Activated: no torn read |
| `src/devices/src/virtio/device.rs` | `shuttle_device_state_post_activation_consistent` | post-activation readers: all see Activated |

### Kani Proofs (Phase 6)

| Proof File | Proof Name | Property Verified | Bound |
|-----------|-----------|-------------------|-------|
| `kani-proofs/src/lib.rs` | PageTracker deduplication | mark_loaded twice increments counter once | 128 pages |
| `kani-proofs/src/lib.rs` | DirtyBitmap bounds | mark_dirty with any u64 never panics/OOB | 256 pages |
| `kani-proofs/src/lib.rs` | ReclaimedBitmap consistency | mark->is_set=true, clear->is_set=false, count=popcount | 256 pages |
| `kani-proofs/src/lib.rs` | Snapshot header validation | rejects invalid magic/version/region/vcpu count | unbounded |
| `kani-proofs/src/lib.rs` | GDT round-trip | get_base(gdt_entry)==base, get_limit equivalent | all inputs |
| `kani-proofs/src/lib.rs` | Address translation | guest_to_host correct for in/out of range | 4 regions |

### Fuzz Harnesses (Phase 4)

| Harness File | Target | Attack Surface |
|-------------|--------|----------------|
| `fuzz/fuzz_targets/fuzz_fuse_parsing.rs` | FUSE message parsing | Server::handle_message with arbitrary bytes via mock FileSystem |
| `fuzz/fuzz_targets/fuzz_descriptor_chain.rs` | Virtio descriptor chain | DescriptorChain::checked_new + Reader/Writer iteration with random descriptor table |
| `fuzz/fuzz_targets/fuzz_snapshot_deser.rs` | Snapshot deserialization | bincode::deserialize::<VmSnapshot> and ::<IncrementalSnapshot> with arbitrary bytes |
| `fuzz/fuzz_targets/fuzz_vhost_user_msg.rs` | Vhost-user messages | ByteValued deserialization of message headers from arbitrary bytes |
| `fuzz/fuzz_targets/fuzz_block_request.rs` | Block request header | RequestHeader ByteValued deserialization + request type discrimination |

### Unit Tests (Phase 2)

| Module | Test Function | What It Verifies |
|--------|--------------|------------------|
| `src/devices/src/virtio/fs/fuse_dispatch.rs` | `test_is_valid_len_boundary` | Length validation at 0, max, max+1, u32::MAX |
| `src/devices/src/virtio/fs/fuse_dispatch.rs` | `test_classify_opcode_known` | Known opcodes classified correctly |
| `src/devices/src/virtio/fs/fuse_dispatch.rs` | `test_classify_opcode_unknown` | Unknown opcodes return None |
| `src/vmm/src/dirty_bitmap.rs` | `miri_spike_pure_atomics` | DirtyBitmap pure atomics are Miri-compatible |

### Integration Tests (Phase 7)

| Test File | AC | Scenario |
|-----------|-----|----------|
| `tests/test_cases/src/test_balloon_snapshot_uffd.rs` | AC3.1 | Balloon inflate -> snapshot -> UFFD restore -> zero-fill -> deflate -> reuse |
| `tests/test_cases/src/test_block_snapshot_uffd.rs` | AC3.2 | Block write -> snapshot -> UFFD restore -> read back matches |
| `tests/test_cases/src/test_virtiofs_dax_snapshot.rs` | AC3.3 | VirtioFS+DAX mount -> write -> snapshot -> restore -> read back |
| `tests/test_cases/src/test_full_stack.rs` | AC3.4 | TSI + balloon + snapshot -> restore -> TSI resumes (stretch goal) |
| `tests/test_cases/src/test_block_backend_errors.rs` | AC3.5 | Failing backend: guest I/O errors, device stable |
| `tests/test_cases/src/test_block_backend_slow.rs` | AC3.6 | Slow backend: queue doesn't stall, metrics correct |
| `tests/test_cases/src/test_virtiofs_minimal.rs` | AC3.7 | Minimal FS: ENOSYS for unsupported ops, DAX works |
| `tests/test_cases/src/test_balloon_snapshot_race.rs` | AC3.8 | Rapid inflate/deflate during snapshot: no race |
| `tests/test_cases/src/test_uffd_balloon_parallel.rs` | AC3.9 | Multi-vCPU UFFD faults + balloon-reclaimed pages: no race |

### Integration Test Helper Modules (Phase 7)

| File | Purpose |
|------|---------|
| `tests/test_cases/src/failing_block_backend.rs` | AsyncBlockBackend returning errors on configured sectors |
| `tests/test_cases/src/slow_block_backend.rs` | AsyncBlockBackend with configurable delays |
| `tests/test_cases/src/minimal_filesystem.rs` | FileSystem impl with only lookup+read, ENOSYS otherwise |
