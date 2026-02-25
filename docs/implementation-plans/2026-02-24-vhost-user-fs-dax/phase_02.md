# VhostUserFs with DAX Implementation Plan - Phase 2

**Goal:** Create `VhostUserFs` struct that wraps the generic `VhostUserDevice` with filesystem-specific specialization: device type 26, config space from daemon, HPQ + N request queues, and DAX window allocation.

**Architecture:** `VhostUserFs` composes `VhostUserDevice` (from Phase 1) and overrides FS-specific `VirtioDevice` trait methods. Config space is fetched from the daemon via `VHOST_USER_GET_CONFIG` on construction and cached locally. The DAX window is a memfd of configurable size, exposed via `shm_region()` as a `VirtioShmRegion` with SHM region ID 0.

**Tech Stack:** Rust, vhost crate v0.15 (GET_CONFIG support via VhostUserFrontend trait)

**Scope:** 8 phases from original design (phase 2 of 8)

**Codebase verified:** 2026-02-24

**Reference files:**
- Existing Fs device: `src/devices/src/virtio/fs/device.rs` (VirtioDevice impl pattern)
- VirtioDevice trait: `src/devices/src/virtio/device.rs:83-256`
- VirtioShmRegion: `src/devices/src/virtio/device.rs:70-75`
- Fs config space: `src/devices/src/virtio/fs/device.rs:25-40` (VirtioFsConfig)
- Fs queue defs: `src/devices/src/virtio/fs/mod.rs:30-45` (QUEUE_SIZE=1024, 2 queues)
- ShmManager: `src/vmm/src/device_manager/shm.rs:19-91`
- CLAUDE.md files: root, `src/vmm/CLAUDE.md`, `src/libkrun/CLAUDE.md`, `tests/CLAUDE.md`

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-fs-dax.AC1: VhostUserDevice generic wrapper works
- **vhost-user-fs-dax.AC1.1 Success:** VhostUserDevice connects to a vhost-user daemon over a Unix socket and completes feature negotiation
- **vhost-user-fs-dax.AC1.2 Success:** VhostUserDevice shares memfd-backed guest memory with daemon via SET_MEM_TABLE
- **vhost-user-fs-dax.AC1.3 Failure:** VhostUserDevice returns error when daemon socket is unavailable

### vhost-user-fs-dax.AC2: VhostUserFs device exposes correct virtio-fs identity
- **vhost-user-fs-dax.AC2.1 Success:** device_type() returns 26 (VIRTIO_ID_FS)
- **vhost-user-fs-dax.AC2.2 Success:** Config space contains filesystem tag and num_request_queues from daemon (via VHOST_USER_GET_CONFIG)

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->

<!-- START_TASK_1 -->
### Task 1: Create VhostUserFs struct and VhostUserFsConfig

**Files:**
- Create: `src/devices/src/virtio/vhost_user/fs.rs`
- Modify: `src/devices/src/virtio/vhost_user/mod.rs` (add `pub mod fs; pub use fs::VhostUserFs;`)
- Create: `src/vmm/src/vmm_config/vhost_user_fs.rs`
- Modify: `src/vmm/src/vmm_config/mod.rs` (add `#[cfg(feature = "vhost-user")] pub mod vhost_user_fs;`)

**Implementation:**

Create `VhostUserFs` in `fs.rs`:

```rust
#[cfg(feature = "vhost-user")]
use std::os::unix::io::RawFd;
use vm_memory::ByteValued;

use crate::virtio::device::{DeviceState, VirtioDevice, VirtioShmRegion};
use crate::virtio::queue::QueueConfig;
use super::VhostUserDevice;

const VIRTIO_ID_FS: u32 = 26;
const QUEUE_SIZE: u16 = 1024;

/// Config space layout for virtio-fs (matches kernel's virtio_fs_config).
#[repr(C, packed)]
#[derive(Copy, Clone, Default)]
struct VirtioFsConfig {
    tag: [u8; 36],
    num_request_queues: u32,
}

// SAFETY: VirtioFsConfig is repr(C, packed) with only primitive fields
unsafe impl ByteValued for VirtioFsConfig {}

pub struct VhostUserFs {
    vhost_user: VhostUserDevice,
    config: VirtioFsConfig,
    queue_configs: Vec<QueueConfig>,
    shm_region: Option<VirtioShmRegion>,
    dax_window_size: Option<usize>,
    dax_window_fd: Option<RawFd>,
    tag: String,
    socket_path: String,
}
```

