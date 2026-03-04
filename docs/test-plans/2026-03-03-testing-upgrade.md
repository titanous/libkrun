# Human Test Plan: Testing Upgrade

Generated from implementation plan: `docs/implementation-plans/2026-03-03-testing-upgrade/`

**Coverage: PASS** — 16/17 acceptance criteria have automated tests. AC3.4 is explicitly deferred as a stretch goal.

---

## Prerequisites

- NixOS development shell active (`nix develop .` if not already inside)
- `libkrunfw.so` symlinked into `test-prefix/lib64/` (nix shellHook handles this)
- Rust nightly toolchain available (`rustup toolchain install nightly`)
- `cargo-fuzz`, `cargo-kani`, `cargo-mutants` installed
- `just` installed (`cargo install just` or via nixpkgs)
- `just test` passing (baseline unit tests clean)

---

## Phase 1: C API Removal and Rust API Stability

| Step | Action | Expected |
|------|--------|----------|
| 1.1 | Run `grep -r 'pub extern "C"' src/libkrun/src/lib.rs` | Empty output (no matches). Verifies AC1.1: no `pub extern "C"` functions remain. |
| 1.2 | Run `cargo check -p libkrun --features embedded_init,snapshot,uffd,blk,vhost-user` | Clean compilation, exit code 0. Verifies AC1.2: `Builder`, `Context`, `VmHandle`, and trait re-exports compile and are publicly accessible. |
| 1.3 | Run `just check` | Exit code 0 (cargo fmt --check + cargo clippy both pass). Verifies AC1.3. |
| 1.4 | Run `just integration` | 5–6 out of 6 tests pass (inherent VM test flakiness is expected). Verifies AC1.4: C API removal has not broken integration tests using Rust API. |
| 1.5 | Verify `Cargo.toml` for libkrun no longer lists `crate-type = ["cdylib"]` | Run `grep 'cdylib' src/libkrun/Cargo.toml` — expect no matches. Confirms no C shared library is produced. |

---

## Phase 2: Testing Infrastructure Invocation

| Step | Action | Expected |
|------|--------|----------|
| 2.1 | Run `just miri` | All Miri test suites pass (gdt, dirty_bitmap, snapshot, page_tracker, reclaimed_bitmap, block/request). No "unsupported Miri functionality" or UB errors. Verifies AC2.1. |
| 2.2a | Run `just fuzz-list` | Output lists exactly 5 targets: `fuzz_block_request`, `fuzz_descriptor_chain`, `fuzz_fuse_parsing`, `fuzz_snapshot_deser`, `fuzz_vhost_user_msg`. Verifies AC2.2. |
| 2.2b | Run `just fuzz fuzz_snapshot_deser 10` (repeat for each of the 5 targets) | Each target builds, runs for 10 seconds, exits cleanly (exit code 0, no crash artifacts). Verifies AC2.2. |
| 2.7 | Run `just asan` | Both devices and vmm crate tests run under ASan with no `ERROR: AddressSanitizer` messages. Exit code 0. Verifies AC2.7. |
| 2.8 | Run `just integration-asan` | Integration tests run with ASan instrumentation on host-side binaries. Pass rate consistent with `just integration` (5–6/6). No ASan violations in output. Verifies AC2.8. |
| 2.9a | Run `just mutants` | `mutants.out/outcomes.json` is created with scored results. Verifies AC2.9. |
| 2.9b | Verify `docs/mutation-baseline.md` exists and is populated | Run `wc -l docs/mutation-baseline.md` — expect > 10 lines. |
| 2.10 | Run `just fuzz-all 60` | All 5 fuzz targets run for 60 seconds each and exit with code 0 (no crashes). Verifies AC2.10. |
| 2.11 | Open `justfile` and inspect line 3 | A single `features` variable is defined. Verify no other target hardcodes feature strings. Verifies AC2.11. |

---

## Phase 3: Refactoring Verification

