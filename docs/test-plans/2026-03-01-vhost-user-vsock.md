# Human Test Plan: Vhost-User-VSock

Generated from implementation plan: `docs/implementation-plans/2026-03-01-vhost-user-vsock/`

## Prerequisites

- NixOS dev shell active (or equivalent build environment)
- libkrunfw available at runtime (symlinked to test-prefix/lib64)
- All unit tests passing:
  ```bash
  cargo test -p devices --features vhost-user -- vhost_user::vsock
  cargo test -p devices --features vhost-user,snapshot -- vhost_user::vsock
  cargo test -p libkrun --features vhost-user -- vsock
  ```
- All integration tests passing:
  ```bash
  make test FEATURE_FLAGS="--features embedded_init"
  ```

## Phase 1: Unit-Level Behavior Verification

| Step | Action | Expected |
|------|--------|----------|
| 1.1 | Run `cargo test -p devices --features vhost-user -- vhost_user::vsock` | All tests pass: `test_device_type_is_vsock`, `test_queue_layout`, `test_read_config_guest_cid_full`, `test_read_config_guest_cid_split`, `test_read_config_beyond_size`, `test_new_fails_nonexistent_socket`, `test_device_name`, `test_guest_cid_accessor` |
| 1.2 | Run `cargo test -p devices --features vhost-user,snapshot -- vhost_user::vsock` | All snapshot tests pass additionally: `test_snapshot_state_roundtrip`, `test_restore_backend_state_stores_pending`, `test_activate_restore_fails_with_dummy_backend` |
| 1.3 | Run `cargo test -p libkrun --features vhost-user -- vsock` | All mutual exclusivity tests pass: `test_add_vsock_vhost_user_socket_path_success`, `test_add_vsock_vhost_user_fd_success`, `test_vsock_conflict_explicit_then_vhost_user`, `test_vsock_conflict_vhost_user_then_explicit`, `test_vsock_conflict_vhost_user_fd_then_explicit` |

## Phase 2: Integration Test Execution

| Step | Action | Expected |
|------|--------|----------|
| 2.1 | Run `make test FEATURE_FLAGS="--features embedded_init" -- --test-case vhost-user-vsock-echo` | Test starts proxy at tmp socket, launches VM with `add_vsock_vhost_user()`, guest echoes `b"hello"` and two concurrent connections. Console output contains "OK". Exit code 0. |
| 2.2 | Run `make test FEATURE_FLAGS="--features embedded_init" -- --test-case vhost-user-vsock-fd` | Test starts proxy, connects `UnixStream`, passes to `add_vsock_vhost_user_fd()`, guest echoes `b"hello"`. Console output contains "OK". Exit code 0. |
| 2.3 | Run `make test FEATURE_FLAGS="--features embedded_init" -- --test-case vhost-user-vsock-snapshot` | Test starts proxy, guest echoes 100 bytes, queries counter (100), host snapshots, kills/restarts proxy, restores, guest echoes 50 more bytes, queries counter (150). Console output contains "OK". Exit code 0. |
| 2.4 | Run `make test FEATURE_FLAGS="--features embedded_init" -- --test-case vsock-guest-connect` | Existing userspace vsock test passes unchanged: guest/host ping-pong exchange works. Console output contains "OK". |

## Phase 3: Backward Compatibility Verification

| Step | Action | Expected |
|------|--------|----------|
| 3.1 | Run full test suite: `make test FEATURE_FLAGS="--features embedded_init"` | All pre-existing tests continue to pass (5-6/6 expected due to known flakiness in vsock/TSI timing tests). No regressions introduced. |
| 3.2 | Verify `test_tsi_tcp_guest_connect` passes | TSI TCP test using `krun_add_vsock_port` C API succeeds, confirming port-config API is preserved |
| 3.3 | Inspect build output for `VHOST_USER=1 make` | Build completes without errors when vhost-user feature is enabled. No new warnings related to vsock code. |

## End-to-End: Full Vsock Lifecycle (Socket Path)

**Purpose:** Validates the complete lifecycle from API configuration through guest communication, covering AC1.1, AC2.1, AC3.1, AC3.2.

**Steps:**
1. Build the test-vsock-proxy: `cargo build -p test-vsock-proxy` (in tests workspace)
2. Start proxy manually: `./target/debug/test-vsock-proxy --socket-path /tmp/test-vsock.sock --guest-cid 3`
3. Verify socket file `/tmp/test-vsock.sock` appears within 5 seconds
4. Run `vhost-user-vsock-echo` integration test
5. Verify test output shows "OK" -- confirms guest connected to proxy via AF_VSOCK, sent echo data, received response, and multiple concurrent connections worked
6. Kill proxy process. Verify it exits cleanly.

## End-to-End: Snapshot/Restore with State Transfer

**Purpose:** Validates the complete snapshot/restore lifecycle including DEVICE_STATE protocol, covering AC4.1, AC4.2, AC4.3.

