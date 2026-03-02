// Copyright 2026, Red Hat Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Generic vhost-user device wrapper.
//!
//! This module provides a wrapper around the vhost crate's Frontend,
//! adapting it to work with libkrun's VirtioDevice trait.

use std::fs::File;
use std::io::{self, ErrorKind, Read as IoRead, Result as IoResult, Write as IoWrite};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;

use log::{debug, error};
use utils::eventfd::EventFd;
use vhost::vhost_user::message::{VhostTransferStateDirection, VhostTransferStatePhase};
use vhost::vhost_user::{Frontend, VhostUserFrontend, VhostUserProtocolFeatures};
use vhost::{VhostBackend, VhostUserMemoryRegionInfo, VringConfigData};
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};

use crate::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, InterruptTransport, QueueConfig,
    VirtioDevice,
};

/// Translate a guest physical address to a VMM virtual address.
fn gpa_to_vmm_va(mem: &GuestMemoryMmap, gpa: u64) -> IoResult<u64> {
    for region in mem.iter() {
        let region_start = region.start_addr().raw_value();
        let region_end = region_start + region.len();

        if gpa >= region_start && gpa < region_end {
            let offset = gpa - region_start;
            let vmm_va = region.as_ptr() as u64 + offset;
            return Ok(vmm_va);
        }
    }

    Err(io::Error::new(
        ErrorKind::InvalidInput,
        format!("GPA 0x{:x} not found in any memory region", gpa),
    ))
}

/// Generic vhost-user device wrapper.
///
/// This wraps a vhost-user backend connection and implements the VirtioDevice
/// trait, allowing it to be used like any other virtio device in libkrun.
pub struct VhostUserDevice {
    /// Vhost-user frontend connection
    pub(super) frontend: Arc<Mutex<Frontend>>,

    /// Device type (e.g., VIRTIO_ID_RNG = 4)
    device_type: u32,

    /// Device name for logging
    device_name: String,

    /// Queue configurations
    queue_configs: Vec<QueueConfig>,

    /// Available features from the backend
    avail_features: u64,

    /// Backend-only features (not exposed to guest)
    backend_features: u64,

    /// Acknowledged features
    acked_features: u64,

    /// Acknowledged protocol features
    acked_protocol_features: VhostUserProtocolFeatures,

    /// Whether DEVICE_STATE protocol feature is supported by the backend
    device_state_supported: bool,

    /// Device state
    device_state: DeviceState,
}

impl std::fmt::Debug for VhostUserDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VhostUserDevice")
            .field("device_type", &self.device_type)
            .field("device_name", &self.device_name)
            .field("queue_configs", &self.queue_configs)
            .field("avail_features", &self.avail_features)
            .field("backend_features", &self.backend_features)
            .field("acked_features", &self.acked_features)
            .field("acked_protocol_features", &self.acked_protocol_features)
            .field("device_state_supported", &self.device_state_supported)
            .finish_non_exhaustive()
    }
}

impl VhostUserDevice {
    /// Create a new vhost-user device by connecting to a socket.
    ///
    /// # Arguments
    ///
    /// * `socket_path` - Path to the vhost-user Unix domain socket
    /// * `device_type` - Virtio device type ID
    /// * `device_name` - Human-readable device name for logging
    /// * `num_queues` - Number of queues (0 = query backend via MQ protocol)
    /// * `queue_sizes` - Size for each queue (empty = use default 256)
    ///
    /// # Returns
    ///
    /// A new VhostUserDevice or an error if connection fails.
    pub fn new(
        socket_path: &str,
        device_type: u32,
        device_name: String,
        num_queues: u16,
        queue_sizes: &[u16],
    ) -> IoResult<Self> {
        debug!("Connecting to vhost-user backend at {}", socket_path);
        let stream = UnixStream::connect(socket_path)?;
        Self::negotiate_and_build(stream, device_type, device_name, num_queues, queue_sizes)
    }

