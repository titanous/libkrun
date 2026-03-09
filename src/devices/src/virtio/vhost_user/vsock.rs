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
use crate::virtio::{ActivateResult, Queue};

use super::VhostUserDevice;

#[cfg(feature = "snapshot")]
use crate::snapshot_serde;
#[cfg(feature = "snapshot")]
use vhost::VhostBackend;

const VIRTIO_ID_VSOCK: u32 = 19;
const NUM_QUEUES: usize = 3; // RX, TX, Event
const QUEUE_SIZE: u16 = 256;
const MAX_SNAPSHOT_BYTES: usize = 8192;

#[derive(Debug)]
pub struct VhostUserVsock {
    vhost_user: VhostUserDevice,
    guest_cid: u64,
    // Only read in snapshot restore paths (feature-gated by snapshot); set unconditionally in new().
    #[allow(dead_code)]
    socket_path: Option<String>,
    queue_configs: Vec<QueueConfig>,
    /// Queue state buffer for snapshot support
    queues: Vec<Queue>,
    #[cfg(feature = "snapshot")]
    pending_restore_state: Option<VhostUserVsockState>,
}

/// Snapshot state for VhostUserVsock device.
#[cfg(feature = "snapshot")]
#[derive(Clone, Debug, bincode_next::Encode, bincode_next::Decode)]
pub(crate) struct VhostUserVsockState {
    pub guest_cid: u64,
    pub acked_features: u64,
    pub acked_protocol_features: u64,
    pub vring_bases: Vec<u16>,
    pub daemon_state: Vec<u8>,
    pub socket_path: Option<String>,
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
        Self::build_from_device(vhost_user, Some(socket_path.to_string()))
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
        Self::build_from_device(vhost_user, None)
    }

    /// Shared construction: fetch guest_cid from backend config, build struct.
    fn build_from_device(
        vhost_user: VhostUserDevice,
        socket_path: Option<String>,
    ) -> IoResult<Self> {
        // Fetch guest_cid from backend via GET_CONFIG
        let guest_cid = {
            let mut frontend = vhost_user.frontend.lock().unwrap();
            let config_buf = [0u8; 8]; // virtio_vsock_config is 8 bytes (u64 guest_cid)
            let (_hdr, payload) = frontend
                .get_config(0, 8, VhostUserConfigFlags::empty(), &config_buf)
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
            socket_path,
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
        if offset >= 8 {
            warn!(
                "VhostUserVsock: config read at offset {} beyond config size 8",
                offset
            );
        }
        read_vsock_config(self.guest_cid, offset, data);
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
            socket_path: self.socket_path.clone(),
        };

        // 4. Serialize with bincode-next
        snapshot_serde::serialize(&state)
            .map_err(|e| log::error!("serialize VhostUserVsockState: {e}"))
            .ok()
    }

    #[cfg(feature = "snapshot")]
    fn restore_backend_state(&mut self, data: &[u8]) {
        // 1. Deserialize state
        let state: VhostUserVsockState = match snapshot_serde::deserialize::<
            VhostUserVsockState,
            { MAX_SNAPSHOT_BYTES },
        >(data)
        {
            Ok(s) => s,
            Err(e) => {
                log::error!("deserialize VhostUserVsockState: {e}");
                return;
            }
        };

        // 2. Restore local fields
        self.guest_cid = state.guest_cid;
        self.socket_path = state.socket_path.clone();

        // 3. Mark as inactive so complete_restore() → activate() runs
        self.vhost_user.mark_inactive();

        // 4. Store state for activate() to consume in restore mode
        self.pending_restore_state = Some(state);
    }
}

