# Vhost-User-VSock Implementation Plan - Phase 4

**Goal:** Implement save/restore for vhost-user-vsock following the vhost-user-fs snapshot pattern.

**Architecture:** Follows the VhostUserFs snapshot pattern exactly: `save_backend_state()` stops vrings via `get_vring_base()`, saves daemon state via DEVICE_STATE protocol, serializes `VhostUserVsockState` with bincode. `restore_backend_state()` deserializes state, marks device inactive, stores pending state. `activate_restore()` (called during re-activation) reconnects to a fresh backend via a new provisioned fd, re-negotiates features, sets up vrings with saved bases, and loads daemon state. Since VhostUserVsock has no DAX window, the restore path is simpler than VhostUserFs.

**Tech Stack:** Rust, bincode (serialization), vendored vhost crate (DEVICE_STATE protocol)

**Scope:** 4 of 6 phases from original design

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-vsock.AC4: Snapshot and restore preserves state
- **vhost-user-vsock.AC4.1 Success:** VhostUserVsock state is saved (vring bases + backend state blob via DEVICE_STATE)
- **vhost-user-vsock.AC4.4 Failure:** Restore fails cleanly if backend is unavailable at restore time

Note: AC4.2 and AC4.3 (full restore round-trip and backend state continuity) require a real backend and are verified in Phase 6 integration tests.

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->
<!-- START_TASK_1 -->
### Task 1: Implement save_backend_state, restore_backend_state, and activate_restore

**Verifies:** vhost-user-vsock.AC4.1, vhost-user-vsock.AC4.4

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/vsock.rs` (add snapshot methods and activate_restore)

**Implementation:**

The `VhostUserVsockState` struct was already defined in Phase 2 (behind `#[cfg(feature = "snapshot")]`). Now add the snapshot trait methods and the activate_restore logic.

Add these imports at the top of vsock.rs (the snapshot-specific ones):
```rust
#[cfg(feature = "snapshot")]
use vhost::VhostBackend;
```

Add `save_backend_state` and `restore_backend_state` to the `VirtioDevice` impl for `VhostUserVsock` (these override the default no-op implementations):

```rust
#[cfg(feature = "snapshot")]
fn save_backend_state(&self) -> Option<Vec<u8>> {
    // 1. Get vring bases from daemon (this stops each vring — the quiesce mechanism)
    let mut vring_bases = Vec::new();
    let num_queues = self.queue_configs.len();
    for i in 0..num_queues {
        match self.vhost_user.frontend.lock().unwrap().get_vring_base(i) {
            Ok(base) => {
                vring_bases.push(base as u16);
            }
            Err(e) => {
                log::error!("get_vring_base({i}) failed: {e}");
                return None;
            }
        }
    }

    // 2. Save daemon internal state via DEVICE_STATE protocol
    let daemon_state = match self.vhost_user.save_device_state() {
        Ok(state) => state,
        Err(e) => {
            log::error!("save_device_state failed: {e}");
            return None;
        }
    };

    // 3. Build state struct
    let state = VhostUserVsockState {
        guest_cid: self.guest_cid,
        acked_features: self.vhost_user.acked_features(),
        acked_protocol_features: self.vhost_user.acked_protocol_features().bits(),
        vring_bases,
        daemon_state,
    };

    // 4. Serialize with bincode
    bincode::serialize(&state)
        .map_err(|e| log::error!("serialize VhostUserVsockState: {e}"))
        .ok()
}

#[cfg(feature = "snapshot")]
fn restore_backend_state(&mut self, data: &[u8]) {
    // 1. Deserialize state
    let state: VhostUserVsockState = match bincode::deserialize(data) {
        Ok(s) => s,
        Err(e) => {
            log::error!("deserialize VhostUserVsockState: {e}");
            return;
        }
    };

    // 2. Restore local fields
    self.guest_cid = state.guest_cid;

    // 3. Mark as inactive so complete_restore() → activate() runs
    self.vhost_user.mark_inactive();

    // 4. Store state for activate() to consume in restore mode
    self.pending_restore_state = Some(state);
}
```

Replace the stub `activate_restore` (from Phase 2) with the full implementation. Add to `impl VhostUserVsock` (in a `#[cfg(feature = "snapshot")]` block):

```rust
#[cfg(feature = "snapshot")]
impl VhostUserVsock {
    /// Activate device in restore mode using previously saved state.
    /// Called by activate() when pending_restore_state is Some.
    ///
    /// Phase 6 will add reconnection logic here (reconnect_for_restore via
    /// socket_path, matching the VhostUserFs pattern). For now, this handles
    /// the activate/load sequence assuming the connection is already established.
    fn activate_restore(
        &mut self,
        mem: vm_memory::GuestMemoryMmap,
        interrupt: crate::virtio::InterruptTransport,
        queues: Vec<crate::virtio::DeviceQueue>,
        state: VhostUserVsockState,
    ) -> ActivateResult {
        // TODO(Phase 6): Add reconnect_for_restore() call here using
        // state.socket_path to reconnect to a fresh backend process.

        // 1. Share guest memory + set up vrings with SAVED bases
        self.vhost_user
            .activate_vhost_user(&mem, &interrupt, &queues, Some(&state.vring_bases))
            .map_err(|_| ActivateError::BadActivate)?;

        // 2. Load daemon state via DEVICE_STATE protocol
        if self
            .vhost_user
            .load_device_state(&state.daemon_state)
            .is_err()
        {
            self.vhost_user.reset();
            return Err(ActivateError::BadActivate);
        }

        // 3. Mark as activated
        self.vhost_user.mark_activated(mem, interrupt);

        Ok(())
    }
}
```

