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

#[cfg(feature = "snapshot")]
impl VhostUserVsock {
    fn activate_restore(
        &mut self,
        _mem: vm_memory::GuestMemoryMmap,
        _interrupt: crate::virtio::InterruptTransport,
        _queues: Vec<crate::virtio::DeviceQueue>,
        _state: VhostUserVsockState,
    ) -> ActivateResult {
        Err(ActivateError::BadActivate) // Stub until Phase 4
    }
}

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