Create `VhostUserFsConfig` in `src/vmm/src/vmm_config/vhost_user_fs.rs`:

```rust
#[derive(Clone, Debug)]
pub struct VhostUserFsConfig {
    pub tag: String,
    pub socket_path: String,
    pub dax_window_mib: Option<u32>,
}
```

**Note on config types:** PR #527 introduces a generic `VhostUserDeviceConfig` in `src/vmm/src/resources.rs` (device_type + socket_path + queue config). `VhostUserFsConfig` is the FS-specific replacement — it contains the filesystem tag and DAX window size instead of generic device_type/queue fields. The generic `VhostUserDeviceConfig` is retained by Phase 1 but not used for FS devices; it may be useful for future vhost-user device types.

**Verification:**
```bash
cargo build --features vhost-user
```
Expected: Compiles (struct not yet used).

**Commit:** `feat(devices): add VhostUserFs struct and VhostUserFsConfig`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: VhostUserFs constructor and VirtioDevice trait implementation

**Verifies:** vhost-user-fs-dax.AC1.1, vhost-user-fs-dax.AC1.2, vhost-user-fs-dax.AC2.1, vhost-user-fs-dax.AC2.2

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/fs.rs`

**Implementation:**

Add `VhostUserFs::new()` constructor:

1. Accept `tag: &str`, `socket_path: &str`, `dax_window_mib: Option<u32>`
2. Validate tag length <= 36 bytes, return error if exceeds
3. Create inner `VhostUserDevice::new(socket_path, VIRTIO_ID_FS, "virtio-fs-vhost", 0, &[])` — pass `num_queues=0` to auto-detect from daemon via `get_queue_num()`, empty queue_sizes to use daemon defaults. **Note:** Verify `VhostUserDevice::new()` signature from PR #527. If it does not support auto-detection from 0, explicitly pass `1 + config.num_request_queues` after fetching config via `get_config()`
4. Fetch config from daemon: call `frontend.get_config(0, std::mem::size_of::<VirtioFsConfig>() as u32, ...)` via the inner VhostUserDevice's frontend. Parse the response bytes into a `VirtioFsConfig`.
5. Build queue configs: 1 HPQ (index 0, size QUEUE_SIZE=1024) + `config.num_request_queues` request queues (each size 1024)
6. If `dax_window_mib` is Some: create memfd via `memfd_create("vhost-fs-dax", MFD_CLOEXEC)`, ftruncate to `mib * 1024 * 1024`, store the fd
7. Copy tag bytes into config.tag field

Implement `VirtioDevice` trait for `VhostUserFs`:

- `avail_features()` → delegate to `self.vhost_user.avail_features()`
- `acked_features()` → delegate to `self.vhost_user.acked_features()`
- `set_acked_features(features)` → delegate to `self.vhost_user.set_acked_features(features)`
- `device_type()` → return `VIRTIO_ID_FS` (26)
- `device_name()` → return `"virtio-fs-vhost"`
- `queue_config()` → return `&self.queue_configs`
- `read_config(offset, data)` → copy bytes from `self.config` at offset into data buffer (follow pattern in `src/devices/src/virtio/fs/device.rs:139-151`)
- `write_config(offset, data)` → log warning, no-op (guest shouldn't write config)
- `activate(mem, interrupt, queues)` → delegate to `self.vhost_user.activate_vhost_user(mem, interrupt, queues)`. The generic VhostUserDevice handles SET_MEM_TABLE, vring setup, and interrupt forwarding.
- `is_activated()` → delegate to `self.vhost_user.is_activated()`
- `reset()` → delegate to `self.vhost_user.reset()`
- `shm_region()` → return `self.shm_region.as_ref()`

Add accessor methods:
- `pub fn set_shm_region(&mut self, region: VirtioShmRegion)` → sets `self.shm_region = Some(region)`
- `pub fn dax_window_fd(&self) -> Option<RawFd>` → returns the memfd fd for sharing with daemon
- `pub fn dax_window_size(&self) -> Option<usize>` → returns the size
- `pub fn socket_path(&self) -> &str`
- `pub fn tag(&self) -> &str`

The inner `VhostUserDevice` handles the vhost-user protocol (AC1.1 socket connection + feature negotiation, AC1.2 memory sharing via set_mem_table). `VhostUserFs` adds FS-specific identity (AC2.1 device_type=26, AC2.2 config space from daemon).

**Constructor architecture (decided):** `VhostUserFs::new()` constructs `VhostUserDevice::new()` first (which connects to the daemon and negotiates features), then accesses the inner `Frontend` via the `Arc<Mutex<Frontend>>` field to call `get_config()`. This works because `VhostUserDevice::new()` leaves the Frontend in a fully negotiated state but does NOT call `activate_vhost_user()` — activation happens later when the VMM calls `activate()`. The sequence is:

1. `VhostUserDevice::new(socket_path, ...)` — connects, negotiates features, stores Frontend
2. `vhost_user.frontend.lock().unwrap().get_config(0, size, flags, &mut buf)` — fetch FS config
3. Parse `VirtioFsConfig` from response bytes
4. Build queue configs from `config.num_request_queues`

This requires `VhostUserDevice` to expose its `frontend` field (or add a `get_config()` forwarding method). If the field is not public, add: `pub fn frontend(&self) -> &Arc<Mutex<Frontend>>` to VhostUserDevice.

**Verification:**
```bash
cargo build --features vhost-user
```
Expected: Compiles without errors.

**Commit:** `feat(devices): implement VhostUserFs constructor and VirtioDevice trait`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Unit tests for VhostUserFs device identity

**Verifies:** vhost-user-fs-dax.AC1.3, vhost-user-fs-dax.AC2.1, vhost-user-fs-dax.AC2.2

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/fs.rs` (add `#[cfg(test)] mod tests`)

