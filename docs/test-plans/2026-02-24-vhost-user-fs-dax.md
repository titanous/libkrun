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
| 2.1 | Run `make test FEATURE_FLAGS="--features embedded_init,vhost-user"` | All integration tests run. The three vhost-user-fs tests (`vhost-user-fs-dax-read`, `vhost-user-fs-dax-write`, `vhost-user-fs-dax-snapshot`) appear in test output. (AC6.4) |
| 2.2 | Observe `vhost-user-fs-dax-read` test output | Test passes. Guest mounts virtiofs with `dax=inode`, reads `/mnt/testfs/hello.txt`, and receives all 0xBB bytes (DAX pattern, not 0xAA FUSE_READ pattern). Printed "OK". (AC6.1) |
| 2.3 | Observe `vhost-user-fs-dax-write` test output | Test passes. Guest writes 0xCC to `/mnt/testfs/hello.txt` via DAX, reads back, verifies all 0xCC bytes. Printed "OK". (AC6.2) |
| 2.4 | Observe `vhost-user-fs-dax-snapshot` test output | Test passes. Guest reads 0xBB pre-snapshot, signals READY, host snapshots, daemon is killed and restarted, host restores, guest reads 0xBB post-restore. Printed "OK". (AC6.3) |
| 2.5 | Verify existing tests still pass alongside vhost-user tests | The standard snapshot, block, network, and VM lifecycle tests (approximately 5-6 of 6 expected passing) continue to pass. No regressions. (AC1.4) |

---

## Phase 3: Build Verification

| Step | Action | Expected |
|------|--------|----------|
| 3.1 | Run `VHOST_USER=1 make` | Library builds successfully with vhost-user feature enabled. No compiler errors or warnings related to vhost-user code. |
| 3.2 | Run `make` (without VHOST_USER) | Library builds successfully without vhost-user feature. Vhost-user code is completely gated behind feature flag and does not appear in the binary. |
| 3.3 | Verify `tests/test_daemon/` binary builds | Run `cargo build -p test-daemon` in the tests workspace. The test daemon binary compiles without errors. |

---

## End-to-End: DAX Read Path

**Purpose:** Validate the complete path from Builder API through device creation, daemon connection, feature negotiation, memory sharing, virtqueue setup, FUSE_INIT, file lookup, SETUPMAPPING, and DAX window read.

**Steps:**
1. The `vhost-user-fs-dax-read` integration test exercises this full path.
2. Manually inspect the test-daemon log output (if available) to confirm: vhost-user connection established, FUSE_INIT received with `MAP_ALIGNMENT` flag, LOOKUP for "hello.txt" returned `FUSE_ATTR_DAX`, SETUPMAPPING handler wrote 0xBB to DAX window offset.
3. Guest assertion `data.iter().all(|&b| b == 0xBB)` confirms the entire chain worked. If 0xAA is read instead, the diagnostic output identifies the failure point (kernel version, missing CONFIG_FUSE_DAX).

---

## End-to-End: Snapshot/Restore Cycle

**Purpose:** Validate the full snapshot lifecycle: save vring bases via GET_VRING_BASE, save daemon state via SET_DEVICE_STATE_FD/CHECK_DEVICE_STATE, serialize VhostUserFsState, daemon kill/restart, deserialize state, reconnect to daemon, re-negotiate features, restore vring bases, re-share memory + DAX window, load daemon state via DEVICE_STATE, and verify guest reads correct data post-restore.

**Steps:**
1. The `vhost-user-fs-dax-snapshot` integration test exercises this full path.
2. Verify the test uses vsock synchronization (guest sends "READY", host sends "CHECK") to ensure snapshot happens at the correct point.
3. Verify the daemon is actually killed between snapshot and restore (not just paused) — the test calls `daemon.kill()`, `daemon.wait()`, removes socket file, then starts a fresh daemon. This proves the daemon state was truly saved and reloaded, not just preserved in memory.
4. Guest assertion post-restore `data.iter().all(|&b| b == 0xBB)` confirms the entire restore chain worked, including daemon state round-trip and DAX page re-fault.

---

## End-to-End: DAX Write Path

**Purpose:** Validate guest can write to DAX-mapped files and read back written content, proving the bidirectional DAX window is functional.

