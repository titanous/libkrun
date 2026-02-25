# VhostUserFs with DAX Implementation Plan - Phase 6

**Goal:** Full snapshot/restore support for `VhostUserFs` via the `VirtioDevice` trait's snapshot hooks.

**Architecture:** `VhostUserFs` overrides `save_backend_state()` and `restore_backend_state()` on the `VirtioDevice` trait (NOT `Snapshottable` — that trait is for legacy PortIO devices like i8042/CMOS/Serial/RTC). The `MmioTransport` wrapper implements `Snapshottable` and delegates to these `VirtioDevice` methods during its own save/restore. `save_backend_state()` captures vring bases, device config, acked features, and daemon state blob (via Phase 5's DEVICE_STATE protocol). `restore_backend_state()` deserializes and stores the saved state. `activate()` detects restore mode and performs restore-time activation: reconnect to daemon, re-negotiate with saved features, share memory + DAX window, load daemon state, and set saved vring bases. The DAX window contents are automatically captured as part of guest memory during the VMM's full memory snapshot.

**Tech Stack:** Rust, serde + bincode (behind `snapshot` feature), vhost crate v0.15

**Scope:** 8 phases from original design (phase 6 of 8)

**Codebase verified:** 2026-02-24

**Reference files:**
- VirtioDevice trait snapshot hooks: `src/devices/src/virtio/device.rs:242-250` (save_backend_state, restore_backend_state)
- VirtioDevice::post_snapshot_restore: `src/devices/src/virtio/device.rs:235`
- Block device backend state example: `src/devices/src/virtio/block/device.rs:721-729`
- Net device backend state example: `src/devices/src/virtio/net/device.rs:397-405`
- MmioTransport Snapshottable: `src/devices/src/virtio/mmio.rs:691` (delegates to VirtioDevice hooks)
- MmioTransport restore sequence: `src/devices/src/virtio/mmio.rs:760-921` (restore_state → post_snapshot_restore → complete_restore → activate)
- VhostUserDevice (Phase 1): save_device_state/load_device_state from Phase 5

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-fs-dax.AC4: Snapshot/restore preserves device and daemon state
- **vhost-user-fs-dax.AC4.3 Success:** Snapshot captures vring bases (GET_VRING_BASE), device config, and daemon state blob
- **vhost-user-fs-dax.AC4.4 Success:** Restore reconnects to daemon, re-shares memory + DAX window, restores vring bases and daemon state
- **vhost-user-fs-dax.AC4.5 Success:** DAX window contents survive snapshot/restore (see note below on cache re-population)
- **vhost-user-fs-dax.AC4.6 Failure:** Restore fails gracefully when daemon is not running at socket path

---

## Snapshot Trait Architecture

**Important:** Virtio devices do NOT implement `Snapshottable`. That trait is for legacy PortIO devices (i8042, CMOS, Serial, RTC) which are directly on the system bus. Virtio devices participate in snapshots through the `VirtioDevice` trait hooks:

- **`save_backend_state(&self) -> Option<Vec<u8>>`** — called by `MmioTransport::save_state()` at `mmio.rs:718`
- **`restore_backend_state(&mut self, &[u8])`** — called by `MmioTransport::restore_state()` at `mmio.rs:811`
- **`post_snapshot_restore(&mut self)`** — called by `MmioTransport::restore_state()` at `mmio.rs:814`

The `MmioTransport` is the `Snapshottable` wrapper. It serializes transport state (queue configs, device status, features) and delegates to the inner `VirtioDevice` for backend-specific state.

## Restore Sequence

The VMM's `MmioTransport` restore sequence (verified in `src/devices/src/virtio/mmio.rs`) is:

1. **`MmioTransport::restore_state()`** — restores transport state, then calls `device.restore_backend_state(data)` and `device.post_snapshot_restore()`
2. **`MmioTransport::complete_restore()`** — (called later) calls `device.activate(mem, interrupt, queues)`

**Critical ordering:** `restore_backend_state()` and `post_snapshot_restore()` run BEFORE `activate()`. The daemon needs memory regions (set_mem_table, add_mem_region) before it can accept state load, and those happen inside `activate()`.

**Approach:** `restore_backend_state()` deserializes and stores pending state. `activate()` detects restore mode via `pending_restore_state` and takes a different path: reconnect, use saved features, share memory, load daemon state, set saved vring bases.

---

<!-- START_SUBCOMPONENT_A (tasks 1-4) -->

<!-- START_TASK_1 -->
### Task 1: Define VhostUserFsState snapshot struct

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/fs.rs`

**Implementation:**

Define the state struct that captures everything needed for restore:

```rust
#[cfg(feature = "snapshot")]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct VhostUserFsState {
    /// Filesystem tag (for identification on restore)
    tag: String,
    /// Socket path (daemon must be running here on restore)
    socket_path: String,
    /// DAX window size in MiB (None = no DAX)
    dax_window_mib: Option<u32>,
    /// Features acked with daemon during initial negotiation
    acked_features: u64,
    /// Protocol features acked during initial negotiation
    acked_protocol_features: u64,
    /// Per-queue vring base positions (from GET_VRING_BASE)
    vring_bases: Vec<u16>,
    /// Opaque daemon state blob (from DEVICE_STATE protocol)
    daemon_state: Vec<u8>,
    /// Config space snapshot
    config_tag: [u8; 36],
    config_num_request_queues: u32,
}
```

Add `pending_restore_state` field to VhostUserFs struct:

```rust
/// Saved state from restore_backend_state(), consumed by activate() in restore mode.
#[cfg(feature = "snapshot")]
pending_restore_state: Option<VhostUserFsState>,
```

The state is serialized with bincode, matching the existing device snapshot pattern (see `block/device.rs:721-729`). Feature-gated with `#[cfg(feature = "snapshot")]`.

