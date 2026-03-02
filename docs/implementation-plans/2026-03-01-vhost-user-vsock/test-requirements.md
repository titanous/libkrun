# Vhost-User-VSock Test Requirements

Maps each acceptance criterion from the [design plan](../../design-plans/2026-03-01-vhost-user-vsock.md) to automated tests or documented human verification.

## Legend

| Column | Meaning |
|--------|---------|
| **AC** | Acceptance criterion ID |
| **Method** | `automated` or `human-verification` |
| **Test Type** | `unit`, `integration`, or `existing-regression` |
| **Test File** | Expected file path for the test |
| **Phase** | Implementation phase that covers this criterion |

---

## AC1: VhostUserVsock device activates and connects to backend

| AC | Description | Method | Test Type | Test File | Phase |
|----|-------------|--------|-----------|-----------|-------|
| AC1.1 | Socket path connection: VhostUserVsock connects to backend via socket path, negotiates features, exposes correct guest_cid | automated | integration | `tests/test_cases/src/test_vhost_user_vsock.rs` (`TestVhostUserVsockEcho`) | Phase 6 |
| AC1.2 | Pre-provisioned fd connection: VhostUserVsock connects via pre-provisioned fd (UnixStream), same feature negotiation and config behavior | automated | integration | `tests/test_cases/src/test_vhost_user_vsock.rs` (`TestVhostUserVsockFd`) | Phase 6 |
| AC1.3 | Device type 19, 3 queues: Device reports VIRTIO_ID_VSOCK (19) and configures 3 queues (RX, TX, Event) | automated | unit | `src/devices/src/virtio/vhost_user/vsock.rs` (`test_device_type_is_vsock`, `test_queue_layout`) | Phase 2 |
| AC1.4 | Non-existent socket path returns error | automated | unit | `src/devices/src/virtio/vhost_user/vsock.rs` (`test_new_fails_nonexistent_socket`) | Phase 2 |
| AC1.5 | Incompatible backend feature negotiation fails | human-verification | -- | -- | Phase 2 (note below) |

**AC1.5 note:** Handled by the generic `VhostUserDevice::negotiate_and_build()` layer which checks protocol features during construction and returns an error if required features are missing. Phase 2 explicitly notes "No vsock-specific test needed; the generic layer tests cover this."

**Human verification procedure for AC1.5:**
1. Start a vhost-user backend that does not advertise `VHOST_USER_F_PROTOCOL_FEATURES`.
2. Attempt to construct `VhostUserVsock::new()` pointing at that backend.
3. Confirm the constructor returns an `Err`.

---

## AC2: API enforces mutual exclusivity with userspace vsock

| AC | Description | Method | Test Type | Test File | Phase |
|----|-------------|--------|-----------|-----------|-------|
| AC2.1 | `add_vsock_vhost_user()` success: configures VM when no userspace vsock is configured | automated | unit | `src/libkrun/src/lib.rs` (mutual exclusivity tests) | Phase 3 |
| AC2.2 | `add_vsock_vhost_user_fd()` success: configures VM with pre-provisioned fd | automated | unit | `src/libkrun/src/lib.rs` (mutual exclusivity tests) | Phase 3 |
| AC2.3 | `krun_add_vsock` + `add_vsock_vhost_user` = error: calling both returns error | automated | unit | `src/libkrun/src/lib.rs` (mutual exclusivity tests) | Phase 3 |
| AC2.4 | `add_vsock_vhost_user` + `krun_add_vsock` = error: calling both in either order returns error | automated | unit | `src/libkrun/src/lib.rs` (mutual exclusivity tests) | Phase 3 |

**Note on AC2.1 and AC2.2:** The Builder's `new()` requires firmware (libkrunfw) not available in unit test binaries, so the success-path tests may verify by directly manipulating `ContextConfig` fields. Full success-path integration testing is deferred to Phase 6 (`TestVhostUserVsockEcho` for AC2.1, `TestVhostUserVsockFd` for AC2.2).

---

## AC3: Guest can communicate with backend over vsock

| AC | Description | Method | Test Type | Test File | Phase |
|----|-------------|--------|-----------|-----------|-------|
| AC3.1 | Guest echo communication: guest connects via AF_VSOCK, sends data, receives response | automated | integration | `tests/test_cases/src/test_vhost_user_vsock.rs` (`TestVhostUserVsockEcho`) | Phase 6 |
| AC3.2 | Multiple concurrent connections: multiple simultaneous vsock connections work | automated | integration | `tests/test_cases/src/test_vhost_user_vsock.rs` (`TestVhostUserVsockEcho`) | Phase 6 |

