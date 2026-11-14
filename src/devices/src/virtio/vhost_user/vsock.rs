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
use crate::virtio::ActivateResult;
use crate::virtio::QueueConfig;

use super::VhostUserDevice;

const VIRTIO_ID_VSOCK: u32 = 19;
const NUM_QUEUES: usize = 3; // RX, TX, Event
const QUEUE_SIZE: u16 = 256;

#[derive(Debug)]
pub struct VhostUserVsock {
    vhost_user: VhostUserDevice,
    guest_cid: u64,
    queue_configs: Vec<QueueConfig>,
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

        Ok(VhostUserVsock {
            vhost_user,
            guest_cid,
            queue_configs,
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
            queue_configs: vec![QueueConfig::new(QUEUE_SIZE); NUM_QUEUES],
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