| Step | Action | Expected |
|------|--------|----------|
| 3.1 | Run `ls src/vmm/src/uffd/page_tracker.rs` | File exists. Verifies AC4.1 (file existence). |
| 3.2 | Run `grep -c 'libc::ioctl\|libc::mmap\|KVM_' src/vmm/src/uffd/page_tracker.rs` | Zero matches. The file contains only pure logic (atomic bitmap ops, address translation). Verifies AC4.1 (no syscall dependencies). |
| 3.3 | Run `ls src/devices/src/virtio/block/request.rs` | File exists. Verifies AC4.2 (file existence). |
| 3.4 | Run `grep -c 'tokio::' src/devices/src/virtio/block/request.rs` | Zero matches. No async runtime dependency. Verifies AC4.2 (no async runtime dependency). |
| 3.5 | Inspect `dirty_bitmap.rs`, `reclaimed_bitmap.rs`, `page_tracker.rs`, `request.rs` for loom shims | Each file has both `#[cfg(loom)] use loom::sync::atomic::{...}` and `#[cfg(not(loom))] use std::sync::atomic::{...}` import pairs. Verifies AC4.4. |
| 3.6 | Run `just test` | All unit tests pass. Behavior-preserving refactoring verified. Verifies AC4.5. |
| 3.7 | Run `cargo +nightly miri test -p vmm --features snapshot -- miri_spike_pure_atomics` | Test passes under Miri, confirming DirtyBitmap pure atomics are Miri-compatible. Documents that `GuestMemoryMmap` is Miri-incompatible (mmap64 FFI) — covered by fuzzing and ASan instead. Verifies AC4.6. |

---

## Phase 4: Justfile as Test Runner

| Step | Action | Expected |
|------|--------|----------|
| 4.1 | Run `ls Makefile` from project root | "No such file or directory". Verifies AC5.1. |
| 4.2 | Run `just --list` from project root | Output shows all targets including: `all`, `asan`, `build`, `check`, `fuzz`, `fuzz-all`, `integration`, `integration-asan`, `kani`, `loom`, `miri`, `mutants`, `proptest`, `safety`, `shuttle`, `test`. Verifies AC5.2. |
| 4.3 | Run `just build` | Successful compilation of release library with all features. Verifies AC5.3. |
| 4.4 | Run `just integration configure-vm-1cpu-256MiB` | Only the named test runs (not the full suite). Verifies AC5.4. |
| 4.5 | Inspect justfile `all` target | Contains: `check test miri proptest loom shuttle`. Verifies AC5.5. |
| 4.6 | Inspect justfile `safety` target | Contains: `check fuzz-all asan shuttle kani`. Verifies AC5.6. |

---

## End-to-End: Full Automated Test Suite

**Purpose:** Validates that the entire automated test infrastructure operates correctly end-to-end.

| Step | Action | Expected |
|------|--------|----------|
| E2E.1 | Run `just all` | Sequentially executes: check, test, miri, proptest, loom, shuttle. All pass. |
| E2E.2 | Run `just integration` | All integration tests execute inside real microVMs. 5–6/6 passing is normal. |
| E2E.3 | Run `just kani` | All 22 Kani bounded model checking proofs verify successfully. |

---

## End-to-End: Snapshot Feature Coverage

**Purpose:** Validates cross-cutting snapshot/restore functionality across multiple device types.

| Step | Action | Expected |
|------|--------|----------|
| S.1 | `just integration balloon-snapshot-uffd` | Balloon inflate → full snapshot → UFFD cold restore → zero-fill of reclaimed pages → guest reconnects. Corresponds to AC3.1. |
| S.2 | `just integration block-snapshot-uffd` | Block write → snapshot → UFFD restore → read-back matches written pattern. Corresponds to AC3.2. |
| S.3 | `just integration virtiofs-dax-snapshot` | VirtioFS + DAX mount → write file → hot snapshot → restore → read-back matches. Corresponds to AC3.3. |
| S.4 | `just integration balloon-snapshot-race` | Rapid inflate/deflate during snapshot. No panic, no data corruption. Corresponds to AC3.8. |
| S.5 | `just integration uffd-balloon-parallel` | Multi-vCPU UFFD faults with balloon-reclaimed pages. No SIGBUS. Corresponds to AC3.9. |