#[cfg(feature = "snapshot")]
impl VhostUserVsock {
    /// Activate device in restore mode using previously saved state.
    /// Called by activate() when pending_restore_state is Some.
    fn activate_restore(
        &mut self,
        mem: vm_memory::GuestMemoryMmap,
        interrupt: crate::virtio::InterruptTransport,
        queues: Vec<crate::virtio::DeviceQueue>,
        state: VhostUserVsockState,
    ) -> ActivateResult {
        use crate::virtio::ActivateError;

        // Reconnect to fresh backend at the saved socket path.
        // For fd-based devices (socket_path is None), the orchestrator must
        // provide a new connection before restore — not yet supported.
        if let Some(ref path) = state.socket_path {
            let stream = std::os::unix::net::UnixStream::connect(path)
                .map_err(|_| ActivateError::BadActivate)?;
            self.vhost_user
                .reconnect_for_restore(stream, state.acked_features, state.acked_protocol_features)
                .map_err(|_| ActivateError::BadActivate)?;
        }

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

/// Pure config-space read logic for vsock (8-byte guest_cid LE).
/// Extracted for Kani verification — production `read_config` delegates here.
fn read_vsock_config(guest_cid: u64, offset: u64, data: &mut [u8]) {
    data.fill(0);
    let cid_bytes = guest_cid.to_le_bytes();
    let config_len = cid_bytes.len() as u64;
    if offset >= config_len {
        return;
    }
    if let Some(end) = offset.checked_add(data.len() as u64) {
        let end = std::cmp::min(end, config_len);
        let len = (end - offset) as usize;
        data[..len].copy_from_slice(&cid_bytes[offset as usize..end as usize]);
    }
}

impl VhostUserVsock {
    /// Constructor for unit tests that bypasses socket connection.
    #[cfg(test)]
    fn new_for_test(guest_cid: u64) -> Self {
        VhostUserVsock {
            vhost_user: VhostUserDevice::new_for_test_unconnected(),
            guest_cid,
            socket_path: None,
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

    // Config space: read beyond config size zeroes buffer
    #[test]
    fn test_read_config_beyond_size() {
        let device = VhostUserVsock::new_for_test(42);
        let mut buf = [0xFF; 4];
        device.read_config(8, &mut buf); // offset 8 is beyond 8-byte config
        assert_eq!(buf, [0u8; 4]); // buffer zeroed
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
            socket_path: Some("/tmp/test.sock".to_string()),
        };

        let serialized = snapshot_serde::serialize(&state).expect("serialize failed");
        let deserialized: VhostUserVsockState =
            snapshot_serde::deserialize::<VhostUserVsockState, { MAX_SNAPSHOT_BYTES }>(&serialized)
                .expect("deserialize failed");

        assert_eq!(deserialized.guest_cid, 42);
        assert_eq!(deserialized.acked_features, 0x1234_5678);
        assert_eq!(deserialized.acked_protocol_features, 0x9abc_def0);
        assert_eq!(deserialized.vring_bases, vec![5, 10, 15]);
        assert_eq!(deserialized.daemon_state, vec![1, 2, 3, 4, 5]);
        assert_eq!(deserialized.socket_path, Some("/tmp/test.sock".to_string()));
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
            socket_path: Some("/tmp/restore.sock".to_string()),
        };

        let serialized = snapshot_serde::serialize(&state).expect("serialize");
        device.restore_backend_state(&serialized);

        assert!(device.pending_restore_state.is_some());
        let restored = device.pending_restore_state.as_ref().unwrap();
        assert_eq!(restored.guest_cid, 99);
        assert_eq!(restored.vring_bases, vec![1, 2, 3]);
        assert_eq!(restored.socket_path, Some("/tmp/restore.sock".to_string()));
        // guest_cid on device should also be updated
        assert_eq!(device.guest_cid(), 99);
        // socket_path on device should also be updated
        assert_eq!(device.socket_path, Some("/tmp/restore.sock".to_string()));
    }

    // Info leak: partial read must zero untouched tail bytes
    #[test]
    fn test_read_config_partial_zeroes_tail() {
        let device = VhostUserVsock::new_for_test(42);
        let mut buf = [0xFFu8; 8];
        device.read_config(6, &mut buf); // offset 6, 8-byte buf → only 2 config bytes fit
                                         // bytes 0..2 should be config[6..8], bytes 2..8 should be zero
        let cid_bytes = 42u64.to_le_bytes();
        assert_eq!(buf[..2], cid_bytes[6..8]);
        assert_eq!(buf[2..], [0u8; 6], "tail bytes must be zero, not stale");
    }

    // Kani-style exhaustive test: all (offset, bufsize) combos yield no stale data
    #[test]
    fn test_read_config_no_stale_data_exhaustive() {
        let cid: u64 = 0x0102_0304_0506_0708;
        let device = VhostUserVsock::new_for_test(cid);
        let cid_bytes = cid.to_le_bytes();

        // Sweep all offsets 0..=16 and buffer sizes 1..=16
        for offset in 0..=16u64 {
            for bufsize in 1..=16usize {
                let mut buf = [0xFFu8; 16];
                let data = &mut buf[..bufsize];
                device.read_config(offset, data);

                for (i, &byte) in data.iter().enumerate() {
                    let config_idx = offset as usize + i;
                    if config_idx < 8 {
                        assert_eq!(
                            byte, cid_bytes[config_idx],
                            "offset={offset}, buf[{i}]: expected config byte"
                        );
                    } else {
                        assert_eq!(
                            byte, 0,
                            "offset={offset}, buf[{i}]: expected zero, got {byte:#x} (stale data)"
                        );
                    }
                }
            }
        }
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
            socket_path: None,
        };
        device.pending_restore_state = Some(state);

        let mem =
            vm_memory::GuestMemoryMmap::from_ranges(&[(vm_memory::GuestAddress(0), 1024 * 1024)])
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
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// After read_vsock_config, every byte is either a valid config byte or zero.
    ///
    /// Prevents info leak of stale MMIO buffer data to the guest.
    /// Removing `data.fill(0)` from read_vsock_config would break this proof.
    ///
    /// Bound: while loop iterates up to 8 times (max bufsize), unwind = 9.
    #[kani::proof]
    #[kani::unwind(9)]
    fn proof_read_config_no_stale_data() {
        let guest_cid: u64 = kani::any();
        let offset: u64 = kani::any_where(|&o| o <= 12);
        let bufsize: usize = kani::any_where(|&s| s >= 1 && s <= 8);
        let cid_bytes = guest_cid.to_le_bytes();

        let mut buf = [0xFFu8; 8];
        let data = &mut buf[..bufsize];
        read_vsock_config(guest_cid, offset, data);

        let mut i: usize = 0;
        while i < bufsize {
            let config_idx = offset as usize + i;
            if config_idx < 8 {
                kani::assert(
                    data[i] == cid_bytes[config_idx],
                    "in-range byte must match config",
                );
            } else {
                kani::assert(data[i] == 0, "out-of-range byte must be zero");
            }
            i += 1;
        }

        // Coverage: verify proof exercises key scenarios
        kani::cover!(offset == 0 && bufsize == 8, "exact full read");
        kani::cover!(offset >= 8, "fully out of range");
        kani::cover!(
            offset < 8 && offset as usize + bufsize > 8,
            "partial overlap"
        );
        kani::cover!(offset == 6 && bufsize == 4, "cross-boundary read");
    }
}
