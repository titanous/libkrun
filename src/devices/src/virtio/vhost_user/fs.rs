// Copyright 2026, Red Hat Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! VhostUserFs device: vhost-user filesystem with DAX support.
//!
//! This module provides a specialized VhostUserFs struct that wraps the generic
//! VhostUserDevice with filesystem-specific features: device type 26, config space
//! fetching from daemon, HPQ + request queues, and DAX window allocation.

use std::io::{self, Result as IoResult};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use log::warn;
use vhost::vhost_user::message::VhostUserConfigFlags;
use vhost::vhost_user::VhostUserFrontend;
use vhost::VhostBackend;
use vm_memory::ByteValued;

use crate::virtio::device::{VirtioDevice, VirtioShmRegion};
use crate::virtio::QueueConfig;
use crate::virtio::{ActivateError, ActivateResult, Queue};
use vhost::VhostUserMemoryRegionInfo;

use super::VhostUserDevice;

#[cfg(feature = "snapshot")]
use crate::snapshot_serde;

const MAX_SNAPSHOT_BYTES: usize = 8192;

/// Snapshot state for VhostUserFs device.
/// Captures all information needed to restore the device to its saved state.
#[cfg(feature = "snapshot")]
#[derive(Clone, Debug, bincode_next::Encode, bincode_next::Decode)]
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
    /// Config space snapshot (filesystem tag, up to 36 bytes)
    config_tag: Vec<u8>,
    config_num_request_queues: u32,
}

const VIRTIO_ID_FS: u32 = 26;
const QUEUE_SIZE: u16 = 1024;

/// Config space layout for virtio-fs (matches kernel's virtio_fs_config).
#[repr(C, packed)]
#[derive(Copy, Clone, Debug)]
struct VirtioFsConfig {
    tag: [u8; 36],
    num_request_queues: u32,
}

impl Default for VirtioFsConfig {
    fn default() -> Self {
        VirtioFsConfig {
            tag: [0; 36],
            num_request_queues: 0,
        }
    }
}

// SAFETY: VirtioFsConfig is repr(C, packed) with only primitive fields
unsafe impl ByteValued for VirtioFsConfig {}

#[derive(Debug)]
pub struct VhostUserFs {
    vhost_user: VhostUserDevice,
    config: VirtioFsConfig,
    queue_configs: Vec<QueueConfig>,
    shm_region: Option<VirtioShmRegion>,
    dax_window_size: Option<usize>,
    dax_window_fd: Option<OwnedFd>,
    tag: String,
    socket_path: String,
    /// Queue state buffer for snapshot support
    queues: Vec<Queue>,
    /// Saved state from restore_backend_state(), consumed by activate() in restore mode.
    #[cfg(feature = "snapshot")]
    pending_restore_state: Option<VhostUserFsState>,
}

