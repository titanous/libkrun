//! Host configuration contract for fresh backends over frozen guest memory.
#[cfg(target_arch = "x86_64")]
use crate::device_manager::shm::ShmManager;
#[cfg(target_arch = "x86_64")]
use crate::resources::VmResources;
#[cfg(target_arch = "x86_64")]
use vm_memory::Address;

pub fn encode_device(data: &mut Vec<u8>, device: &dyn devices::virtio::VirtioDevice) {
    data.extend_from_slice(&device.device_type().to_le_bytes());
    data.extend_from_slice(&device.avail_features().to_le_bytes());
    data.extend_from_slice(&(device.queue_config().len() as u64).to_le_bytes());
    for queue in device.queue_config() {
        data.extend_from_slice(&queue.size.to_le_bytes());
    }
}

#[cfg(feature = "blk")]
pub fn encode_block(
    data: &mut Vec<u8>,
    device: &devices::virtio::Block,
    identities: &std::collections::BTreeMap<String, String>,
) -> std::io::Result<()> {
    encode_device(data, device);
    let identity = identities
        .get(device.id())
        .filter(|id| !id.is_empty() && id.len() <= 1024)
        .ok_or_else(|| std::io::Error::other("template disk content identity is required"))?;
    data.extend_from_slice(&(identity.len() as u64).to_le_bytes());
    data.extend_from_slice(identity.as_bytes());
    data.extend_from_slice(&device.template_capacity()?.to_le_bytes());
    data.extend_from_slice(&(device.is_read_only() as u64).to_le_bytes());
    Ok(())
}

#[cfg(target_arch = "x86_64")]
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
            let device = device.lock().unwrap();
            encode_block(&mut data, &device, &resources.template_disk_identities)?;
        }
    }
    #[cfg(feature = "net")]
    {
        put(&mut data, resources.net.list.len() as u64);
        for device in &resources.net.list {
            encode_device(&mut data, &*device.lock().unwrap());
        }
    }
    // Entropy is always attached; custom backends use the same transport.
    let entropy = devices::virtio::Rng::new()
        .map_err(|error| std::io::Error::other(format!("entropy manifest: {error:?}")))?;
    encode_device(&mut data, &entropy);
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

#[cfg(all(test, feature = "blk", target_arch = "x86_64"))]
mod disk_tests {
    use super::*;

    #[test]
    fn disk_identity_capacity_and_mode_are_required() {
        use crate::vmm_config::block::BlockDeviceConfig;
        use devices::virtio::block::{device::BlockDeviceType, ImageType, SyncMode};
        let path = std::env::temp_dir().join(format!("clone-disk-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.set_len(4096).unwrap();
        let make = |readonly| {
            let mut resources = VmResources::default();
            resources
                .add_block_device(BlockDeviceConfig {
                    block_id: "data".into(),
                    cache_type: devices::virtio::CacheType::Writeback,
                    disk_type: BlockDeviceType::Image {
                        path: path.to_str().unwrap().into(),
                        format: ImageType::Raw,
                        sync_mode: SyncMode::Full,
                    },
                    is_disk_read_only: readonly,
                    direct_io: false,
                })
                .unwrap();
            resources
        };
        let (info, _) = arch::arch_memory_regions(256 << 20, None, 0, 0, None);
        let shm = ShmManager::new(&info);
        let mut source = make(false);
        assert!(encode(&source, &shm)
            .unwrap_err()
            .to_string()
            .contains("identity"));
        source
            .template_disk_identities
            .insert("data".into(), "snapshot-a".into());
        let original = encode(&source, &shm).unwrap();
        source
            .template_disk_identities
            .insert("data".into(), "snapshot-b".into());
        assert_ne!(original, encode(&source, &shm).unwrap());
        let mut readonly = make(true);
        readonly
            .template_disk_identities
            .insert("data".into(), "snapshot-a".into());
        assert_ne!(original, encode(&readonly, &shm).unwrap());
        file.set_len(8192).unwrap();
        let mut larger = make(false);
        larger
            .template_disk_identities
            .insert("data".into(), "snapshot-a".into());
        assert_ne!(original, encode(&larger, &shm).unwrap());
        std::fs::remove_file(path).unwrap();
    }
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
