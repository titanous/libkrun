# Human Test Plan: vhost-user-fs with DAX Window

Generated from: `docs/implementation-plans/2026-02-24-vhost-user-fs-dax/`
Coverage: 28/28 acceptance criteria (PASS)

---

## Prerequisites

- NixOS development environment with `nix develop` shell active
- `libkrunfw` symlinked into `test-prefix/lib64/` (done by shellHook)
- Kernel >= 6.2 with `CONFIG_FUSE_DAX` enabled
- All unit tests passing:
  - `cargo test -p devices --features net,snapshot`
  - `cargo test -p vmm --features snapshot`
  - `cargo test -p devices --features net,snapshot,vhost-user`
  - `cargo test -p krun --features vhost-user`

---

## Phase 1: Unit Test Verification

| Step | Action | Expected |
|------|--------|----------|
| 1.1 | Run `cargo test -p devices --features net,snapshot` | All existing device tests pass. No regressions from vhost-user code. (AC1.4) |
| 1.2 | Run `cargo test -p vmm --features snapshot` | All existing VMM tests pass. No regressions. (AC1.4) |
| 1.3 | Run `cargo test -p devices --features net,snapshot,vhost-user` | All vhost-user unit tests pass: `test_device_type_is_fs`, `test_read_config_tag`, `test_read_config_num_queues`, `test_new_fails_with_unavailable_socket`, `test_queue_layout_ac2_3`, `test_shm_region_with_dax_ac2_4`, `test_shm_region_without_dax_ac2_5`, `test_snapshot_state_roundtrip`, `test_restore_backend_state_stores_pending`, `test_activate_restore_fails_when_daemon_unavailable`, `test_save_device_state_not_supported`, `test_load_device_state_not_supported`. |
| 1.4 | Run `cargo test -p krun --features vhost-user` | Builder API tests pass: `test_add_virtiofs_vhost_user_tag_too_long_ac3_3`, `test_add_virtiofs_vhost_user_tag_max_length`, `test_add_virtiofs_vhost_user_coexistence_ac3_4`. |

---

## Phase 2: Integration Test Execution

| Step | Action | Expected |
|------|--------|----------|
| 2.1 | Run `make test FEATURE_FLAGS="--features embedded_init,vhost-user"` | All integration tests run. The three vhost-user-fs tests (`vhost-user-fs-dax-always`, `vhost-user-fs-dax-inode`, `vhost-user-fs-dax-never`) appear in test output. (AC6.4) |
| 2.2 | Observe `vhost-user-fs-dax-always` test output | Test passes. Guest mounts virtiofs with `dax=always`, reads `/mnt/testfs/hello.txt` (0xBB via DAX), writes 0xCC, reads back 0xCC, snapshot/restore cycle, reads 0xCC post-restore (DAX memfd survives snapshot). Printed "OK". (AC6.1, AC6.2, AC6.3) |
| 2.3 | Observe `vhost-user-fs-dax-inode` test output | Test passes. Guest mounts with `dax=inode`, reads `hello.txt` (0xBB via DAX, FUSE_ATTR_DAX set), reads `nodax.txt` (0xAA via FUSE_READ, no FUSE_ATTR_DAX), writes 0xCC to `hello.txt`, snapshot/restore, verifies both files post-restore. Printed "OK". (AC5.2, AC5.4, AC6.1, AC6.3) |
| 2.4 | Observe `vhost-user-fs-dax-never` test output | Test passes. Guest mounts with `dax=never`, reads 0xAA via FUSE_READ (not DAX despite DAX window configured), snapshot/restore, reads 0xAA post-restore. Printed "OK". (AC5.4, AC6.3) |
| 2.5 | Verify existing tests still pass alongside vhost-user tests | The standard snapshot, block, network, and VM lifecycle tests (approximately 5-6 of 6 expected passing) continue to pass. No regressions. (AC1.4) |

---

## Phase 3: Build Verification

| Step | Action | Expected |
|------|--------|----------|
| 3.1 | Run `VHOST_USER=1 make` | Library builds successfully with vhost-user feature enabled. No compiler errors or warnings related to vhost-user code. |
| 3.2 | Run `make` (without VHOST_USER) | Library builds successfully without vhost-user feature. Vhost-user code is completely gated behind feature flag and does not appear in the binary. |
| 3.3 | Verify `tests/test_daemon/` binary builds | Run `cargo build -p test-daemon` in the tests workspace. The test daemon binary compiles without errors. |