**Testing:**

Unit tests that verify device identity properties without requiring a daemon connection. These test the static properties of a constructed VhostUserFs.

Since VhostUserFs::new() requires a daemon (socket connection), tests that verify construction-time behavior (AC1.1, AC1.2) are deferred to Phase 8 integration tests. Unit tests here verify:

- **vhost-user-fs-dax.AC2.1:** `device_type()` returns 26 — test by constructing a VhostUserFs with mocked/minimal state (bypass the socket connection for unit testing) and asserting `device_type() == 26`
- **vhost-user-fs-dax.AC2.2:** Config space read returns correct tag and num_request_queues — test by constructing with known config values and reading back via `read_config()`

**Approach for unit tests without a daemon:**

Create a test helper `VhostUserFs::new_for_test(config: VirtioFsConfig, dax_window_mib: Option<u32>)` behind `#[cfg(test)]` that constructs the struct without a socket connection. This allows testing device identity, config space, queue layout, and shm_region without a vhost-user daemon.

Test cases:
1. `test_device_type_is_fs` — assert `device_type() == 26`
2. `test_read_config_tag` — set tag "testfs", read_config at offset 0, verify tag bytes
3. `test_read_config_num_queues` — set num_request_queues=2, read_config at offset 36, verify u32 LE bytes
4. `test_queue_config_hpq_plus_request_queues` — verify queue_configs length is 1 + num_request_queues, all sizes 1024
5. `test_shm_region_none_without_dax` — construct with dax_window_mib=None, assert shm_region() is None
6. `test_shm_region_some_with_dax` — construct with dax_window_mib=Some(32), call set_shm_region with a test region, assert shm_region() returns it
7. `test_new_fails_with_unavailable_socket` — **(AC1.3)** call `VhostUserFs::new("tag", "/tmp/nonexistent-socket-path-12345", None)`, assert it returns an error (not panic). This verifies the failure path for unavailable daemon sockets without requiring a running daemon.

**Verification:**
```bash
cargo test -p devices --features vhost-user
```
Expected: All unit tests pass.

**Commit:** `test(devices): add VhostUserFs device identity unit tests`
<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->