**Verification:**
```bash
cargo build --features vhost-user,snapshot
```

**Commit:** `feat(devices): define VhostUserFsState snapshot struct`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Implement save/restore backend state for VhostUserFs

**Verifies:** vhost-user-fs-dax.AC4.3, vhost-user-fs-dax.AC4.4, vhost-user-fs-dax.AC4.6

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/fs.rs`

**Implementation:**

Override `save_backend_state()` and `restore_backend_state()` on the existing `impl VirtioDevice for VhostUserFs` block (from Phase 2 Task 2). These are `VirtioDevice` trait methods, NOT `Snapshottable` methods:

```rust
// Inside the existing: impl VirtioDevice for VhostUserFs { ... }

#[cfg(feature = "snapshot")]
fn save_backend_state(&self) -> Option<Vec<u8>> {
    // 1. Get vring bases from daemon (saves queue positions).
    //    NOTE: GET_VRING_BASE is defined by the vhost-user spec as
    //    "sent to stop a running vring." This is the quiesce mechanism
    //    for vhost-user devices — calling it stops the daemon from
    //    processing that vring. The default no-op begin_snapshot_quiesce()
    //    inherited from VirtioDevice is correct because the actual quiesce
    //    happens here inside save_backend_state() when get_vring_base()
    //    stops each vring. Any FUSE operations in-flight at this instant
    //    are the daemon's responsibility to complete or discard.
    let mut vring_bases = Vec::new();
    let num_queues = self.queue_configs.len();
    for i in 0..num_queues {
        // get_vring_base returns VhostUserVringState; extract the base index
        // Use the accessor method (Phase 2 adds pub fn frontend() -> &Arc<Mutex<Frontend>>)
        // Note: get_vring_base() returns Result<u32> (the base index directly)
        let base = self.vhost_user.frontend().lock().unwrap()
            .get_vring_base(i)
            .map_err(|e| log::error!("get_vring_base({i}): {e}"))
            .ok()?;
        vring_bases.push(base as u16);
    }

    // 2. Save daemon internal state via DEVICE_STATE protocol
    let daemon_state = self.vhost_user.save_device_state()
        .map_err(|e| log::error!("save_device_state: {e}"))
        .ok()?;

    // 3. Build state struct
    let state = VhostUserFsState {
        tag: self.tag.clone(),
        socket_path: self.socket_path.clone(),
        dax_window_mib: self.dax_window_size.map(|s| (s / (1024 * 1024)) as u32),
        acked_features: self.vhost_user.acked_features(),
        acked_protocol_features: self.vhost_user.acked_protocol_features().bits(),
        vring_bases,
        daemon_state,
        config_tag: self.config.tag,
        config_num_request_queues: self.config.num_request_queues,
    };

    // 4. Serialize with bincode
    bincode::serialize(&state)
        .map_err(|e| log::error!("serialize VhostUserFsState: {e}"))
        .ok()
}

