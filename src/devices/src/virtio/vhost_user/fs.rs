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
use vm_memory::ByteValued;

use crate::virtio::device::{VirtioDevice, VirtioShmRegion};
use crate::virtio::QueueConfig;

use super::VhostUserDevice;

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

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            warn!("VhostUserFs: config read at offset {} beyond config size {}", offset, config_len);
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
    ) -> crate::virtio::ActivateResult {
        self.vhost_user.activate(mem, interrupt, queues)
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
            frontend
                .get_config(0, std::mem::size_of::<VirtioFsConfig>() as u32, VhostUserConfigFlags::empty(), &config_buf)
                .map_err(|e| io::Error::other(format!("get_config failed: {}", e)))?;
            if let Some(cfg) = VirtioFsConfig::from_slice(&config_buf) {
                *cfg
            } else {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid config from daemon"));
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

        Ok(VhostUserFs {
            vhost_user,
            config,
            queue_configs,
            shm_region: None,
            dax_window_size,
            dax_window_fd,
            tag: tag.to_string(),
            socket_path: socket_path.to_string(),
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
    fn test_queue_config_hpq_plus_request_queues() {
        let mut config = VirtioFsConfig::default();
        config.num_request_queues = 2;

        let device = VhostUserFs::new_for_test(config, None);
        assert_eq!(device.queue_config().len(), 3); // 1 HPQ + 2 request queues
        for queue_cfg in device.queue_config() {
            assert_eq!(queue_cfg.size, 1024);
        }
    }

    #[test]
    fn test_shm_region_none_without_dax() {
        let config = VirtioFsConfig::default();
        let device = VhostUserFs::new_for_test(config, None);
        assert!(device.shm_region().is_none());
    }

    #[test]
    fn test_shm_region_some_with_dax() {
        let config = VirtioFsConfig::default();
        let mut device = VhostUserFs::new_for_test(config, Some(32));

        let region = VirtioShmRegion {
            host_addr: 0x1000,
            guest_addr: 0x2000,
            size: 32 * 1024 * 1024,
        };
        device.set_shm_region(region);

        let retrieved = device.shm_region().unwrap();
        assert_eq!(retrieved.host_addr, 0x1000);
        assert_eq!(retrieved.guest_addr, 0x2000);
    }

    #[test]
    fn test_new_fails_with_unavailable_socket() {
        let result = VhostUserFs::new("testfs", "/tmp/nonexistent-socket-path-12345", None);
        assert!(result.is_err());
        let error = result.unwrap_err();
        // Should be a connection error (NotFound for nonexistent path), not a panic
        assert!(matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused));
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

        VhostUserFs {
            vhost_user: VhostUserDevice::new_for_test_unconnected(),
            config,
            queue_configs,
            shm_region: None,
            dax_window_size: dax_window_mib.map(|mib| (mib as usize) * 1024 * 1024),
            dax_window_fd: None,
            tag: String::new(),
            socket_path: String::new(),
        }
    }
}