**Steps:**
1. Run `vhost-user-vsock-snapshot` integration test
2. Observe test log output (set `RUST_LOG=debug`): confirm "save_device_state" and "load_device_state" messages appear in proxy logs
3. Verify the proxy was killed and restarted (socket file is removed and recreated in test output)
4. Verify guest output shows counter continuing from pre-snapshot value (100 -> 150 after 50 additional echo bytes)
5. Result confirms: vring bases saved, DEVICE_STATE transferred via pipe to new proxy, counter state preserved across proxy restart

## End-to-End: Fd-Provisioned Connection

**Purpose:** Validates the pre-provisioned fd path for orchestrator-managed connections, covering AC1.2, AC2.2.

**Steps:**
1. Run `vhost-user-vsock-fd` integration test
2. Verify test output shows "OK"
3. Confirm the host test code at `tests/test_cases/src/test_vhost_user_vsock.rs` shows `UnixStream::connect()` followed by `builder.add_vsock_vhost_user_fd(stream)` -- no socket path is passed to the device
4. Guest-side echo works identically to socket path variant

## Human Verification Required

| Criterion | Why Manual | Steps |
|-----------|------------|-------|
| AC1.5 - Incompatible backend feature negotiation fails | Requires a custom backend that does not advertise `VHOST_USER_F_PROTOCOL_FEATURES`. The test-vsock-proxy always advertises this feature. | 1. Modify `tests/test_vsock_proxy/src/main.rs` line 134 to return `VhostUserProtocolFeatures::empty()` instead of the full set. 2. Rebuild: `cargo build -p test-vsock-proxy`. 3. Start modified proxy: `./target/debug/test-vsock-proxy --socket-path /tmp/bad-vsock.sock --guest-cid 3`. 4. Attempt `VhostUserVsock::new("/tmp/bad-vsock.sock")` in a test harness. 5. Confirm the constructor returns `Err`. 6. Revert the modification. |

## Traceability

| Acceptance Criterion | Automated Test | Manual Step |
|----------------------|----------------|-------------|
| AC1.1 - Socket path connection | `test_vhost_user_vsock.rs` / `TestVhostUserVsockEcho` | Step 2.1, E2E Lifecycle |
| AC1.2 - Pre-provisioned fd | `test_vhost_user_vsock.rs` / `TestVhostUserVsockFd` | Step 2.2, E2E Fd-Provisioned |
| AC1.3 - Device type 19, 3 queues | `vsock.rs` / `test_device_type_is_vsock`, `test_queue_layout` | Step 1.1 |
| AC1.4 - Non-existent socket error | `vsock.rs` / `test_new_fails_nonexistent_socket` | Step 1.1 |
| AC1.5 - Incompatible features fail | -- (human-verification) | Human Verification table above |
| AC2.1 - `add_vsock_vhost_user` success | `lib.rs` / `test_add_vsock_vhost_user_socket_path_success` | Step 1.3 |
| AC2.2 - `add_vsock_vhost_user_fd` success | `lib.rs` / `test_add_vsock_vhost_user_fd_success` | Step 1.3 |
| AC2.3 - explicit + vhost-user = error | `lib.rs` / `test_vsock_conflict_explicit_then_vhost_user` | Step 1.3 |
| AC2.4 - vhost-user + explicit = error | `lib.rs` / `test_vsock_conflict_vhost_user_then_explicit`, `test_vsock_conflict_vhost_user_fd_then_explicit` | Step 1.3 |
| AC3.1 - Guest echo communication | `test_vhost_user_vsock.rs` / `TestVhostUserVsockEcho` guest | Step 2.1 |
| AC3.2 - Multiple concurrent connections | `test_vhost_user_vsock.rs` / `TestVhostUserVsockEcho` guest | Step 2.1 |
| AC4.1 - State save roundtrip | `vsock.rs` / `test_snapshot_state_roundtrip`, `test_restore_backend_state_stores_pending` | Step 1.2 |
| AC4.2 - Restore reconnects to fresh backend | `test_vhost_user_vsock.rs` / `TestVhostUserVsockSnapshot` | Step 2.3, E2E Snapshot/Restore |
| AC4.3 - Backend counter continuity | `test_vhost_user_vsock.rs` / `TestVhostUserVsockSnapshot` | Step 2.3, E2E Snapshot/Restore |
| AC4.4 - Restore fails if backend unavailable | `vsock.rs` / `test_activate_restore_fails_with_dummy_backend` | Step 1.2 |
| AC5.1 - Existing userspace vsock unchanged | `test_vsock_guest_connect.rs` (existing-regression) | Step 2.4, Step 3.1 |
| AC5.2 - `krun_add_vsock_port` preserved | `test_tsi_tcp_guest_connect.rs` (existing-regression) | Step 3.2 |
| AC5.3 - Guest kernel unchanged | All vhost-user-vsock integration tests (stock kernel) | Steps 2.1-2.3 |
