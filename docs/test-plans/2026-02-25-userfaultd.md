# Userfaultfd Demand-Paging Test Plan

## Prerequisites

- Linux host with kernel UFFD support (`/proc/sys/vm/unprivileged_userfaultfd` set to 1, or running as root)
- Inside the nix development shell
- `libkrunfw` symlinked in `test-prefix/lib64/`
- Unit tests passing: `cargo test -p vmm --features snapshot` and `cargo test -p vmm --features uffd`
- Integration tests passing: `make test FEATURE_FLAGS="--features embedded_init"`

## Phase 1: AC1.5 -- Verify No Snapshot IDs or Lineage Concepts

This is the only acceptance criterion marked as "Human verification" in the requirements.

| Step | Action | Expected |
|------|--------|----------|
| 1 | Open `src/vmm/src/snapshot_store.rs` lines 49-113 (trait definitions) | File opens in editor |
| 2 | Inspect `SnapshotStore` trait. Check every method signature for parameters or return types containing "id", "lineage", "parent", "chain", or identity-related fields | No method has ID or lineage parameters. Methods are: `read_vmstate()`, `read_page(guest_addr: u64)`, `preload(regions)`, `write_vmstate(data)`, `write_pages(pages)`, `close()`. Only `guest_addr` (physical address) is used for page identity. |
| 3 | Inspect `SnapshotStoreFactory` trait. Check `create` method signature | `create(self: Box<Self>) -> SendBoxFuture<'static, io::Result<Box<dyn SnapshotStore>>>`. No ID argument, no associated types for identity. Factory is consumed by value (single-use). |
| 4 | Inspect `FsSnapshotStoreFactory::new()` | Takes `base_path` and `incremental_paths`. Paths are filesystem locations, not snapshot identifiers. No lineage or chain concept in the API. |

## Phase 2: SnapshotStore Trait Contract Verification

| Step | Action | Expected |
|------|--------|----------|
| 1 | Run `cargo test -p vmm --features snapshot` | All 13 snapshot_store tests pass (AC1.1-AC1.4, AC2.1-AC2.4, AC4.4, plus bounds tests and vmstate merge tests) |
| 2 | Verify test output includes: `test_ac1_1`, `test_ac1_3`, `test_ac1_4`, `test_ac2_1`, `test_ac2_2`, `test_ac2_3`, `test_ac2_4` | All named tests appear in output as `ok` |
| 3 | Verify no temp directory leaks: `ls /tmp/libkrun_test_*` | No leftover directories (tests clean up on success) |

## Phase 3: UFFD Handler Unit Tests

| Step | Action | Expected |
|------|--------|----------|
| 1 | Run `cargo test -p vmm --features uffd` | All UFFD unit tests pass |
| 2 | If any test reports "Permission denied", verify `/proc/sys/vm/unprivileged_userfaultfd` is set to 1 | Set with `sysctl -w vm.unprivileged_userfaultfd=1` |
| 3 | Verify `test_page_tracker_concurrent_marking` passes (4 threads, 1000 pages) | All 1000 pages marked, 500 preload + 500 fault, no panic |
| 4 | Verify `test_preload_task_with_uffd_and_mmap` passes | Preload data written to mmap'd memory at correct addresses |

## Phase 4: Backward Compatibility Regression

| Step | Action | Expected |
|------|--------|----------|
| 1 | `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-restore-full` | Full snapshot/restore cycle works through new delegation layer |
| 2 | `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-restore-incremental` | Incremental snapshot/restore works |
| 3 | `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-serial-scratch` | Serial device state survives snapshot/restore |
| 4 | `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-block-data` | Block device data survives |
| 5 | `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-incremental-state` | Incremental state preservation |
| 6 | `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-net-connectivity` | Network connectivity after restore |

## Phase 5: UFFD Integration Tests

| Step | Action | Expected |
|------|--------|----------|
| 1 | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-demand-page-only` | Empty preload, all pages demand-paged, guest outputs "OK" |
| 2 | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-preload-full` | Full preload, near-zero faults, guest outputs "OK" |
| 3 | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-preload-partial` | 50% preload, remaining pages demand-paged, guest outputs "OK" |
| 4 | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-incremental-chain` | Base + 2 incrementals, guest verifies counter==300, outputs "OK" |
| 5 | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-error-handling` | ErrorStore causes VmExit::Error, no panic, outputs "OK" |
| 6 | `make test FEATURE_FLAGS="--features embedded_init" TEST=uffd-parallel-faults` | 2-vCPU VM with 50ms delay, wall-clock < 3s, proving concurrent fault resolution |

## Traceability

| Acceptance Criterion | Automated Test | Manual Step |
|----------------------|----------------|-------------|
| userfaultd.AC1.1 | `test_ac1_1_snapshot_store_has_all_methods` | Phase 2 |
| userfaultd.AC1.2 | `test_ac1_3_trait_object_safety` (compile-time) | Phase 2 |
| userfaultd.AC1.3 | `test_ac1_3_trait_object_safety`, `test_fs_snapshot_store_bounds` | Phase 2 |
| userfaultd.AC1.4 | `test_ac1_4_factory_trait_and_object_safety`, `test_factory_bounds` | Phase 2 |
| userfaultd.AC1.5 | N/A (human verification) | Phase 1 |
| userfaultd.AC2.1 | `test_ac2_1_fs_snapshot_store_implements_trait` | Phase 2 |
| userfaultd.AC2.2 | `test_ac2_2_write_path_format_compatible` | Phase 2 |
| userfaultd.AC2.3 | `test_ac2_3_*` (3 tests) | Phase 2 |
| userfaultd.AC2.4 | `test_ac2_4_preload_chunks_with_dirty_overlay` | Phase 2 |
| userfaultd.AC2.5 | `snapshot-restore-full`, `snapshot-restore-incremental` | Phase 4 |
| userfaultd.AC3.1 | `test_uffd_handler_creation_and_registration` + 3 related | Phase 3 |
| userfaultd.AC3.2 | `test_preload_task_with_uffd_and_mmap` + code inspection | Phase 3 |
| userfaultd.AC3.3 | `test_is_eexist_constant_verification`, `test_signal_error_idempotent` | Phase 3 |
| userfaultd.AC3.4 | `test_preload_task_with_uffd_and_mmap`, `test_preload_task_updates_tracker` | Phase 3 |
| userfaultd.AC3.5 | `test_preload_task_with_uffd_and_mmap` (8KB multi-page chunk) | Phase 3 |
| userfaultd.AC3.6 | `test_signal_error`, `test_preload_task_stream_error_stops_gracefully` | Phase 3 |
| userfaultd.AC4.1 | `snapshot-restore-full` | Phase 4 |
| userfaultd.AC4.2 | `snapshot-restore-full`, `snapshot-restore-incremental` | Phase 4 |
| userfaultd.AC4.3 | All existing `test_snapshot_*.rs` tests | Phase 4 |
| userfaultd.AC4.4 | `test_ac2_2_write_path_format_compatible` | Phase 2, Phase 4 |
| userfaultd.AC5.1 | `uffd-demand-page-only` | Phase 5 |
| userfaultd.AC5.2 | `uffd-preload-full` | Phase 5 |
| userfaultd.AC5.3 | `uffd-preload-partial` | Phase 5 |
| userfaultd.AC5.4 | `uffd-incremental-chain` | Phase 5 |
| userfaultd.AC5.5 | `uffd-error-handling` | Phase 5 |
| userfaultd.AC5.6 | `uffd-parallel-faults` | Phase 5 |