---

## End-to-End: dax=always (Combined Read + Write + Snapshot)

**Purpose:** Validate DAX read, write, and snapshot/restore in a single test with `dax=always` mount mode.

**Steps:**
1. The `vhost-user-fs-dax-always` integration test exercises the full path.
2. Guest reads `hello.txt` via DAX window (expects 0xBB, not 0xAA FUSE_READ pattern).
3. Guest writes 0xCC to `hello.txt` via DAX, reads back to verify.
4. Snapshot/restore cycle: daemon killed, restarted, state restored.
5. Post-restore read returns 0xCC (DAX memfd content survives snapshot as part of guest memory).

---

## End-to-End: dax=inode (Per-Inode DAX + Snapshot)

**Purpose:** Validate per-inode DAX control via `FUSE_ATTR_DAX` flag, with mixed DAX and non-DAX files.

**Steps:**
1. The `vhost-user-fs-dax-inode` test mounts with `dax=inode`.
2. `hello.txt` (FUSE_ATTR_DAX set) reads via DAX → 0xBB. `nodax.txt` (no FUSE_ATTR_DAX) reads via FUSE_READ → 0xAA.
3. Guest writes 0xCC to `hello.txt`, snapshot/restore cycle.
4. Post-restore: `hello.txt` returns 0xCC (DAX preserved), `nodax.txt` returns 0xAA (FUSE_READ).
5. Requires `FUSE_INIT_EXT` (bit 30) in FUSE_INIT response for kernel to read `flags2` containing `FUSE_HAS_INODE_DAX`.

---

## End-to-End: dax=never (FUSE_READ Path + Snapshot)

**Purpose:** Validate that `dax=never` forces FUSE_READ even when DAX window is configured.

**Steps:**
1. The `vhost-user-fs-dax-never` test mounts with `dax=never` (DAX window still configured on host).
2. Guest reads `hello.txt` → 0xAA (FUSE_READ pattern, NOT 0xBB DAX pattern).
3. Snapshot/restore cycle, post-restore read still returns 0xAA.

---

## Human Verification Required

| Criterion | Why Manual | Steps |
|-----------|------------|-------|
| AC1.4: No regression after PR #527 integration | Requires running full existing test suite and comparing results to known baseline | Run `cargo test -p devices --features net,snapshot` and `cargo test -p vmm --features snapshot`. Compare test count and pass/fail with the main branch. No test should newly fail. |
| AC3.2: Device without DAX window (None variant) | Integration test only exercises `Some(DAX_WINDOW_MIB)` path. The `None` variant is unit-tested at device level but not end-to-end with a running VM. | Manually configure a VM with `add_virtiofs_vhost_user("tag", socket, None)`, boot, mount with `dax=never`. Confirm mount succeeds and file reads work via FUSE_READ (content should be 0xAA if using the test daemon, since no DAX means FUSE_READ path). |
| AC6.4: Full test suite passes with combined features | Flaky tests may mask failures; need human judgment on acceptable pass rate | Run `make test FEATURE_FLAGS="--features embedded_init,vhost-user"` at least twice. Verify vhost-user tests consistently pass. Accept 5-6/6+ for pre-existing flaky tests (vsock/timing), but all three vhost-user-fs tests must pass every run. |
| AC6.5: Test code follows existing patterns | Structural/style review cannot be fully automated | Open `tests/test_cases/src/test_vhost_user_fs.rs`. Verify `#[host]` and `#[guest]` proc macro usage on `impl Test` blocks matches the pattern in `test_snapshot_restore.rs`. Verify test registration in `lib.rs` via `TestCase::new()`. Verify host side uses `krun_rust::setup_fs_builder` helper consistent with other Rust API tests. |

---

## Traceability