#[cfg(feature = "snapshot")]
fn restore_backend_state(&mut self, data: &[u8]) {
    // 1. Deserialize state
    let state: VhostUserFsState = match bincode::deserialize(data) {
        Ok(s) => s,
        Err(e) => {
            log::error!("deserialize VhostUserFsState: {e}");
            return;
        }
    };

    // 2. Restore local device fields from saved state
    self.tag = state.tag.clone();
    self.socket_path = state.socket_path.clone();
    self.config.tag = state.config_tag;
    self.config.num_request_queues = state.config_num_request_queues;

    // 3. Store state for activate() to consume in restore mode.
    //    We do NOT reconnect or load daemon state here because
    //    activate() has not run yet (restore_backend_state runs
    //    BEFORE complete_restore → activate in the VMM sequence).
    //    activate() will detect pending_restore_state and take
    //    the restore-time activation path.
    self.pending_restore_state = Some(state);
}
```

**Note on error handling:** `save_backend_state()` returns `Option<Vec<u8>>` (not `Result`), so errors are logged and `None` is returned. `restore_backend_state()` returns `()` (not `Result`), so errors are logged. This matches the existing pattern in `block/device.rs:721-729` and `net/device.rs:397-405`.

**AC coverage notes:**
- **AC4.3:** save_backend_state captures vring bases, config, daemon state blob
- **AC4.4:** restore_backend_state stores state → activate() reconnects, re-shares memory (set_mem_table + add_mem_region), loads daemon state, restores vring bases
- **AC4.5:** The DAX window is a filesystem cache backed by a separate memfd, NOT part of GuestMemoryMmap (see Phase 4 — DAX regions are excluded from GuestMemoryMmap to avoid overlapping KVM slots). After restore, the DAX window memory is zeroed. The kernel's virtio-fs driver detects SETUPMAPPING requests were lost and re-faults pages from the daemon, which re-populates the cache. File access works correctly because the daemon's internal state (restored via DEVICE_STATE) contains the authoritative data. The Phase 8 snapshot test verifies that file reads return correct data after restore, which exercises this cache re-population path
- **AC4.6:** If daemon not running, reconnection in activate()'s restore path fails, returning ActivateError

**Verification:**
```bash
cargo build --features vhost-user,snapshot
```

**Commit:** `feat(devices): implement save/restore backend state for VhostUserFs`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Add restore-mode activation path to VhostUserFs::activate()

**Verifies:** vhost-user-fs-dax.AC4.4, vhost-user-fs-dax.AC4.6

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/fs.rs`
- Modify: `src/devices/src/virtio/vhost_user/device.rs`

**Implementation:**

**Step 1: Add `queues` field and trait overrides for snapshot support**

`MmioTransport::restore_state()` calls `device.queues_mut()` to inject restored queue state (at `mmio.rs:775-785`), and `complete_restore()` reads `device.queues()` to build `DeviceQueue`s (at `mmio.rs:903-907`). The default `VirtioDevice` implementations return empty slices. Add a `queues` field and overrides:

```rust
// Add to VhostUserFs struct:
queues: Vec<Queue>,

// In VirtioDevice impl:
fn queues(&self) -> &[Queue] {
    &self.queues
}

fn queues_mut(&mut self) -> &mut [Queue] {
    &mut self.queues
}
```

Initialize `queues` in the constructor with the correct number of queues (1 HPQ + `num_request_queues`).

**Step 2: Refactor `VhostUserDevice::activate_vhost_user()` to accept optional vring bases**

The existing `activate_vhost_user()` (from Phase 2/3) handles vring setup: `set_vring_num`, `set_vring_addr`, `set_vring_base(i, 0)`, `set_vring_call`, `set_vring_kick`, `set_vring_enable`. For restore, the only difference is using saved bases instead of 0.

Add an optional parameter:

```rust
/// Activate the vhost-user device.
/// `vring_bases`: if Some, use saved vring bases (restore mode).
///                if None, use 0 (normal activation).
pub fn activate_vhost_user(
    &mut self,
    mem: GuestMemoryMmap,
    interrupt: InterruptTransport,
    device_queues: Vec<DeviceQueue>,
    vring_bases: Option<&[u16]>,
) -> ActivateResult {
    // ... existing setup (set_owner, negotiate features, set_mem_table) ...

    let frontend = self.frontend.lock().unwrap();
    for (i, dq) in device_queues.iter().enumerate() {
        let base = vring_bases
            .and_then(|bases| bases.get(i).copied())
            .unwrap_or(0);

        frontend.set_vring_num(i, dq.queue.size())
            .map_err(|_| ActivateError::BadActivate)?;
        // ... set_vring_addr using dq.queue addresses ...
        frontend.set_vring_base(i, base)
            .map_err(|_| ActivateError::BadActivate)?;
        // set_vring_call uses the interrupt eventfd from MmioTransport
        // set_vring_kick uses dq.event (the queue notification eventfd)
        frontend.set_vring_kick(i, &*dq.event)
            .map_err(|_| ActivateError::BadActivate)?;
        frontend.set_vring_enable(i, true)
            .map_err(|_| ActivateError::BadActivate)?;
    }

    Ok(())
}
```

**Note on `DeviceQueue` fields:** `DeviceQueue` has two fields: `queue: Queue` (queue config/state) and `event: Arc<EventFd>` (queue notification eventfd, used for `set_vring_kick`). The interrupt eventfd for `set_vring_call` comes from the `InterruptTransport` parameter, not from `DeviceQueue`. The existing `activate_vhost_user()` (Phase 2/3) already handles this correctly.

**Step 3: Restore-mode activation in VhostUserFs**

When `activate()` is called during restore, `pending_restore_state` is `Some(...)`. The activation path diverges:

```rust
fn activate(
    &mut self,
    mem: GuestMemoryMmap,
    interrupt: InterruptTransport,
    device_queues: Vec<DeviceQueue>,
) -> ActivateResult {
    #[cfg(feature = "snapshot")]
    if let Some(state) = self.pending_restore_state.take() {
        return self.activate_restore(mem, interrupt, device_queues, state);
    }
    // Normal activation (existing code from Phase 3)
    self.vhost_user.activate_vhost_user(mem, interrupt, device_queues, None)?;
    // ... DAX window add_mem_region ...
    Ok(())
}

#[cfg(feature = "snapshot")]
fn activate_restore(
    &mut self,
    mem: GuestMemoryMmap,
    interrupt: InterruptTransport,
    device_queues: Vec<DeviceQueue>,
    state: VhostUserFsState,
) -> ActivateResult {
    // 1. Reconnect to daemon (AC4.6: fails if daemon unavailable)
    let stream = std::os::unix::net::UnixStream::connect(&self.socket_path)
        .map_err(|e| {
            log::error!("Failed to reconnect to daemon at {}: {e}", self.socket_path);
            ActivateError::BadActivate
        })?;

    // 2. Replace Frontend, re-negotiate with saved features
    self.vhost_user.reconnect_for_restore(
        stream,
        state.acked_features,
        state.acked_protocol_features,
    )?;

    // 3. Share guest memory + set up vrings with SAVED bases
    //    Delegates to activate_vhost_user() which handles
    //    set_mem_table, set_vring_num/addr/base/kick/enable.
    self.vhost_user.activate_vhost_user(
        mem, interrupt, device_queues,
        Some(&state.vring_bases),
    )?;

    // 4. Share DAX window (add_mem_region) if configured
    //    Construct VhostUserMemoryRegionInfo matching Phase 3 Task 3 pattern.
    if let Some(ref shm_region) = self.shm_region {
        if let Some(dax_fd) = self.dax_window_fd {
            // Note: VhostUserMemoryRegionInfo may use public fields (struct literal)
            // or a new() constructor depending on crate version. If public fields
            // are not available, use VhostUserMemoryRegionInfo::new(guest_phys_addr,
            // memory_size, userspace_addr, mmap_offset, mmap_handle) instead.
            let dax_region = VhostUserMemoryRegionInfo {
                guest_phys_addr: shm_region.guest_addr,
                memory_size: shm_region.size as u64,
                userspace_addr: shm_region.host_addr,
                mmap_offset: 0,
                mmap_handle: dax_fd,
            };
            self.vhost_user.add_mem_region(&dax_region)
                .map_err(|_| ActivateError::BadActivate)?;
        }
    }

    // 5. Load daemon state via DEVICE_STATE protocol
    //    (daemon must have memory regions before it can accept state)
    self.vhost_user.load_device_state(&state.daemon_state)
        .map_err(|e| {
            log::error!("Failed to load daemon state: {e}");
            ActivateError::BadActivate
        })?;

    Ok(())
}
```