impl VirtioDevice for VhostUserFs {
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
        VIRTIO_ID_FS
    }

    fn device_name(&self) -> &str {
        "virtio-fs-vhost"
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
        // Zero-fill first to prevent info leak of stale MMIO buffer data
        data.fill(0);
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            warn!(
                "VhostUserFs: config read at offset {} beyond config size {}",
                offset, config_len
            );
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            let end = std::cmp::min(end, config_len);
            let len = (end - offset) as usize;
            data[..len].copy_from_slice(&config_slice[offset as usize..end as usize]);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "VhostUserFs: guest driver attempted to write device config (offset={:x}, len={})",
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

        // Save queue state for snapshot: MmioTransport's save_state() reads
        // device.queues() to serialize queue GPAs/sizes. Vhost-user devices
        // don't have worker threads that update self.queues, so copy the
        // guest-configured values from DeviceQueues here.
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
        // This handles: set_owner, set_mem_table (RAM), set_features,
        // vring setup, interrupt forwarding
        self.vhost_user.activate(mem, interrupt, queues)?;

        // Share DAX window as additional memory region (if configured)
        if let (Some(fd), Some(ref region)) = (self.dax_window_fd(), &self.shm_region) {
            let dax_region = VhostUserMemoryRegionInfo {
                guest_phys_addr: region.guest_addr,
                memory_size: region.size as u64,
                userspace_addr: region.host_addr,
                mmap_offset: 0,
                mmap_handle: fd,
            };
            if self.vhost_user.add_mem_region(&dax_region).is_err() {
                self.vhost_user.reset();
                return Err(ActivateError::BadActivate);
            }
        }

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.vhost_user.is_activated()
    }

    fn reset(&mut self) -> bool {
        self.vhost_user.reset()
    }

    fn shm_region(&self) -> Option<&VirtioShmRegion> {
        self.shm_region.as_ref()
    }

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
            // get_vring_base returns the base index directly
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
        let state = VhostUserFsState {
            tag: self.tag.clone(),
            socket_path: self.socket_path.clone(),
            dax_window_mib: self.dax_window_size.map(|s| (s / (1024 * 1024)) as u32),
            acked_features: self.vhost_user.acked_features(),
            acked_protocol_features: self.vhost_user.acked_protocol_features().bits(),
            vring_bases,
            daemon_state,
            config_tag: self.config.tag.to_vec(),
            config_num_request_queues: self.config.num_request_queues,
        };

        // 4. Serialize with bincode-next
        snapshot_serde::serialize(&state)
            .map_err(|e| log::error!("serialize VhostUserFsState: {e}"))
            .ok()
    }

    #[cfg(feature = "snapshot")]
    fn restore_backend_state(&mut self, data: &[u8]) {
        // 1. Deserialize state
        let state: VhostUserFsState =
            match snapshot_serde::deserialize::<VhostUserFsState, { MAX_SNAPSHOT_BYTES }>(data) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("deserialize VhostUserFsState: {e}");
                    return;
                }
            };

        // 2. Restore local device fields from saved state
        // NOTE: socket_path is intentionally NOT restored from snapshot state.
        // The device uses the socket_path provided at construction time to prevent
        // a tampered snapshot from redirecting the vhost-user connection.
        self.tag = state.tag.clone();
        // Copy tag back from Vec<u8> to [u8; 36]
        self.config.tag.fill(0);
        if state.config_tag.len() <= 36 {
            self.config.tag[..state.config_tag.len()].copy_from_slice(&state.config_tag);
        } else {
            self.config.tag.copy_from_slice(&state.config_tag[..36]);
        }
        self.config.num_request_queues = state.config_num_request_queues;

        // 3. Mark as inactive so complete_restore() will call activate().
        //    Without this, is_activated() returns true (from pre-snapshot),
        //    and complete_restore() skips re-activation.
        self.vhost_user.mark_inactive();

        // 4. Store state for activate() to consume in restore mode.
        //    We do NOT reconnect or load daemon state here because
        //    activate() has not run yet (restore_backend_state runs
        //    BEFORE complete_restore → activate in the VMM sequence).
        //    activate() will detect pending_restore_state and take
        //    the restore-time activation path.
        self.pending_restore_state = Some(state);
    }
}