    /// Create a new vhost-user device from a pre-connected UnixStream.
    ///
    /// This supports the fd-provisioned connection model where the orchestrator
    /// establishes the Unix socket connection before passing the fd to libkrun.
    ///
    /// # Arguments
    ///
    /// * `stream` - A pre-connected UnixStream to the vhost-user backend
    /// * `device_type` - Virtio device type ID
    /// * `device_name` - Human-readable device name for logging
    /// * `num_queues` - Number of queues (0 = query backend via MQ protocol)
    /// * `queue_sizes` - Size for each queue (empty = use default 256)
    pub fn from_stream(
        stream: UnixStream,
        device_type: u32,
        device_name: String,
        num_queues: u16,
        queue_sizes: &[u16],
    ) -> IoResult<Self> {
        debug!(
            "Creating vhost-user device from pre-connected stream for {}",
            device_name
        );
        Self::negotiate_and_build(stream, device_type, device_name, num_queues, queue_sizes)
    }

    /// Shared construction logic: negotiate features with backend and build device.
    fn negotiate_and_build(
        stream: UnixStream,
        device_type: u32,
        device_name: String,
        num_queues: u16,
        queue_sizes: &[u16],
    ) -> IoResult<Self> {
        let mut frontend = Frontend::from_stream(stream, 1);

        // Get available features from backend
        let avail_features = frontend.get_features().map_err(io::Error::other)?;

        debug!("{}: backend features: 0x{:x}", device_name, avail_features);

        // VHOST_USER_F_PROTOCOL_FEATURES (bit 30) is a backend-only feature
        // that enables vhost-user protocol extensions. It's not a virtio feature,
        // so we don't expose it to the guest, but we always use it with the backend.
        const VHOST_USER_F_PROTOCOL_FEATURES: u64 = 1 << 30;

        // Separate backend-only features from virtio features
        let backend_features = avail_features & VHOST_USER_F_PROTOCOL_FEATURES;
        let our_avail_features = avail_features & !VHOST_USER_F_PROTOCOL_FEATURES;

        // Determine actual queue count - may require protocol feature negotiation
        let acked_protocol_features = if backend_features & VHOST_USER_F_PROTOCOL_FEATURES != 0 {
            frontend
                .set_features(backend_features)
                .map_err(io::Error::other)?;

            let protocol_features = frontend.get_protocol_features().map_err(io::Error::other)?;

            let mut our_protocol_features = VhostUserProtocolFeatures::empty();
            if protocol_features.contains(VhostUserProtocolFeatures::CONFIG) {
                our_protocol_features |= VhostUserProtocolFeatures::CONFIG;
            }
            if protocol_features.contains(VhostUserProtocolFeatures::MQ) {
                our_protocol_features |= VhostUserProtocolFeatures::MQ;
            }
            if protocol_features.contains(VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS) {
                our_protocol_features |= VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS;
            }
            if protocol_features.contains(VhostUserProtocolFeatures::DEVICE_STATE) {
                our_protocol_features |= VhostUserProtocolFeatures::DEVICE_STATE;
            }

            frontend
                .set_protocol_features(our_protocol_features)
                .map_err(io::Error::other)?;

            our_protocol_features
        } else {
            VhostUserProtocolFeatures::empty()
        };

        let actual_num_queues = if num_queues == 0 {
            if backend_features & VHOST_USER_F_PROTOCOL_FEATURES != 0 {
                let backend_queue_num = frontend.get_queue_num().map_err(io::Error::other)?;

                debug!(
                    "{}: backend reports {} queues available",
                    device_name, backend_queue_num
                );

                backend_queue_num as usize
            } else {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "Backend doesn't support protocol features, must specify queue count",
                ));
            }
        } else {
            // VHOST_USER_GET_QUEUE_NUM must be called when MQ is negotiated to update
            // the frontend's internal max_queue_num, which gates set_vring_num and
            // other per-queue operations. Without this call, max_queue_num stays at 1
            // and any queue index > 0 is rejected with InvalidParam.
            if acked_protocol_features.contains(VhostUserProtocolFeatures::MQ) {
                frontend.get_queue_num().map_err(io::Error::other)?;
            }
            num_queues as usize
        };

        debug!(
            "{}: using {} queues (requested: {}, sizes provided: {})",
            device_name,
            actual_num_queues,
            num_queues,
            queue_sizes.len()
        );

        let default_size = queue_sizes.last().copied().unwrap_or(256);
        let queue_configs: Vec<_> = (0..actual_num_queues)
            .map(|i| {
                let size = queue_sizes.get(i).copied().unwrap_or(default_size);
                QueueConfig::new(size)
            })
            .collect();

        let device_state_supported =
            acked_protocol_features.contains(VhostUserProtocolFeatures::DEVICE_STATE);

        Ok(VhostUserDevice {
            frontend: Arc::new(Mutex::new(frontend)),
            device_type,
            device_name,
            queue_configs,
            avail_features: our_avail_features,
            backend_features,
            acked_features: 0,
            acked_protocol_features,
            device_state_supported,
            device_state: DeviceState::Inactive,
        })
    }

    /// Activate the vhost-user device by setting up memory and vrings.
    /// `vring_bases`: if Some, use saved vring bases (restore mode).
    ///                if None, use 0 (normal activation).
    pub(super) fn activate_vhost_user(
        &mut self,
        mem: &GuestMemoryMmap,
        interrupt: &InterruptTransport,
        queues: &[DeviceQueue],
        vring_bases: Option<&[u16]>,
    ) -> IoResult<()> {
        let mut frontend = self.frontend.lock().unwrap();

        debug!("{}: activating vhost-user device", self.device_name);

        // Combine guest-acked features with backend-only features (QEMU approach)
        let backend_feature_bits = self.acked_features | self.backend_features;

        frontend.set_owner().map_err(io::Error::other)?;

        // Only share memory regions that have file backing (memfd)
        let regions: Vec<VhostUserMemoryRegionInfo> = mem
            .iter()
            .filter_map(|region| {
                if region.file_offset().is_some() {
                    Some(VhostUserMemoryRegionInfo::from_guest_region(region))
                } else {
                    None
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                error!(
                    "{}: failed to convert memory regions: {:?}",
                    self.device_name, e
                );
                io::Error::other(e)
            })?;

        debug!(
            "{}: sharing {} file-backed regions with backend",
            self.device_name,
            regions.len()
        );

        frontend.set_mem_table(&regions).map_err(|e| {
            error!("{}: set_mem_table failed: {:?}", self.device_name, e);
            io::Error::other(e)
        })?;

        // If protocol features not negotiated, this triggers automatic ring enabling
        frontend
            .set_features(backend_feature_bits)
            .map_err(io::Error::other)?;

        // Create single vring call event file descriptor (backend->guest interrupt)
        // NOTE: Do NOT use EFD_NONBLOCK here - the monitoring thread needs to block
        let vring_call_event = EventFd::new(0)?; // Blocking eventfd

        let has_protocol_features = backend_feature_bits & (1 << 30) != 0;

        for (queue_index, device_queue) in queues.iter().enumerate() {
            let queue = &device_queue.queue;

            frontend
                .set_vring_num(queue_index, queue.actual_size())
                .map_err(io::Error::other)?;

            // Set vring base - use saved value if in restore mode, otherwise 0
            let base = vring_bases
                .and_then(|bases| bases.get(queue_index).copied())
                .unwrap_or(0);

            frontend
                .set_vring_base(queue_index, base)
                .map_err(io::Error::other)?;

            // Vring addresses in queue are GPAs, but vhost-user protocol expects VMM VAs
            let desc_table_gpa = queue.desc_table.0;
            let avail_ring_gpa = queue.avail_ring.0;
            let used_ring_gpa = queue.used_ring.0;

            let desc_table_vmm = gpa_to_vmm_va(mem, desc_table_gpa)?;
            let avail_ring_vmm = gpa_to_vmm_va(mem, avail_ring_gpa)?;
            let used_ring_vmm = gpa_to_vmm_va(mem, used_ring_gpa)?;

            let vring_config = VringConfigData {
                flags: 0,
                queue_max_size: queue.get_max_size(),
                queue_size: queue.actual_size(),
                desc_table_addr: desc_table_vmm,
                used_ring_addr: used_ring_vmm,
                avail_ring_addr: avail_ring_vmm,
                log_addr: None,
            };

            frontend
                .set_vring_addr(queue_index, &vring_config)
                .map_err(|e| {
                    error!("{}: set_vring_addr failed: {:?}", self.device_name, e);
                    io::Error::other(e)
                })?;

            frontend
                .set_vring_kick(queue_index, &device_queue.event)
                .map_err(|e| {
                    error!("{}: set_vring_kick failed: {:?}", self.device_name, e);
                    io::Error::other(e)
                })?;

            frontend
                .set_vring_call(queue_index, &vring_call_event)
                .map_err(io::Error::other)?;

            // Per QEMU vhost.c: when VHOST_USER_F_PROTOCOL_FEATURES is not negotiated,
            // the rings start directly in the enabled state, and set_vring_enable will fail.
            if has_protocol_features {
                frontend
                    .set_vring_enable(queue_index, true)
                    .map_err(io::Error::other)?;
            } else {
                debug!(
                    "{}: vring {} already enabled (protocol features not negotiated)",
                    self.device_name, queue_index
                );
            }
        }

        // Spawn single interrupt monitoring thread
        // All queues share the same vring_call_event, so we only need one thread
        // to monitor it and forward interrupts to the guest
        let vring_call_event = vring_call_event
            .try_clone()
            .map_err(|e| io::Error::other(format!("Failed to clone vring_call_event: {}", e)))?;
        let interrupt_clone = interrupt.clone();
        let device_name = self.device_name.clone();

        thread::Builder::new()
            .name(format!("{}_interrupt_monitor", self.device_name))
            .spawn(move || {
                debug!("{}: interrupt monitor thread started", device_name);
                loop {
                    // Wait for backend to signal interrupt from any queue
                    match vring_call_event.read() {
                        Ok(_) => {
                            debug!(
                                "{}: interrupt received from backend, signaling guest",
                                device_name
                            );
                            interrupt_clone.signal_used_queue();
                        }
                        Err(e) => {
                            error!("{}: interrupt monitor error: {}", device_name, e);
                            break;
                        }
                    }
                }
                debug!("{}: interrupt monitor thread exiting", device_name);
            })
            .map_err(|e| {
                io::Error::other(format!("Failed to spawn interrupt monitor thread: {}", e))
            })?;

        debug!(
            "{}: vhost-user device activated successfully",
            self.device_name
        );

        Ok(())
    }

    /// Get the acknowledged protocol features for this device.
    pub fn acked_protocol_features(&self) -> VhostUserProtocolFeatures {
        self.acked_protocol_features
    }

    /// Check if DEVICE_STATE protocol feature is supported.
    pub fn device_state_supported(&self) -> bool {
        self.device_state_supported
    }

    /// Share an additional memory region with the daemon.
    /// Requires CONFIGURE_MEM_SLOTS protocol feature to have been negotiated.
    pub fn add_mem_region(&self, region_info: &VhostUserMemoryRegionInfo) -> IoResult<()> {
        if !self
            .acked_protocol_features
            .contains(VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS)
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "CONFIGURE_MEM_SLOTS protocol feature not negotiated",
            ));
        }

        self.frontend
            .lock()
            .unwrap()
            .add_mem_region(region_info)
            .map_err(io::Error::other)?;

        Ok(())
    }

    /// Save daemon internal state via DEVICE_STATE protocol.
    /// Returns the serialized state blob.
    pub fn save_device_state(&self) -> IoResult<Vec<u8>> {
        if !self.device_state_supported {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "DEVICE_STATE protocol feature not negotiated",
            ));
        }

        // 1. Create pipe
        let (read_end, write_end) =
            nix::unistd::pipe().map_err(|e| io::Error::other(format!("pipe: {e}")))?;

        // Wrap in File which CONSUMES the OwnedFd, transferring ownership
        let read_file = File::from(read_end);
        let write_file = File::from(write_end);

        // 2. Send SET_DEVICE_STATE_FD with SAVE direction + write end
        //    The daemon will write its state to the pipe.
        //    Frontend may return a replacement fd (or None).
        let reply_fd = self
            .frontend
            .lock()
            .unwrap()
            .set_device_state_fd(
                VhostTransferStateDirection::SAVE,
                VhostTransferStatePhase::STOPPED,
                &write_file,
            )
            .map_err(|e| io::Error::other(format!("set_device_state_fd: {e}")))?;

        // Check if backend returned a replacement fd (not supported)
        if reply_fd.is_some() {
            return Err(io::Error::other(
                "backend returned replacement fd for device state, not supported",
            ));
        }

        // 3. Drop write end so we see EOF after daemon finishes writing
        drop(write_file);

        // 4. Read all data from pipe until EOF
        let mut state = Vec::new();
        let mut read_file = read_file;
        read_file
            .read_to_end(&mut state)
            .map_err(|e| io::Error::other(format!("read pipe: {e}")))?;

        // 5. CHECK_DEVICE_STATE confirms transfer completed successfully.
        //    Returns Result<()> — Ok(()) on success, Err on failure.
        self.frontend
            .lock()
            .unwrap()
            .check_device_state()
            .map_err(|e| io::Error::other(format!("check_device_state: {e}")))?;

        Ok(state)
    }

    /// Load daemon internal state via DEVICE_STATE protocol.
    pub fn load_device_state(&self, data: &[u8]) -> IoResult<()> {
        if !self.device_state_supported {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "DEVICE_STATE protocol feature not negotiated",
            ));
        }

        // 1. Create pipe
        let (read_end, write_end) =
            nix::unistd::pipe().map_err(|e| io::Error::other(format!("pipe: {e}")))?;

        // Wrap in File which CONSUMES the OwnedFd, transferring ownership
        let read_file = File::from(read_end);
        let write_file = File::from(write_end);

        // 2. Send SET_DEVICE_STATE_FD with LOAD direction + read end
        //    The daemon will read state from the pipe.
        let reply_fd = self
            .frontend
            .lock()
            .unwrap()
            .set_device_state_fd(
                VhostTransferStateDirection::LOAD,
                VhostTransferStatePhase::STOPPED,
                &read_file,
            )
            .map_err(|e| io::Error::other(format!("set_device_state_fd: {e}")))?;

        // Check if backend returned a replacement fd (not supported)
        if reply_fd.is_some() {
            return Err(io::Error::other(
                "backend returned replacement fd for device state, not supported",
            ));
        }

        // 3. Drop read end (we only write)
        drop(read_file);

        // 4. Write state data to pipe, then close to signal EOF
        let mut write_file = write_file;
        write_file
            .write_all(data)
            .map_err(|e| io::Error::other(format!("write pipe: {e}")))?;
        drop(write_file); // Signal EOF to daemon

        // 5. CHECK_DEVICE_STATE confirms transfer completed successfully.
        //    Returns Result<()> — Ok(()) on success, Err on failure.
        self.frontend
            .lock()
            .unwrap()
            .check_device_state()
            .map_err(|e| io::Error::other(format!("check_device_state: {e}")))?;

        Ok(())
    }

    /// Mark device as inactive. Used during snapshot restore to force
    /// re-activation via complete_restore() → activate() → activate_restore().
    pub(super) fn mark_inactive(&mut self) {
        self.device_state = DeviceState::Inactive;
    }

    /// Mark device as activated with given memory and interrupt.
    /// Used by subclasses (like VhostUserFs) that perform custom activation logic
    /// and need to update the device state afterward.
    pub(super) fn mark_activated(&mut self, mem: GuestMemoryMmap, interrupt: InterruptTransport) {
        self.device_state = DeviceState::Activated(mem, interrupt);
    }

    /// Replace the Frontend connection for snapshot restore.
    /// Protocol features use saved set intersected with daemon capabilities;
    /// base virtio features are re-negotiated fresh from the new daemon.
    pub(super) fn reconnect_for_restore(
        &mut self,
        stream: UnixStream,
        _saved_features: u64,
        saved_protocol_features: u64,
    ) -> ActivateResult {
        let num_queues = self.queue_configs.len() as u64;
        let frontend = Frontend::from_stream(stream, num_queues);
        *self.frontend.lock().unwrap() = frontend;

        // Mirror VhostUserDevice::new() negotiation: get features, acknowledge
        // PROTOCOL_FEATURES bit to enable protocol extensions, then negotiate
        // protocol features. Do NOT call set_owner() here — activate_vhost_user
        // does that. Calling it twice is a protocol error.
        let mut frontend = self.frontend.lock().unwrap();

        const VHOST_USER_F_PROTOCOL_FEATURES: u64 = 1 << 30;

        let backend_features = frontend
            .get_features()
            .map_err(|_| ActivateError::BadActivate)?;
        let protocol_bit = backend_features & VHOST_USER_F_PROTOCOL_FEATURES;

        // Acknowledge just the protocol features bit (same as new())
        // to enable GET_PROTOCOL_FEATURES. Full features are set by activate_vhost_user.
        if protocol_bit != 0 {
            frontend
                .set_features(protocol_bit)
                .map_err(|_| ActivateError::BadActivate)?;

            let backend_proto_features = frontend
                .get_protocol_features()
                .map_err(|_| ActivateError::BadActivate)?;
            let desired_proto =
                VhostUserProtocolFeatures::from_bits_truncate(saved_protocol_features);
            let negotiated_proto = desired_proto & backend_proto_features;
            frontend
                .set_protocol_features(negotiated_proto)
                .map_err(|_| ActivateError::BadActivate)?;

            self.acked_protocol_features = negotiated_proto;
            self.device_state_supported =
                negotiated_proto.contains(VhostUserProtocolFeatures::DEVICE_STATE);
        }

        // Update available features (daemon may have changed)
        self.avail_features = backend_features & !VHOST_USER_F_PROTOCOL_FEATURES;
        self.backend_features = protocol_bit;

        Ok(())
    }
}