**Verification:**

```bash
cd /home/titanous/vm-platform/libkrun/.worktrees/vhost-user-vsock && cargo check -p devices --features vhost-user,snapshot
```
Expected: compiles without errors.

**Commit:** `feat(vhost-user): add snapshot/restore for VhostUserVsock`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Unit tests for snapshot state serialization

**Verifies:** vhost-user-vsock.AC4.1, vhost-user-vsock.AC4.4

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/vsock.rs` (add snapshot tests to existing test module)

**Testing:**

Add these tests to the existing `#[cfg(test)] mod tests` block in vsock.rs:

Tests must verify:
- AC4.1: VhostUserVsockState serializes and deserializes correctly (round-trip)
- AC4.1: restore_backend_state stores pending state and updates guest_cid
- AC4.4: activate with pending state fails when daemon is unavailable (the test device has a dummy frontend that can't perform vhost-user operations)

```rust
// AC4.1: Snapshot state roundtrip
#[test]
#[cfg(feature = "snapshot")]
fn test_snapshot_state_roundtrip() {
    let state = VhostUserVsockState {
        guest_cid: 42,
        acked_features: 0x1234_5678,
        acked_protocol_features: 0x9abc_def0,
        vring_bases: vec![5, 10, 15],
        daemon_state: vec![1, 2, 3, 4, 5],
    };

    let serialized = bincode::serialize(&state).expect("serialize failed");
    let deserialized: VhostUserVsockState =
        bincode::deserialize(&serialized).expect("deserialize failed");

    assert_eq!(deserialized.guest_cid, 42);
    assert_eq!(deserialized.acked_features, 0x1234_5678);
    assert_eq!(deserialized.acked_protocol_features, 0x9abc_def0);
    assert_eq!(deserialized.vring_bases, vec![5, 10, 15]);
    assert_eq!(deserialized.daemon_state, vec![1, 2, 3, 4, 5]);
}

// AC4.1: restore_backend_state stores pending state
#[test]
#[cfg(feature = "snapshot")]
fn test_restore_backend_state_stores_pending() {
    let mut device = VhostUserVsock::new_for_test(3);

    let state = VhostUserVsockState {
        guest_cid: 99,
        acked_features: 0xdead,
        acked_protocol_features: 0xbeef,
        vring_bases: vec![1, 2, 3],
        daemon_state: vec![10, 20],
    };

    let serialized = bincode::serialize(&state).expect("serialize");
    device.restore_backend_state(&serialized);

    assert!(device.pending_restore_state.is_some());
    let restored = device.pending_restore_state.as_ref().unwrap();
    assert_eq!(restored.guest_cid, 99);
    assert_eq!(restored.vring_bases, vec![1, 2, 3]);
    // guest_cid on device should also be updated
    assert_eq!(device.guest_cid(), 99);
}

// AC4.4: activate with pending restore fails when backend is unavailable
// (test device has a dummy frontend that can't do vhost-user operations)
#[test]
#[cfg(feature = "snapshot")]
fn test_activate_restore_fails_with_dummy_backend() {
    use crate::legacy::DummyIrqChip;
    use crate::virtio::DeviceQueue;
    use std::sync::Arc;
    use utils::eventfd::EventFd;

    let mut device = VhostUserVsock::new_for_test(3);

    let state = VhostUserVsockState {
        guest_cid: 3,
        acked_features: 0,
        acked_protocol_features: 0,
        vring_bases: vec![0, 0, 0],
        daemon_state: vec![],
    };
    device.pending_restore_state = Some(state);

    let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(
        vm_memory::GuestAddress(0),
        1024 * 1024,
    )])
    .expect("create guest memory");

    let queues: Vec<DeviceQueue> = (0..3)
        .map(|_| {
            DeviceQueue::new(
                crate::virtio::Queue::new(256),
                Arc::new(EventFd::new(0).expect("eventfd")),
            )
        })
        .collect();

    let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
    let interrupt = crate::virtio::InterruptTransport::new(irqchip, "test-vsock".into())
        .expect("create interrupt");

    let result = device.activate(mem, interrupt, queues);
    assert!(
        result.is_err(),
        "activate_restore should fail with dummy backend"
    );
}
```

**Verification:**

```bash
cargo test -p devices --features vhost-user,snapshot -- vhost_user::vsock
```
Expected: all tests pass.

**Commit:** `test(vhost-user): add snapshot unit tests for VhostUserVsock`
<!-- END_TASK_2 -->
<!-- END_SUBCOMPONENT_A -->