impl VhostUserFs {
    /// Create a new VhostUserFs device connected to a vhost-user daemon.
    ///
    /// # Arguments
    ///
    /// * `tag` - Filesystem tag (must be <= 36 bytes)
    /// * `socket_path` - Path to the vhost-user Unix domain socket
    /// * `dax_window_mib` - Optional DAX window size in MiB
    ///
    /// # Returns
    ///
    /// A new VhostUserFs device or an error if connection/config fails.
    pub fn new(tag: &str, socket_path: &str, dax_window_mib: Option<u32>) -> IoResult<Self> {
        // Validate tag length
        if tag.len() > 36 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "filesystem tag length exceeds 36 bytes",
            ));
        }

        // Create inner VhostUserDevice with auto-detect queue count (num_queues=0)
        let vhost_user = VhostUserDevice::new(
            socket_path,
            VIRTIO_ID_FS,
            "virtio-fs-vhost".to_string(),
            0,
            &[],
        )?;

        // Fetch config from daemon via get_config
        let mut config = {
            let mut frontend = vhost_user.frontend.lock().unwrap();
            let config_buf = [0u8; std::mem::size_of::<VirtioFsConfig>()];
            let (_hdr, payload) = frontend
                .get_config(
                    0,
                    std::mem::size_of::<VirtioFsConfig>() as u32,
                    VhostUserConfigFlags::empty(),
                    &config_buf,
                )
                .map_err(|e| io::Error::other(format!("get_config failed: {}", e)))?;
            if let Some(cfg) = VirtioFsConfig::from_slice(payload.as_slice()) {
                *cfg
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid config from daemon",
                ));
            }
        };

        // Copy tag into config
        let tag_bytes = tag.as_bytes();
        config.tag[..tag_bytes.len()].copy_from_slice(tag_bytes);

        // Build queue configs: 1 HPQ + num_request_queues request queues
        let num_queues = config.num_request_queues as usize;
        let mut queue_configs = Vec::with_capacity(1 + num_queues);
        queue_configs.push(QueueConfig::new(QUEUE_SIZE)); // HPQ
        for _ in 0..num_queues {
            queue_configs.push(QueueConfig::new(QUEUE_SIZE)); // Request queues
        }

        // Create DAX window if requested
        let (dax_window_fd, dax_window_size) = if let Some(mib) = dax_window_mib {
            let size = (mib as usize) * 1024 * 1024;
            let raw_fd = memfd_create("vhost-fs-dax", libc::MFD_CLOEXEC)
                .map_err(|e| io::Error::other(format!("memfd_create failed: {}", e)))?;

            // SAFETY: raw_fd is valid and exclusively owned after memfd_create success
            let owned_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

            unsafe {
                if libc::ftruncate(owned_fd.as_raw_fd(), size as libc::off_t) < 0 {
                    return Err(io::Error::last_os_error());
                    // owned_fd is dropped here automatically, closing the fd
                }
            }

            (Some(owned_fd), Some(size))
        } else {
            (None, None)
        };

        let num_queues = config.num_request_queues as usize;
        let queues = (0..1 + num_queues)
            .map(|_| Queue::new(QUEUE_SIZE))
            .collect();

        Ok(VhostUserFs {
            vhost_user,
            config,
            queue_configs,
            shm_region: None,
            dax_window_size,
            dax_window_fd,
            tag: tag.to_string(),
            socket_path: socket_path.to_string(),
            queues,
            #[cfg(feature = "snapshot")]
            pending_restore_state: None,
        })
    }

    pub fn set_shm_region(&mut self, region: VirtioShmRegion) {
        self.shm_region = Some(region);
    }

    pub fn dax_window_fd(&self) -> Option<RawFd> {
        self.dax_window_fd.as_ref().map(|fd| fd.as_raw_fd())
    }

    pub fn dax_window_size(&self) -> Option<usize> {
        self.dax_window_size
    }

    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }
}