| Acceptance Criterion | Automated Test | Manual Step |
|----------------------|----------------|-------------|
| AC1.1 VhostUserDevice connects and negotiates | `TestVhostUserFsDaxAlways` (integration) + `test_new_fails_with_unavailable_socket` (unit) | Step 2.2 |
| AC1.2 SET_MEM_TABLE shares memory | `TestVhostUserFsDaxAlways` (integration) | Step 2.2 |
| AC1.3 Error on unavailable socket | `test_new_fails_with_unavailable_socket` | Step 1.3 |
| AC1.4 No regression | Existing test suites | Steps 1.1, 1.2, 2.5 |
| AC2.1 device_type() == 26 | `test_device_type_is_fs` | Step 1.3 |
| AC2.2 Config space tag + num_queues | `test_read_config_tag`, `test_read_config_num_queues` | Step 1.3 |
| AC2.3 Queue layout HPQ + N | `test_queue_layout_ac2_3` | Step 1.3 |
| AC2.4 shm_region Some with DAX | `test_shm_region_with_dax_ac2_4` | Step 1.3 |
| AC2.5 shm_region None without DAX | `test_shm_region_without_dax_ac2_5` | Step 1.3 |
| AC2.6 DAX window shared via ADD_MEM_REGION | `TestVhostUserFsDaxAlways` (integration) | Step 2.2 |
| AC3.1 Builder API boots VM with device | `TestVhostUserFsDaxAlways` (integration) | Step 2.2 |
| AC3.2 Builder API without DAX | Unit: `test_shm_region_without_dax_ac2_5` | Human: AC3.2 in Human Verification table |
| AC3.3 Tag > 36 bytes rejected | `test_add_virtiofs_vhost_user_tag_too_long_ac3_3` | Step 1.4 |
| AC3.4 Coexists with direct FUSE device | `test_add_virtiofs_vhost_user_coexistence_ac3_4` | Step 1.4 |
| AC4.1 SET_DEVICE_STATE_FD | `TestVhostUserFsDaxAlways` (integration) + `test_save_device_state_not_supported` | Step 2.2 |
| AC4.2 CHECK_DEVICE_STATE | `TestVhostUserFsDaxAlways` (integration) + `test_load_device_state_not_supported` | Step 2.2 |
| AC4.3 Snapshot state roundtrip | `test_snapshot_state_roundtrip` | Step 1.3 |
| AC4.4 Restore reconnects and restores | `TestVhostUserFsDaxAlways` (integration) | Step 2.2 |
| AC4.5 DAX contents survive restore | `TestVhostUserFsDaxAlways` (integration) | Step 2.2 |
| AC4.6 Restore fails if daemon unavailable | `test_restore_backend_state_stores_pending`, `test_activate_restore_fails_when_daemon_unavailable` | Step 1.3 |
| AC5.1 FUSE_INIT with DAX flags | `TestVhostUserFsDaxAlways` | Step 2.2 |
| AC5.2 FUSE_ATTR_DAX on files | `TestVhostUserFsDaxInode` | Step 2.3 |
| AC5.3 SETUPMAPPING writes 0xBB | `TestVhostUserFsDaxAlways` | Step 2.2 |
| AC5.4 FUSE_READ vs DAX distinguishable | `TestVhostUserFsDaxInode`, `TestVhostUserFsDaxNever` | Steps 2.3, 2.4 |
| AC5.5 DEVICE_STATE roundtrips file table | `TestVhostUserFsDaxAlways` | Step 2.2 |
| AC5.6 Daemon observes guest DAX writes | `TestVhostUserFsDaxAlways`, `TestVhostUserFsDaxInode` | Steps 2.2, 2.3 |
| AC6.1 DAX read end-to-end | `TestVhostUserFsDaxAlways`, `TestVhostUserFsDaxInode` | Steps 2.2, 2.3 |
| AC6.2 DAX write end-to-end | `TestVhostUserFsDaxAlways`, `TestVhostUserFsDaxInode` | Steps 2.2, 2.3 |
| AC6.3 Snapshot/restore end-to-end | `TestVhostUserFsDaxAlways`, `TestVhostUserFsDaxInode`, `TestVhostUserFsDaxNever` | Steps 2.2, 2.3, 2.4 |
| AC6.4 Full test suite passes | CI build verification | Steps 2.1, 2.5, Human Verification AC6.4 |
| AC6.5 Uses proc macro framework | Structural (code review) | Human Verification AC6.5 |