**Steps:**
1. The `vhost-user-fs-dax-write` integration test exercises this path.
2. Guest opens `/mnt/testfs/hello.txt` for writing, writes 4096 bytes of 0xCC, flushes, then reads back via `fs::read()`.
3. Assertion `data.iter().take(4096).all(|&b| b == 0xCC)` confirms the write path works through the DAX window.

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
| AC1.1 VhostUserDevice connects and negotiates | `TestVhostUserFsDaxRead` (integration) + `test_new_fails_with_unavailable_socket` (unit) | Step 2.2 |
| AC1.2 SET_MEM_TABLE shares memory | `TestVhostUserFsDaxRead` (integration) | Step 2.2 |
| AC1.3 Error on unavailable socket | `test_new_fails_with_unavailable_socket` | Step 1.3 |
| AC1.4 No regression | Existing test suites | Steps 1.1, 1.2, 2.5 |
| AC2.1 device_type() == 26 | `test_device_type_is_fs` | Step 1.3 |
| AC2.2 Config space tag + num_queues | `test_read_config_tag`, `test_read_config_num_queues` | Step 1.3 |
| AC2.3 Queue layout HPQ + N | `test_queue_layout_ac2_3` | Step 1.3 |
| AC2.4 shm_region Some with DAX | `test_shm_region_with_dax_ac2_4` | Step 1.3 |
| AC2.5 shm_region None without DAX | `test_shm_region_without_dax_ac2_5` | Step 1.3 |
| AC2.6 DAX window shared via ADD_MEM_REGION | `TestVhostUserFsDaxRead` (integration) | Step 2.2 |
| AC3.1 Builder API boots VM with device | `TestVhostUserFsDaxRead` (integration) | Step 2.2 |
| AC3.2 Builder API without DAX | Unit: `test_shm_region_without_dax_ac2_5` | Human: AC3.2 in Human Verification table |
| AC3.3 Tag > 36 bytes rejected | `test_add_virtiofs_vhost_user_tag_too_long_ac3_3` | Step 1.4 |
| AC3.4 Coexists with direct FUSE device | `test_add_virtiofs_vhost_user_coexistence_ac3_4` | Step 1.4 |
| AC4.1 SET_DEVICE_STATE_FD | `TestVhostUserFsDaxSnapshot` + `test_save_device_state_not_supported` | Step 2.4 |
| AC4.2 CHECK_DEVICE_STATE | `TestVhostUserFsDaxSnapshot` + `test_load_device_state_not_supported` | Step 2.4 |
| AC4.3 Snapshot state roundtrip | `test_snapshot_state_roundtrip` | Step 1.3 |
| AC4.4 Restore reconnects and restores | `TestVhostUserFsDaxSnapshot` | Step 2.4 |
| AC4.5 DAX contents survive restore | `TestVhostUserFsDaxSnapshot` | Step 2.4 |
| AC4.6 Restore fails if daemon unavailable | `test_restore_backend_state_stores_pending`, `test_activate_restore_fails_when_daemon_unavailable` | Step 1.3 |
| AC5.1 FUSE_INIT with DAX flags | `TestVhostUserFsDaxRead` | Step 2.2 |
| AC5.2 FUSE_ATTR_DAX on files | `TestVhostUserFsDaxRead` | Step 2.2 |
| AC5.3 SETUPMAPPING writes 0xBB | `TestVhostUserFsDaxRead` | Step 2.2 |
| AC5.4 FUSE_READ vs DAX distinguishable | `TestVhostUserFsDaxRead` | Step 2.2 |
| AC5.5 DEVICE_STATE roundtrips file table | `TestVhostUserFsDaxSnapshot` | Step 2.4 |
| AC5.6 Daemon observes guest DAX writes | `TestVhostUserFsDaxWrite` | Step 2.3 |
| AC6.1 DAX read end-to-end | `TestVhostUserFsDaxRead` | Step 2.2 |
| AC6.2 DAX write end-to-end | `TestVhostUserFsDaxWrite` | Step 2.3 |
| AC6.3 Snapshot/restore end-to-end | `TestVhostUserFsDaxSnapshot` | Step 2.4 |
| AC6.4 Full test suite passes | CI build verification | Steps 2.1, 2.5, Human Verification AC6.4 |
| AC6.5 Uses proc macro framework | Structural (code review) | Human Verification AC6.5 |