impl VhostUserDevice {
    /// Helper for creating test instances without connection.
    /// This constructs a minimal device; the frontend is not used in tests.
    #[cfg(test)]
    pub(super) fn new_for_test_unconnected() -> Self {
        VhostUserDevice {
            // For tests, we create a placeholder Frontend by connecting to /dev/null.
            // This allows Frontend::from_stream to succeed without a real daemon.
            // Tests should not actually use the frontend.
            frontend: Arc::new(Mutex::new(Frontend::from_stream(
                std::os::unix::net::UnixStream::connect("/dev/null").unwrap_or_else(|_| {
                    // If /dev/null fails, create a dummy pair
                    let (a, _) = std::os::unix::net::UnixStream::pair()
                        .expect("failed to create unix socket pair for test");
                    a
                }),
                1,
            ))),
            device_type: 0,
            device_name: String::from("test-device"),
            queue_configs: vec![],
            avail_features: 0,
            backend_features: 0,
            acked_features: 0,
            acked_protocol_features: VhostUserProtocolFeatures::empty(),
            device_state_supported: false,
            device_state: DeviceState::Inactive,
        }
    }
}

impl VirtioDevice for VhostUserDevice {
    fn device_type(&self) -> u32 {
        self.device_type
    }

    fn device_name(&self) -> &str {
        &self.device_name
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &self.queue_configs
    }

    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // For now, configuration space reads are not supported
        // This can be extended using VHOST_USER_GET_CONFIG
        debug!(
            "{}: config read at offset {} (not yet implemented)",
            self.device_name, offset
        );
        data.fill(0);
    }

    fn write_config(&mut self, offset: u64, _data: &[u8]) {
        // For now, configuration space writes are not supported
        // This can be extended using VHOST_USER_SET_CONFIG
        debug!(
            "{}: config write at offset {} (not yet implemented)",
            self.device_name, offset
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if let Err(e) = self.activate_vhost_user(&mem, &interrupt, &queues, None) {
            error!(
                "{}: failed to activate vhost-user device: {}",
                self.device_name, e
            );
            return Err(ActivateError::BadActivate);
        }

        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        matches!(self.device_state, DeviceState::Activated(_, _))
    }

    fn reset(&mut self) -> bool {
        debug!("{}: resetting vhost-user device", self.device_name);

        // Disable all vrings
        if let Ok(mut frontend) = self.frontend.lock() {
            for queue_index in 0..self.queue_configs.len() {
                if let Err(e) = frontend.set_vring_enable(queue_index, false) {
                    debug!(
                        "{}: failed to disable vring {} during reset: {}",
                        self.device_name, queue_index, e
                    );
                }
            }
        }

        self.device_state = DeviceState::Inactive;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that save_device_state returns error when DEVICE_STATE not supported
    #[test]
    fn test_save_device_state_not_supported() {
        let device = VhostUserDevice::new_for_test_unconnected();
        assert!(!device.device_state_supported());

        let result = device.save_device_state();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
    }

    /// Test that load_device_state returns error when DEVICE_STATE not supported
    #[test]
    fn test_load_device_state_not_supported() {
        let device = VhostUserDevice::new_for_test_unconnected();
        assert!(!device.device_state_supported());

        let result = device.load_device_state(b"test data");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
    }
}
