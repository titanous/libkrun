//! Host configuration contract for fresh backends over frozen guest memory.
use crate::device_manager::shm::ShmManager;
use crate::resources::VmResources;
use vm_memory::Address;

fn encode_device(data: &mut Vec<u8>, device: &dyn devices::virtio::VirtioDevice) {
    data.extend_from_slice(&device.device_type().to_le_bytes());
    data.extend_from_slice(&device.avail_features().to_le_bytes());
    data.extend_from_slice(&(device.queue_config().len() as u64).to_le_bytes());
    for queue in device.queue_config() {
        data.extend_from_slice(&queue.size.to_le_bytes());
    }
}

pub fn encode(resources: &VmResources, shm: &ShmManager) -> std::io::Result<Vec<u8>> {
    #[cfg(feature = "vhost-user")]
    if !resources.vhost_user_devices.is_empty()
        || !resources.vhost_user_fs.is_empty()
        || resources.vhost_user_vsock.is_some()
    {
        return Err(std::io::Error::other(
            "cooperative templates do not support vhost-user backends",
        ));
    }
    #[cfg(feature = "net")]
    if !resources.net.list.is_empty() {
        return Err(std::io::Error::other(
            "cooperative templates do not support network devices: guest MAC state is retained",
        ));
    }
    let mut data = Vec::new();
    let put = |data: &mut Vec<u8>, value: u64| data.extend_from_slice(&value.to_le_bytes());
    // Fixed-order transport families. Backend paths/policy/objects deliberately
    // do not participate: each new guest receives its own fresh backends.
    put(&mut data, 1);
    put(&mut data, resources.omit_balloon as u64);
    put(&mut data, resources.disable_implicit_console as u64);
    put(&mut data, resources.virtio_consoles.len() as u64);
    for console in &resources.virtio_consoles {
        use crate::resources::{PortConfig, VirtioConsoleConfigMode};
        match console {
            VirtioConsoleConfigMode::Autoconfigure(_) => put(&mut data, 0),
            VirtioConsoleConfigMode::Explicit(ports) => {
                put(&mut data, 1);
                put(&mut data, ports.len() as u64);
                for port in ports {
                    let name = match port {
                        PortConfig::Tty { name, .. } | PortConfig::InOut { name, .. } => {
                            name.as_str()
                        }
                        PortConfig::Console { .. } => "",
                    };
                    put(&mut data, name.len() as u64);
                    data.extend_from_slice(name.as_bytes());
                }
            }
            VirtioConsoleConfigMode::Custom(ports) => {
                put(&mut data, 2);
                put(&mut data, ports.len() as u64);
            }
        }
    }
    #[cfg(feature = "blk")]
    {
        put(&mut data, resources.block.list.len() as u64);
        for device in &resources.block.list {
            encode_device(&mut data, &*device.lock().unwrap());
        }
    }
    #[cfg(feature = "net")]
    {
        put(&mut data, resources.net.list.len() as u64);
        for device in &resources.net.list {
            encode_device(&mut data, &*device.lock().unwrap());
        }
    }
    put(&mut data, resources.vsock.get().is_some() as u64);
    if let Some(device) = resources.vsock.get() {
        encode_device(&mut data, &*device.lock().unwrap());
    }
    put(&mut data, resources.fs.len() as u64);
    #[cfg(not(feature = "tee"))]
    for (index, mount) in resources.fs.iter().enumerate() {
        if mount.tag.len() > 36 || mount.tag.as_bytes().contains(&0) {
            return Err(std::io::Error::other("invalid template filesystem tag"));
        }
        put(&mut data, mount.tag.len() as u64);
        data.extend_from_slice(mount.tag.as_bytes());
        let queues = devices::virtio::fs::queue_sizes();
        put(&mut data, queues.len() as u64);
        for size in queues {
            put(&mut data, size as u64);
        }
        if let Some(region) = shm.fs_region(index) {
            put(&mut data, region.guest_addr.raw_value());
            put(&mut data, region.size as u64);
        } else {
            put(&mut data, 0);
            put(&mut data, 0);
        }
    }
    Ok(data)
}

#[cfg(all(test, feature = "net"))]
mod tests {
    use super::*;
    #[test]
    fn refuses_network_devices() {
        use std::os::fd::IntoRawFd;
        let (socket, _peer) = std::os::unix::net::UnixDatagram::pair().unwrap();
        let mut resources = VmResources::default();
        resources
            .add_network_interface(crate::vmm_config::net::NetworkInterfaceConfig {
                iface_id: "template-net".into(),
                backend: devices::virtio::net::device::VirtioNetBackend::UnixgramFd(
                    socket.into_raw_fd(),
                ),
                mac: [2, 0, 0, 0, 0, 1],
                features: 0,
            })
            .unwrap();
        let (info, _) = arch::arch_memory_regions(256 << 20, None, 0, 0, None);
        let error = encode(&resources, &ShmManager::new(&info)).unwrap_err();
        assert!(error.to_string().contains("network"));
    }
}
