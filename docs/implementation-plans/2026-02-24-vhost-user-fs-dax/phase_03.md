# VhostUserFs with DAX Implementation Plan - Phase 3

**Goal:** Share the DAX window memfd with the vhost-user daemon so it can map file content directly into the guest-accessible shared memory region.

**Architecture:** After `VhostUserDevice::activate_vhost_user()` shares guest RAM via `SET_MEM_TABLE`, the DAX window needs to be shared as an additional memory region. This requires negotiating the `CONFIGURE_MEM_SLOTS` vhost-user protocol feature, then calling `ADD_MEM_REGION` with the DAX window memfd. The daemon receives the fd and mmaps it, giving it direct write access to the DAX window.

**Tech Stack:** Rust, vhost crate v0.15 (VhostUserFrontend::add_mem_region, CONFIGURE_MEM_SLOTS protocol feature)

**Scope:** 8 phases from original design (phase 3 of 8)

**Codebase verified:** 2026-02-24

**Reference files:**
- VhostUserDevice activate: PR #527 `activate_vhost_user()` (set_mem_table pattern)
- vhost crate: `VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS`
- ShmManager: `src/vmm/src/device_manager/shm.rs`
- VirtioShmRegion: `src/devices/src/virtio/device.rs:70-75`

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-fs-dax.AC2: VhostUserFs device exposes correct virtio-fs identity
- **vhost-user-fs-dax.AC2.3 Success:** Queue layout has HPQ (queue 0) + N request queues, each with 1024 descriptors
- **vhost-user-fs-dax.AC2.4 Success:** shm_region() returns VirtioShmRegion with SHM region ID 0 when DAX configured
- **vhost-user-fs-dax.AC2.5 Success:** shm_region() returns None when dax_window_mib is None
- **vhost-user-fs-dax.AC2.6 Success:** DAX window memfd shared with daemon as additional region via ADD_MEM_REGION (CONFIGURE_MEM_SLOTS)

---

<!-- START_TASK_1 -->
### Task 1: Add CONFIGURE_MEM_SLOTS negotiation to VhostUserDevice

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/device.rs`

**Implementation:**

In `VhostUserDevice::new()`, the PR #527 code already negotiates protocol features (CONFIG, MQ). Extend the protocol feature negotiation to also request `CONFIGURE_MEM_SLOTS`:

```rust
// In the protocol feature negotiation section of new():
let desired_protocol_features = VhostUserProtocolFeatures::CONFIG
    | VhostUserProtocolFeatures::MQ
    | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS;
```

The vhost crate's `VhostUserProtocolFeatures` enum includes `CONFIGURE_MEM_SLOTS`. Negotiation succeeds if the daemon also supports it; the result is the intersection of desired and daemon features.

Store the negotiated protocol features on the `VhostUserDevice` struct for later querying:

```rust
pub struct VhostUserDevice {
    // ... existing fields ...
    acked_protocol_features: VhostUserProtocolFeatures,
}
```

Add accessor:
```rust
pub fn acked_protocol_features(&self) -> VhostUserProtocolFeatures {
    self.acked_protocol_features
}
```

**Verification:**
```bash
cargo build --features vhost-user
```
Expected: Compiles. CONFIGURE_MEM_SLOTS negotiation happens at connection time.

**Commit:** `feat(devices): negotiate CONFIGURE_MEM_SLOTS protocol feature`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add add_mem_region method to VhostUserDevice

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/device.rs`

**Implementation:**

Add a public method on `VhostUserDevice` that wraps the vhost crate's `add_mem_region()`:

```rust
/// Share an additional memory region with the daemon.
/// Requires CONFIGURE_MEM_SLOTS protocol feature to have been negotiated.
pub fn add_mem_region(&self, region_info: &VhostUserMemoryRegionInfo) -> Result<(), Error> {
    if !self.acked_protocol_features.contains(VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS) {
        return Err(Error::MissingProtocolFeature("CONFIGURE_MEM_SLOTS"));
    }
    self.frontend.lock().unwrap()
        .add_mem_region(region_info)
        .map_err(Error::VhostUser)
}
```