---

## AC4: Snapshot and restore preserves state

| AC | Description | Method | Test Type | Test File | Phase |
|----|-------------|--------|-----------|-----------|-------|
| AC4.1 | State save: vring bases + DEVICE_STATE saved | automated | unit | `src/devices/src/virtio/vhost_user/vsock.rs` (`test_snapshot_state_roundtrip`, `test_restore_backend_state_stores_pending`) | Phase 4 |
| AC4.2 | Restore reconnects to fresh backend: restored VM reconnects via new provisioned fd, resumes vsock | automated | integration | `tests/test_cases/src/test_vhost_user_vsock.rs` (`TestVhostUserVsockSnapshot`) | Phase 6 |
| AC4.3 | Backend counter continuity: post-restore counter continues from pre-snapshot value | automated | integration | `tests/test_cases/src/test_vhost_user_vsock.rs` (`TestVhostUserVsockSnapshot`) | Phase 6 |
| AC4.4 | Restore fails if backend unavailable | automated | unit | `src/devices/src/virtio/vhost_user/vsock.rs` (`test_activate_restore_fails_with_dummy_backend`) | Phase 4 |

---

## AC5: Backward compatibility

| AC | Description | Method | Test Type | Test File | Phase |
|----|-------------|--------|-----------|-----------|-------|
| AC5.1 | Existing userspace vsock works unchanged | automated | existing-regression | `tests/test_cases/src/test_vsock_guest_connect.rs` (`vsock-guest-connect`) | Phase 6 (verified by existing tests passing) |
| AC5.2 | `krun_add_vsock_port` preserved and functional with userspace vsock | automated | existing-regression | `tests/test_cases/src/test_tsi_tcp_guest_connect.rs` (`tsi-tcp-guest-connect`) | Phase 6 (verified by existing tests passing) |
| AC5.3 | Guest kernel unchanged: same TSI patches work with both backends | automated | integration | `tests/test_cases/src/test_vhost_user_vsock.rs` (all tests use unmodified libkrunfw) | Phase 6 |

**AC5 note:** No new test code is needed for backward compatibility. The implementation adds a new code path without modifying the existing userspace vsock path. AC5.1 and AC5.2 are verified by confirming that existing integration tests continue to pass. AC5.3 is implicitly verified by all vhost-user-vsock integration tests succeeding with the stock libkrunfw kernel image.

---

## Test Infrastructure (not directly testing acceptance criteria)

| Component | Purpose | File | Phase |
|-----------|---------|------|-------|
| `VhostUserDevice::from_stream()` | Enables fd-provisioned connections (prerequisite for AC1.2) | `src/devices/src/virtio/vhost_user/device.rs` | Phase 1 |
| `test-vsock-proxy` binary | Echo backend for integration tests (prerequisite for AC1, AC3, AC4) | `tests/test_vsock_proxy/src/main.rs` | Phase 5 |
| Counter query port (9998) | Enables counter verification for AC4.3 | `tests/test_vsock_proxy/src/main.rs` | Phase 6 (Task 2) |
| `socket_path` field in `VhostUserVsockState` | Enables restore-time reconnection for AC4.2 | `src/devices/src/virtio/vhost_user/vsock.rs` | Phase 6 (Task 1) |

---

## Summary

| AC | Total Criteria | Automated | Human Verification |
|----|---------------|-----------|-------------------|
| AC1 | 5 | 4 | 1 (AC1.5) |
| AC2 | 4 | 4 | 0 |
| AC3 | 2 | 2 | 0 |
| AC4 | 4 | 4 | 0 |
| AC5 | 3 | 3 | 0 |
| **Total** | **18** | **17** | **1** |

## Test Execution Commands

```bash
# Unit tests (Phase 2, Phase 4)
cargo test -p devices --features vhost-user -- vhost_user::vsock
cargo test -p devices --features vhost-user,snapshot -- vhost_user::vsock

# API mutual exclusivity unit tests (Phase 3)
cargo test -p libkrun --features vhost-user -- vsock

# Integration tests (Phase 6) - require embedded_init and libkrunfw
make test FEATURE_FLAGS="--features embedded_init" -- --test-case vhost-user-vsock-echo
make test FEATURE_FLAGS="--features embedded_init" -- --test-case vhost-user-vsock-fd
make test FEATURE_FLAGS="--features embedded_init" -- --test-case vhost-user-vsock-snapshot

# Backward compatibility regression (Phase 6 verification)
make test FEATURE_FLAGS="--features embedded_init" -- --test-case vsock-guest-connect
```