#[cfg(feature = "snapshot")]
impl VhostUserFs {
    /// Activate device in restore mode using previously saved state.
    /// Called by activate() when pending_restore_state is Some.
    fn activate_restore(
        &mut self,
        mem: vm_memory::GuestMemoryMmap,
        interrupt: crate::virtio::InterruptTransport,
        queues: Vec<crate::virtio::DeviceQueue>,
        state: VhostUserFsState,
    ) -> ActivateResult {
        use std::os::unix::net::UnixStream;

        // 1. Reconnect to daemon (AC4.6: fails if daemon unavailable)
        let stream = UnixStream::connect(&self.socket_path).map_err(|e| {
            log::error!("Failed to reconnect to daemon at {}: {e}", self.socket_path);
            ActivateError::BadActivate
        })?;

        // 2. Replace Frontend, re-negotiate features (protocol features use saved set,
        //    base virtio features are re-negotiated fresh from daemon)
        self.vhost_user.reconnect_for_restore(
            stream,
            state.acked_features,
            state.acked_protocol_features,
        )?;

        // 3. Share guest memory + set up vrings with SAVED bases
        self.vhost_user
            .activate_vhost_user(&mem, &interrupt, &queues, Some(&state.vring_bases))
            .map_err(|_| ActivateError::BadActivate)?;

        // 4. Share DAX window (add_mem_region) if configured
        if let Some(ref shm_region) = self.shm_region {
            if let Some(dax_fd) = self.dax_window_fd() {
                let dax_region = VhostUserMemoryRegionInfo {
                    guest_phys_addr: shm_region.guest_addr,
                    memory_size: shm_region.size as u64,
                    userspace_addr: shm_region.host_addr,
                    mmap_offset: 0,
                    mmap_handle: dax_fd,
                };
                if self.vhost_user.add_mem_region(&dax_region).is_err() {
                    self.vhost_user.reset();
                    return Err(ActivateError::BadActivate);
                }
            }
        }

        // 5. Load daemon state via DEVICE_STATE protocol
        if self
            .vhost_user
            .load_device_state(&state.daemon_state)
            .is_err()
        {
            self.vhost_user.reset();
            return Err(ActivateError::BadActivate);
        }

        // 6. Mark as activated
        self.vhost_user.mark_activated(mem, interrupt);

        Ok(())
    }
}

