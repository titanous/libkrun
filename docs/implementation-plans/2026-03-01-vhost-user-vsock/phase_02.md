# Vhost-User-VSock Implementation Plan - Phase 2

**Goal:** Implement the vsock-specific vhost-user device wrapper, following the VhostUserFs pattern.

**Architecture:** `VhostUserVsock` wraps `VhostUserDevice` via composition, adding vsock-specific config space (guest_cid), 3-queue layout (RX, TX, Event), and device type 19. Both `new()` (socket path) and `from_stream()` (pre-connected UnixStream) constructors are provided, sharing a common `build_from_device()` helper. Config struct `VhostUserVsockConfig` holds the connection method for use by the builder in Phase 3.

**Tech Stack:** Rust, vendored vhost crate, `vm-memory::ByteValued`

**Scope:** 2 of 6 phases from original design

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-vsock.AC1: VhostUserVsock device activates and connects to backend
- **vhost-user-vsock.AC1.3 Success:** Device reports VIRTIO_ID_VSOCK (19) as device type and configures 3 queues (RX, TX, Event)
- **vhost-user-vsock.AC1.4 Failure:** Connection to non-existent socket path returns error
- **vhost-user-vsock.AC1.5 Failure:** Backend that doesn't support required protocol features returns error during negotiation — **Note:** This is handled by the generic `VhostUserDevice::negotiate_and_build()` layer which checks protocol features during construction and returns an error if required features are missing. No vsock-specific test needed; the generic layer tests cover this.

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->
<!-- START_TASK_1 -->
### Task 1: Create VhostUserVsock device wrapper and VhostUserVsockConfig

**Files:**
- Create: `src/devices/src/virtio/vhost_user/vsock.rs`
- Modify: `src/devices/src/virtio/vhost_user/mod.rs` (add module declaration and re-export)
- Create: `src/vmm/src/vmm_config/vhost_user_vsock.rs`
- Modify: `src/vmm/src/vmm_config/mod.rs` (add module declaration)

**Implementation:**

**`src/devices/src/virtio/vhost_user/vsock.rs`** — new file:

```rust
// Copyright 2026, Red Hat Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! VhostUserVsock device: vhost-user vsock backend.
//!
//! Wraps VhostUserDevice with vsock-specific config space (guest_cid),
//! 3-queue layout (RX, TX, Event), and device type 19.

use std::io::{self, Result as IoResult};
use std::os::unix::net::UnixStream;

use log::{debug, warn};
use vhost::vhost_user::message::VhostUserConfigFlags;
use vhost::vhost_user::VhostUserFrontend;

use crate::virtio::device::VirtioDevice;
use crate::virtio::QueueConfig;
use crate::virtio::{ActivateError, ActivateResult, Queue};

use super::VhostUserDevice;

const VIRTIO_ID_VSOCK: u32 = 19;
const NUM_QUEUES: usize = 3; // RX, TX, Event
const QUEUE_SIZE: u16 = 256;

#[derive(Debug)]
pub struct VhostUserVsock {
    vhost_user: VhostUserDevice,
    guest_cid: u64,
    queue_configs: Vec<QueueConfig>,
    /// Queue state buffer for snapshot support
    queues: Vec<Queue>,
    #[cfg(feature = "snapshot")]
    pending_restore_state: Option<VhostUserVsockState>,
}

/// Snapshot state for VhostUserVsock device.
#[cfg(feature = "snapshot")]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct VhostUserVsockState {
    pub guest_cid: u64,
    pub acked_features: u64,
    pub acked_protocol_features: u64,
    pub vring_bases: Vec<u16>,
    pub daemon_state: Vec<u8>,
}

impl VhostUserVsock {
    /// Create a new VhostUserVsock by connecting to a backend at socket_path.
    pub fn new(socket_path: &str) -> IoResult<Self> {
        debug!("Creating vhost-user-vsock via socket path: {}", socket_path);
        let vhost_user = VhostUserDevice::new(
            socket_path,
            VIRTIO_ID_VSOCK,
            "virtio-vsock-vhost".to_string(),
            NUM_QUEUES as u16,
            &[QUEUE_SIZE; NUM_QUEUES],
        )?;
        Self::build_from_device(vhost_user)
    }

    /// Create a new VhostUserVsock from a pre-connected UnixStream.
    pub fn from_stream(stream: UnixStream) -> IoResult<Self> {
        debug!("Creating vhost-user-vsock from pre-connected stream");
        let vhost_user = VhostUserDevice::from_stream(
            stream,
            VIRTIO_ID_VSOCK,
            "virtio-vsock-vhost".to_string(),
            NUM_QUEUES as u16,
            &[QUEUE_SIZE; NUM_QUEUES],
        )?;
        Self::build_from_device(vhost_user)
    }

    /// Shared construction: fetch guest_cid from backend config, build struct.
    fn build_from_device(vhost_user: VhostUserDevice) -> IoResult<Self> {
        // Fetch guest_cid from backend via GET_CONFIG
        let guest_cid = {
            let mut frontend = vhost_user.frontend.lock().unwrap();
            let config_buf = [0u8; 8]; // virtio_vsock_config is 8 bytes (u64 guest_cid)
            let (_hdr, payload) = frontend
                .get_config(
                    0,
                    8,
                    VhostUserConfigFlags::empty(),
                    &config_buf,
                )
                .map_err(|e| io::Error::other(format!("get_config failed: {}", e)))?;
            if payload.len() < 8 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "vsock config too short: expected 8 bytes, got {}",
                        payload.len()
                    ),
                ));
            }
            u64::from_le_bytes(payload[..8].try_into().unwrap())
        };

        debug!("vhost-user-vsock: guest_cid = {}", guest_cid);

        let queue_configs = vec![QueueConfig::new(QUEUE_SIZE); NUM_QUEUES];
        let queues = (0..NUM_QUEUES).map(|_| Queue::new(QUEUE_SIZE)).collect();

        Ok(VhostUserVsock {
            vhost_user,
            guest_cid,
            queue_configs,
            queues,
            #[cfg(feature = "snapshot")]
            pending_restore_state: None,
        })
    }

    /// Get the guest CID.
    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }
}

impl VirtioDevice for VhostUserVsock {
    fn avail_features(&self) -> u64 {
        self.vhost_user.avail_features()
    }

    fn acked_features(&self) -> u64 {
        self.vhost_user.acked_features()
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.vhost_user.set_acked_features(acked_features);
    }

    fn device_type(&self) -> u32 {
        VIRTIO_ID_VSOCK
    }

    fn device_name(&self) -> &str {
        "virtio-vsock-vhost"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &self.queue_configs
    }

    fn queues(&self) -> &[Queue] {
        &self.queues
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // Config space is virtio_vsock_config { guest_cid: u64 } (8 bytes, LE)
        let cid_bytes = self.guest_cid.to_le_bytes();
        let config_len = cid_bytes.len() as u64;
        if offset >= config_len {
            warn!(
                "VhostUserVsock: config read at offset {} beyond config size {}",
                offset, config_len
            );
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            let end = std::cmp::min(end, config_len);
            let len = (end - offset) as usize;
            data[..len].copy_from_slice(&cid_bytes[offset as usize..end as usize]);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "VhostUserVsock: guest attempted config write (offset={:x}, len={})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: vm_memory::GuestMemoryMmap,
        interrupt: crate::virtio::InterruptTransport,
        queues: Vec<crate::virtio::DeviceQueue>,
    ) -> ActivateResult {
        #[cfg(feature = "snapshot")]
        if let Some(state) = self.pending_restore_state.take() {
            return self.activate_restore(mem, interrupt, queues, state);
        }

        // Copy queue state for snapshot (MmioTransport reads device.queues())
        for (i, dq) in queues.iter().enumerate() {
            if let Some(q) = self.queues.get_mut(i) {
                q.size = dq.queue.size;
                q.ready = dq.queue.ready;
                q.desc_table = dq.queue.desc_table;
                q.avail_ring = dq.queue.avail_ring;
                q.used_ring = dq.queue.used_ring;
                q.set_next_avail(dq.queue.next_avail().0);
                q.set_next_used(dq.queue.next_used().0);
            }
        }

        // Delegate to generic VhostUserDevice activation
        self.vhost_user.activate(mem, interrupt, queues)
    }

    fn is_activated(&self) -> bool {
        self.vhost_user.is_activated()
    }

    fn reset(&mut self) -> bool {
        self.vhost_user.reset()
    }
}
```

Note: the `#[cfg(feature = "snapshot")]` block in `activate()` references `activate_restore` which will be implemented in Phase 4. For now, include the conditional block — it compiles because the `pending_restore_state` field and `take()` only exist when `snapshot` feature is enabled, and `activate_restore` will be added in Phase 4. If this causes a compilation issue, wrap the entire block in `#[cfg(feature = "snapshot")]` and add a stub:

```rust
#[cfg(feature = "snapshot")]
fn activate_restore(
    &mut self,
    _mem: vm_memory::GuestMemoryMmap,
    _interrupt: crate::virtio::InterruptTransport,
    _queues: Vec<crate::virtio::DeviceQueue>,
    _state: VhostUserVsockState,
) -> ActivateResult {
    Err(ActivateError::BadActivate) // Stub until Phase 4
}
```

**`src/devices/src/virtio/vhost_user/mod.rs`** — add vsock module:

Add after `pub mod fs;`:
```rust
pub mod vsock;
```

Add after `pub use fs::VhostUserFs;`:
```rust
pub use vsock::VhostUserVsock;
```

**`src/vmm/src/vmm_config/vhost_user_vsock.rs`** — new file:

```rust
// Copyright 2026, Red Hat Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::net::UnixStream;

/// Connection method for vhost-user-vsock backend.
pub enum VhostUserVsockConnection {
    /// Connect via Unix domain socket path.
    SocketPath(String),
    /// Use a pre-connected UnixStream (fd-provisioned by orchestrator).
    Stream(UnixStream),
}

/// Configuration for a vhost-user-vsock device.
pub struct VhostUserVsockConfig {
    pub connection: VhostUserVsockConnection,
}

impl std::fmt::Debug for VhostUserVsockConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.connection {
            VhostUserVsockConnection::SocketPath(path) => {
                f.debug_struct("VhostUserVsockConfig")
                    .field("connection", &format!("SocketPath({})", path))
                    .finish()
            }
            VhostUserVsockConnection::Stream(_) => {
                f.debug_struct("VhostUserVsockConfig")
                    .field("connection", &"Stream(<fd>)")
                    .finish()
            }
        }
    }
}
```

**`src/vmm/src/vmm_config/mod.rs`** — add module declaration. Add alongside the existing `vhost_user_fs` entry, gated on the same feature:

```rust
#[cfg(feature = "vhost-user")]
pub mod vhost_user_vsock;
```

**Verification:**

```bash
cd /home/titanous/vm-platform/libkrun/.worktrees/vhost-user-vsock && cargo check -p devices --features vhost-user
cargo check -p vmm --features vhost-user
```
Expected: compiles without errors.

**Commit:** `feat(vhost-user): add VhostUserVsock device wrapper`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Unit tests for VhostUserVsock

**Verifies:** vhost-user-vsock.AC1.3, vhost-user-vsock.AC1.4

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/vsock.rs` (add test module and test constructor)

**Implementation:**

Add a test constructor and test module at the end of `vsock.rs`:

```rust
impl VhostUserVsock {
    /// Constructor for unit tests that bypasses socket connection.
    #[cfg(test)]
    fn new_for_test(guest_cid: u64) -> Self {
        VhostUserVsock {
            vhost_user: VhostUserDevice::new_for_test_unconnected(),
            guest_cid,
            queue_configs: vec![QueueConfig::new(QUEUE_SIZE); NUM_QUEUES],
            queues: (0..NUM_QUEUES).map(|_| Queue::new(QUEUE_SIZE)).collect(),
            #[cfg(feature = "snapshot")]
            pending_restore_state: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC1.3: Device type is VIRTIO_ID_VSOCK (19)
    #[test]
    fn test_device_type_is_vsock() {
        let device = VhostUserVsock::new_for_test(3);
        assert_eq!(device.device_type(), 19);
    }

    // AC1.3: 3 queues (RX, TX, Event), each size 256
    #[test]
    fn test_queue_layout() {
        let device = VhostUserVsock::new_for_test(3);
        let queues = device.queue_config();
        assert_eq!(queues.len(), 3, "expected 3 queues (RX, TX, Event)");
        for (i, qc) in queues.iter().enumerate() {
            assert_eq!(qc.size, 256, "queue {} should have size 256", i);
        }
    }

    // Config space: guest_cid as u64 LE (full 8-byte read)
    #[test]
    fn test_read_config_guest_cid_full() {
        let device = VhostUserVsock::new_for_test(42);
        let mut buf = [0u8; 8];
        device.read_config(0, &mut buf);
        assert_eq!(u64::from_le_bytes(buf), 42);
    }

    // Config space: split read (low 4 bytes, high 4 bytes)
    #[test]
    fn test_read_config_guest_cid_split() {
        let cid: u64 = 0x0000_0001_0000_002A; // high=1, low=42
        let device = VhostUserVsock::new_for_test(cid);

        let mut lo = [0u8; 4];
        device.read_config(0, &mut lo);
        assert_eq!(u32::from_le_bytes(lo), 42);

        let mut hi = [0u8; 4];
        device.read_config(4, &mut hi);
        assert_eq!(u32::from_le_bytes(hi), 1);
    }

    // Config space: read beyond config size is a no-op
    #[test]
    fn test_read_config_beyond_size() {
        let device = VhostUserVsock::new_for_test(42);
        let mut buf = [0xFF; 4];
        device.read_config(8, &mut buf); // offset 8 is beyond 8-byte config
        assert_eq!(buf, [0xFF; 4]); // buffer unchanged
    }

    // AC1.4: Connection to non-existent socket path returns error
    #[test]
    fn test_new_fails_nonexistent_socket() {
        let result = VhostUserVsock::new("/tmp/nonexistent-vhost-vsock-12345.sock");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ));
    }

    #[test]
    fn test_device_name() {
        let device = VhostUserVsock::new_for_test(3);
        assert_eq!(device.device_name(), "virtio-vsock-vhost");
    }

    #[test]
    fn test_guest_cid_accessor() {
        let device = VhostUserVsock::new_for_test(99);
        assert_eq!(device.guest_cid(), 99);
    }
}
```

**Testing:**

```bash
cargo test -p devices --features vhost-user -- vhost_user::vsock
```
Expected: all tests pass.

**Commit:** `test(vhost-user): add unit tests for VhostUserVsock`
<!-- END_TASK_2 -->
<!-- END_SUBCOMPONENT_A -->