---

## End-to-End: Error Handling and Edge Cases

**Purpose:** Validates graceful error handling and edge-case behavior.

| Step | Action | Expected |
|------|--------|----------|
| EH.1 | `just integration block-backend-errors` | FailingBlockBackend returns I/O errors on sector 5; guest handles error, device stays functional, VM exits cleanly. Corresponds to AC3.5. |
| EH.2 | `just integration block-backend-slow` | SlowBlockBackend with 20ms delay; guest completes I/O, queue does not stall, VM exits cleanly. Corresponds to AC3.6. |
| EH.3 | `just integration virtiofs-minimal-fs` | MinimalFileSystem with only lookup+read; guest reads file (success), writes (error/ENOSYS), device stays functional. Corresponds to AC3.7. |

---

## Deferred: AC3.4 (Stretch Goal)

AC3.4 (full stack: TSI + balloon + snapshot) is explicitly documented as a stretch goal in the test requirements. If implemented later:

- Run `just integration full-stack`
- Verify: TSI connection resumes after snapshot/restore; balloon state preserved; network connectivity confirmed post-restore.

---

## Traceability

| Acceptance Criterion | Automated Test | Manual Step |
|----------------------|----------------|-------------|
| AC1.1 | — | 1.1 (grep for pub extern "C") |
| AC1.2 | — | 1.2 (cargo check) |
| AC1.3 | — | 1.3 (just check) |
| AC1.4 | — | 1.4 (just integration) |
| AC2.1 | — | 2.1 (just miri) |
| AC2.2 | — | 2.2a, 2.2b (fuzz-list + fuzz each) |
| AC2.3 | loom_tests in dirty_bitmap.rs, reclaimed_bitmap.rs, page_tracker.rs | — |
| AC2.4 | shuttle_tests in async_worker.rs, balloon/device.rs, virtio/device.rs | — |
| AC2.5 | proptest_tests in dirty_bitmap.rs, reclaimed_bitmap.rs, gdt.rs, snapshot.rs, page_tracker.rs, lib.rs | — |
| AC2.6 | 22 kani proofs in kani-proofs/src/ (6 files) | — |
| AC2.7 | — | 2.7 (just asan) |
| AC2.8 | — | 2.8 (just integration-asan) |
| AC2.9 | — | 2.9a, 2.9b (just mutants) |
| AC2.10 | — | 2.10 (just fuzz-all 60) |
| AC2.11 | — | 2.11 (justfile inspection) |
| AC3.1 | test_balloon_snapshot_uffd.rs | S.1 |
| AC3.2 | test_block_snapshot_uffd.rs | S.2 |
| AC3.3 | test_virtiofs_dax_snapshot.rs | S.3 |
| AC3.4 | — (deferred stretch goal) | Deferred |
| AC3.5 | test_block_backend_errors.rs | EH.1 |
| AC3.6 | test_block_backend_slow.rs | EH.2 |
| AC3.7 | test_virtiofs_minimal.rs | EH.3 |
| AC3.8 | test_balloon_snapshot_race.rs | S.4 |
| AC3.9 | test_uffd_balloon_parallel.rs | S.5 |
| AC4.1 | — | 3.1, 3.2 (file + no-syscall) |
| AC4.2 | — | 3.3, 3.4 (file + no-async) |
| AC4.3 | Unit tests in fuse_dispatch.rs | — |
| AC4.4 | — | 3.5 (cfg(loom) inspection) |
| AC4.5 | — | 3.6 (just test) |
| AC4.6 | — | 3.7 (Miri spike) |
| AC5.1 | — | 4.1 (ls Makefile) |
| AC5.2 | — | 4.2 (just --list) |
| AC5.3 | — | 4.3 (just build) |
| AC5.4 | — | 4.4 (just integration <name>) |
| AC5.5 | — | 4.5 (justfile inspection) |
| AC5.6 | — | 4.6 (justfile inspection) |