/// Wrapper around memfd_create syscall
fn memfd_create(name: &str, flags: u32) -> io::Result<RawFd> {
    let c_name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid memfd name"))?;

    let fd = unsafe { libc::memfd_create(c_name.as_ptr(), flags) };

    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_type_is_fs() {
        let config = VirtioFsConfig::default();
        let device = VhostUserFs::new_for_test(config, None);
        assert_eq!(device.device_type(), 26);
    }

    #[test]
    fn test_read_config_tag() {
        let mut config = VirtioFsConfig::default();
        let tag = b"testfs";
        config.tag[..tag.len()].copy_from_slice(tag);

        let device = VhostUserFs::new_for_test(config, None);
        let mut buf = [0u8; 36];
        device.read_config(0, &mut buf);
        assert_eq!(&buf[..6], tag);
    }

    #[test]
    fn test_read_config_num_queues() {
        let mut config = VirtioFsConfig::default();
        config.num_request_queues = 2;

        let device = VhostUserFs::new_for_test(config, None);
        let mut buf = [0u8; 4];
        device.read_config(36, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 2);
    }

    #[test]
    fn test_new_fails_with_unavailable_socket() {
        let result = VhostUserFs::new("testfs", "/tmp/nonexistent-socket-path-12345", None);
        assert!(result.is_err());
        let error = result.unwrap_err();
        // Should be a connection error (NotFound for nonexistent path), not a panic
        assert!(matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ));
    }

    // Info leak: out-of-range read must zero buffer, not leave stale data
    #[test]
    fn test_read_config_oob_zeroes_buffer() {
        let config = VirtioFsConfig::default();
        let device = VhostUserFs::new_for_test(config, None);
        let mut buf = [0xFFu8; 4];
        device.read_config(40, &mut buf); // offset 40 = beyond 40-byte config
        assert_eq!(buf, [0u8; 4], "out-of-range read must zero buffer");
    }

    // Info leak: partial read must zero untouched tail bytes
    #[test]
    fn test_read_config_partial_zeroes_tail() {
        let mut config = VirtioFsConfig::default();
        config.num_request_queues = 0x42;
        let device = VhostUserFs::new_for_test(config, None);
        let mut buf = [0xFFu8; 8];
        device.read_config(36, &mut buf); // offset 36, 8-byte buf → only 4 config bytes fit
                                          // bytes 0..4 should be num_request_queues LE, bytes 4..8 should be zero
        assert_eq!(buf[..4], 0x42u32.to_le_bytes());
        assert_eq!(buf[4..], [0u8; 4], "tail bytes must be zero, not stale");
    }

    // AC2.3: Queue layout test with 3 request queues (1 HPQ + 3 request = 4 total)
    #[test]
    fn test_queue_layout_ac2_3() {
        let mut config = VirtioFsConfig::default();
        config.num_request_queues = 3;

        let device = VhostUserFs::new_for_test(config, None);
        let queues = device.queue_config();

        // Should have 4 entries: 1 HPQ + 3 request queues
        assert_eq!(queues.len(), 4, "expected 4 queues (1 HPQ + 3 request)");

        // Each queue should have max_size of 1024
        for (i, queue_cfg) in queues.iter().enumerate() {
            assert_eq!(queue_cfg.size, 1024, "queue {} should have size 1024", i);
        }
    }

    // AC2.4: shm_region() returns VirtioShmRegion with SHM region ID 0 when DAX configured
    #[test]
    fn test_shm_region_with_dax_ac2_4() {
        let config = VirtioFsConfig::default();
        let mut device = VhostUserFs::new_for_test(config, Some(32));

        let region = VirtioShmRegion {
            host_addr: 0x1_0000_0000u64,
            guest_addr: 0x1_0000_0000u64,
            size: 32 * 1024 * 1024,
        };
        device.set_shm_region(region);

        let retrieved = device.shm_region();
        assert!(
            retrieved.is_some(),
            "shm_region() should return Some when DAX is configured"
        );

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.host_addr, 0x1_0000_0000u64);
        assert_eq!(retrieved.guest_addr, 0x1_0000_0000u64);
        assert_eq!(retrieved.size, 32 * 1024 * 1024);
    }

    // AC2.5: shm_region() returns None when dax_window_mib is None
    #[test]
    fn test_shm_region_without_dax_ac2_5() {
        let config = VirtioFsConfig::default();
        let device = VhostUserFs::new_for_test(config, None);

        assert!(
            device.shm_region().is_none(),
            "shm_region() should return None when DAX is not configured"
        );
    }

    // AC4.3: Snapshot state roundtrip test
    #[test]
    #[cfg(feature = "snapshot")]
    fn test_snapshot_state_roundtrip() {
        let state = VhostUserFsState {
            tag: "testfs".to_string(),
            socket_path: "/tmp/test.sock".to_string(),
            dax_window_mib: Some(64),
            acked_features: 0x123456,
            acked_protocol_features: 0x789abc,
            vring_bases: vec![0, 1, 2],
            daemon_state: vec![1, 2, 3, 4, 5],
            config_tag: vec![42; 36],
            config_num_request_queues: 4,
        };

        // Serialize with bincode-next
        let serialized = snapshot_serde::serialize(&state).expect("serialize failed");

        // Deserialize
        let deserialized: VhostUserFsState =
            snapshot_serde::deserialize::<VhostUserFsState, { MAX_SNAPSHOT_BYTES }>(&serialized)
                .expect("deserialize failed");

        // Verify all fields match
        assert_eq!(deserialized.tag, state.tag);
        assert_eq!(deserialized.socket_path, state.socket_path);
        assert_eq!(deserialized.dax_window_mib, state.dax_window_mib);
        assert_eq!(deserialized.acked_features, state.acked_features);
        assert_eq!(
            deserialized.acked_protocol_features,
            state.acked_protocol_features
        );
        assert_eq!(deserialized.vring_bases, state.vring_bases);
        assert_eq!(deserialized.daemon_state, state.daemon_state);
        assert_eq!(deserialized.config_tag, state.config_tag);
        assert_eq!(
            deserialized.config_num_request_queues,
            state.config_num_request_queues
        );
    }

    // AC4.6: restore_backend_state stores pending state
    #[test]
    #[cfg(feature = "snapshot")]
    fn test_restore_backend_state_stores_pending() {
        let config = VirtioFsConfig::default();
        let mut device = VhostUserFs::new_for_test(config, None);

        // Create a test state
        let state = VhostUserFsState {
            tag: "testfs".to_string(),
            socket_path: "/tmp/test.sock".to_string(),
            dax_window_mib: Some(32),
            acked_features: 0xdeadbeef,
            acked_protocol_features: 0xcafebabe,
            vring_bases: vec![10, 20],
            daemon_state: vec![99, 88, 77],
            config_tag: vec![123; 36],
            config_num_request_queues: 8,
        };

        // Serialize it
        let serialized = snapshot_serde::serialize(&state).expect("serialize failed");

        // Call restore_backend_state
        device.restore_backend_state(&serialized);

        // Verify pending_restore_state is Some and contains the right data
        assert!(device.pending_restore_state.is_some());
        let restored = device.pending_restore_state.as_ref().unwrap();
        assert_eq!(restored.tag, "testfs");
        assert_eq!(restored.socket_path, "/tmp/test.sock");
        assert_eq!(restored.dax_window_mib, Some(32));
        assert_eq!(restored.acked_features, 0xdeadbeef);
        assert_eq!(restored.vring_bases, vec![10, 20]);
    }

    // AC4.6: activate_restore fails gracefully when daemon unavailable
    #[test]
    #[cfg(feature = "snapshot")]
    fn test_activate_restore_fails_when_daemon_unavailable() {
        use crate::legacy::DummyIrqChip;
        use crate::virtio::DeviceQueue;
        use std::sync::Arc;
        use utils::eventfd::EventFd;

        let config = VirtioFsConfig::default();
        let mut device = VhostUserFs::new_for_test(config, None);

        // Create a state pointing to non-existent socket
        let state = VhostUserFsState {
            tag: "testfs".to_string(),
            socket_path: "/tmp/nonexistent-vhost-socket-12345.sock".to_string(),
            dax_window_mib: None,
            acked_features: 0,
            acked_protocol_features: 0,
            vring_bases: vec![],
            daemon_state: vec![],
            config_tag: vec![0; 36],
            config_num_request_queues: 0,
        };

        device.pending_restore_state = Some(state);

        // Create minimal test memory and queues (though activate_restore will fail at reconnect)
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(
            vm_memory::GuestAddress(0),
            1024 * 1024, // 1MB
        )])
        .expect("create guest memory");

        let queues = vec![DeviceQueue::new(
            crate::virtio::Queue::new(QUEUE_SIZE),
            Arc::new(EventFd::new(0).expect("create eventfd")),
        )];

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = crate::virtio::InterruptTransport::new(irqchip, "test-fs".into())
            .expect("create interrupt transport");

        // Try to activate_restore - should fail because daemon is unavailable
        let result = device.activate(mem, interrupt, queues);
        assert!(
            result.is_err(),
            "activate_restore should fail when daemon unavailable"
        );
    }
}

impl VhostUserFs {
    /// Constructor for unit tests that bypasses socket connection.
    /// Builds a VhostUserFs with test values without connecting to a daemon.
    #[cfg(test)]
    fn new_for_test(config: VirtioFsConfig, dax_window_mib: Option<u32>) -> Self {
        let num_queues = config.num_request_queues as usize;
        let mut queue_configs = Vec::with_capacity(1 + num_queues);
        queue_configs.push(QueueConfig::new(QUEUE_SIZE)); // HPQ
        for _ in 0..num_queues {
            queue_configs.push(QueueConfig::new(QUEUE_SIZE)); // Request queues
        }

        let queues = (0..1 + num_queues)
            .map(|_| Queue::new(QUEUE_SIZE))
            .collect();

        VhostUserFs {
            vhost_user: VhostUserDevice::new_for_test_unconnected(),
            config,
            queue_configs,
            shm_region: None,
            dax_window_size: dax_window_mib.map(|mib| (mib as usize) * 1024 * 1024),
            dax_window_fd: None,
            tag: String::new(),
            socket_path: String::new(),
            queues,
            #[cfg(feature = "snapshot")]
            pending_restore_state: None,
        }
    }
}