**Step 4: Add `reconnect_for_restore()` to VhostUserDevice:**
```rust
/// Replace the Frontend connection for snapshot restore.
/// Uses saved negotiated features instead of fresh negotiation.
pub fn reconnect_for_restore(
    &mut self,
    stream: UnixStream,
    saved_features: u64,
    saved_protocol_features: u64,
) -> ActivateResult {
    // Frontend::from_stream(sock: UnixStream, max_queue_num: u64)
    // max_queue_num should match the original negotiation (1 HPQ + num_request_queues).
    // Verify the exact constructor API against Phase 1's VhostUserDevice::new() —
    // it already constructs a Frontend from a socket path, replicate that pattern.
    let num_queues = self.queue_configs.len() as u64;
    let frontend = Frontend::from_stream(stream, num_queues);
    *self.frontend.lock().unwrap() = frontend;

    // Follow the full vhost-user negotiation handshake, same as VhostUserDevice::new().
    // The protocol requires get_features/get_protocol_features before set_*, even on restore.
    // Use saved features as "desired" and intersect with what the (potentially restarted)
    // daemon actually supports. This handles the case where a restarted daemon has
    // different capabilities.
    let mut frontend = self.frontend.lock().unwrap();
    frontend.set_owner().map_err(|_| ActivateError::BadActivate)?;

    // Feature negotiation: get available, intersect with saved, set
    let backend_features = frontend.get_features()
        .map_err(|_| ActivateError::BadActivate)?;
    frontend.set_features(saved_features & backend_features)
        .map_err(|_| ActivateError::BadActivate)?;

    // Protocol feature negotiation: get available, intersect with saved, set
    let backend_proto_features = frontend.get_protocol_features()
        .map_err(|_| ActivateError::BadActivate)?;
    let desired_proto = VhostUserProtocolFeatures::from_bits_truncate(saved_protocol_features);
    frontend.set_protocol_features(desired_proto & backend_proto_features)
        .map_err(|_| ActivateError::BadActivate)?;

    Ok(())
}
```

**Why this approach:**
- Reuses the existing `activate_vhost_user()` vring setup code for both normal and restore paths, avoiding duplication and incorrect field references.
- The `vring_bases` parameter cleanly controls whether bases are 0 (normal) or saved values (restore).
- `post_snapshot_restore()` stays as a no-op with its existing `()` return type — no trait changes needed.
- `queues()/queues_mut()` overrides ensure MmioTransport can inject restored queue state.

**Verification:**
```bash
cargo build --features vhost-user,snapshot
```

**Commit:** `feat(devices): add restore-mode activation path for VhostUserFs`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Snapshot/restore unit tests

**Verifies:** vhost-user-fs-dax.AC4.3, vhost-user-fs-dax.AC4.6

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/fs.rs` (add to tests module)

**Testing:**

- **vhost-user-fs-dax.AC4.3:** `test_snapshot_state_roundtrip` — construct VhostUserFsState with test data, serialize with bincode, deserialize, verify all fields match. This tests the state struct's serialization without requiring a daemon.

- **vhost-user-fs-dax.AC4.6:** `test_restore_backend_state_stores_pending` — construct VhostUserFs with `new_for_test()`, call `restore_backend_state()` with valid serialized state, verify `pending_restore_state` is `Some(...)` with correct values.

- **vhost-user-fs-dax.AC4.6:** `test_activate_restore_fails_when_daemon_unavailable` — construct VhostUserFs with `pending_restore_state` pointing to non-existent socket, call `activate()`, assert `ActivateError` is returned.

Full daemon interaction testing (save_backend_state with real daemon, load_device_state, etc.) is covered in Phase 8 integration tests.

**Verification:**
```bash
cargo test -p devices --features vhost-user,snapshot
```

**Commit:** `test(devices): add VhostUserFs snapshot/restore unit tests`
<!-- END_TASK_4 -->

<!-- END_SUBCOMPONENT_A -->