The `VhostUserMemoryRegionInfo` struct from the vhost crate describes a memory region with guest physical address, size, user address, and an associated fd + offset. For the DAX window, the fd is the memfd created in Phase 2.

Add the necessary error variant to the device's Error enum.

**Verification:**
```bash
cargo build --features vhost-user
```
Expected: Compiles. Method available but not yet called.

**Commit:** `feat(devices): add add_mem_region method to VhostUserDevice`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Share DAX window with daemon during VhostUserFs activation

**Verifies:** vhost-user-fs-dax.AC2.6

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/fs.rs`

**Implementation:**

Override or extend the `activate()` method in `VhostUserFs` to share the DAX window after the generic activation:

```rust
fn activate(
    &mut self,
    mem: GuestMemoryMmap,
    interrupt: InterruptTransport,
    queues: Vec<DeviceQueue>,
) -> ActivateResult {
    // 1. Delegate to generic VhostUserDevice activation
    //    This handles: set_owner, set_mem_table (RAM), set_features,
    //    vring setup, interrupt forwarding
    self.vhost_user.activate_vhost_user(mem, interrupt, queues)?;

    // 2. Share DAX window as additional memory region (if configured)
    if let (Some(fd), Some(ref region)) = (self.dax_window_fd, &self.shm_region) {
        // Note: VhostUserMemoryRegionInfo may use public fields (struct literal)
        // or a new() constructor depending on crate version. If public fields
        // are not available, use VhostUserMemoryRegionInfo::new(guest_phys_addr,
        // memory_size, userspace_addr, mmap_offset, mmap_handle) instead.
        let dax_region = VhostUserMemoryRegionInfo {
            guest_phys_addr: region.guest_addr,
            memory_size: region.size as u64,
            userspace_addr: region.host_addr,
            mmap_offset: 0,
            mmap_handle: fd,
        };
        self.vhost_user.add_mem_region(&dax_region)
            .map_err(|e| ActivateError::BadActivate)?;
    }

    Ok(())
}
```

The DAX window memfd is passed to the daemon via ADD_MEM_REGION. The daemon receives the fd and mmaps it, giving direct write access to the shared region. The guest accesses it through the GPA range allocated by ShmManager.

**Testing:**
- **vhost-user-fs-dax.AC2.6:** DAX window shared via ADD_MEM_REGION — requires a daemon to verify the region is received. This is tested end-to-end in Phase 8 integration tests. At this phase, verify operationally that activate() succeeds when called with a mock daemon that supports CONFIGURE_MEM_SLOTS.

**Verification:**
```bash
cargo build --features vhost-user
```
Expected: Compiles without errors.

**Commit:** `feat(devices): share DAX window with daemon via ADD_MEM_REGION`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Unit tests for DAX window and queue layout

**Verifies:** vhost-user-fs-dax.AC2.3, vhost-user-fs-dax.AC2.4, vhost-user-fs-dax.AC2.5

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/fs.rs` (add to existing tests module)

**Testing:**

Extend the unit tests from Phase 2 Task 3 to cover DAX and queue properties:

- **vhost-user-fs-dax.AC2.3:** Queue layout test — construct with num_request_queues=3, verify queue_config() returns 4 entries (1 HPQ + 3 request), each with max_size=1024
- **vhost-user-fs-dax.AC2.4:** shm_region with DAX — construct with dax_window_mib=Some(32), set_shm_region with guest_addr=0x1_0000_0000 and size=32*1024*1024, verify shm_region() returns the region
- **vhost-user-fs-dax.AC2.5:** shm_region without DAX — construct with dax_window_mib=None, verify shm_region() is None

These use the `new_for_test()` helper from Phase 2 Task 3.

**Verification:**
```bash
cargo test -p devices --features vhost-user
```
Expected: All tests pass.

**Commit:** `test(devices): add DAX window and queue layout unit tests`
<!-- END_TASK_4 -->
