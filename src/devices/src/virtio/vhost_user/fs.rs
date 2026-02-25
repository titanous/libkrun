// Copyright 2026, Red Hat Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! VhostUserFs device: vhost-user filesystem with DAX support.
//!
//! This module provides a specialized VhostUserFs struct that wraps the generic
//! VhostUserDevice with filesystem-specific features: device type 26, config space
//! fetching from daemon, HPQ + request queues, and DAX window allocation.

#[cfg(feature = "vhost-user")]
use std::os::unix::io::RawFd;

use log::warn;
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
    pub fn set_shm_region(&mut self, region: VirtioShmRegion) {
        self.shm_region = Some(region);
    }

    pub fn dax_window_fd(&self) -> Option<RawFd> {
        self.dax_window_fd
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_type_is_fs() {
        let config = VirtioFsConfig::default();
        let device = Self::new_for_test(config, None);
        assert_eq!(device.device_type(), 26);
    }

    #[test]
    fn test_read_config_tag() {
        let mut config = VirtioFsConfig::default();
        let tag = b"testfs";
        config.tag[..tag.len()].copy_from_slice(tag);

        let device = Self::new_for_test(config, None);
        let mut buf = [0u8; 36];
        device.read_config(0, &mut buf);
        assert_eq!(&buf[..6], tag);
    }

    #[test]
    fn test_read_config_num_queues() {
        let mut config = VirtioFsConfig::default();
        config.num_request_queues = 2;

        let device = Self::new_for_test(config, None);
        let mut buf = [0u8; 4];
        device.read_config(36, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 2);
    }

    #[test]
    fn test_queue_config_hpq_plus_request_queues() {
        let mut config = VirtioFsConfig::default();
        config.num_request_queues = 2;

        let device = Self::new_for_test(config, None);
        assert_eq!(device.queue_config().len(), 3); // 1 HPQ + 2 request queues
        for queue_cfg in device.queue_config() {
            assert_eq!(queue_cfg.size, 1024);
        }
    }

    #[test]
    fn test_shm_region_none_without_dax() {
        let config = VirtioFsConfig::default();
        let device = Self::new_for_test(config, None);
        assert!(device.shm_region().is_none());
    }

    #[test]
    fn test_shm_region_some_with_dax() {
        let config = VirtioFsConfig::default();
        let mut device = Self::new_for_test(config, Some(32));

        let region = VirtioShmRegion {
            host_addr: 0x1000,
            guest_addr: 0x2000,
            size: 32 * 1024 * 1024,
        };
        device.set_shm_region(region.clone());

        let retrieved = device.shm_region().unwrap();
        assert_eq!(retrieved.host_addr, 0x1000);
        assert_eq!(retrieved.guest_addr, 0x2000);
    }

    impl VhostUserFs {
        /// Constructor for unit tests that bypasses socket connection.
        #[cfg(test)]
        fn new_for_test(config: VirtioFsConfig, dax_window_mib: Option<u32>) -> Self {
            let num_queues = config.num_request_queues as usize;
            let mut queue_configs = Vec::with_capacity(1 + num_queues);
            queue_configs.push(QueueConfig::new(QUEUE_SIZE)); // HPQ
            for _ in 0..num_queues {
                queue_configs.push(QueueConfig::new(QUEUE_SIZE)); // Request queues
            }

            VhostUserFs {
                vhost_user: VhostUserDevice::new_for_test(),
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
}
