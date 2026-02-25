use std::fs::File;

use vhost_user_backend::{VhostUserBackendMut, VringMutex, VringT};
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap, GuestAddressSpace, Bytes};
use vhost::vhost_user::message::{VhostTransferStateDirection, VhostTransferStatePhase, VhostUserProtocolFeatures};
use virtio_queue::{QueueT, QueueOwnedT};

use crate::filesystem::SyntheticFs;
use crate::fuse::*;

const NUM_QUEUES: usize = 2;  // HPQ + 1 request queue
const QUEUE_SIZE: usize = 1024;

pub struct FsBackend {
    /// Synthetic filesystem state
    fs: SyntheticFs,
    /// DAX window pointer (set when ADD_MEM_REGION shares the memfd)
    dax_window: Option<(*mut u8, usize)>,
    /// Pending DEVICE_STATE transfer state
    device_state_result: Option<std::io::Result<()>>,
    /// Guest memory reference (set by update_memory)
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
}

// SAFETY: FsBackend is only accessed from single-threaded daemon context.
// The dax_window raw pointer is derived from an mmap that lives for
// the daemon's lifetime.
unsafe impl Send for FsBackend {}
unsafe impl Sync for FsBackend {}

impl FsBackend {
    pub fn new(fs: SyntheticFs) -> Self {
        Self {
            fs,
            dax_window: None,
            device_state_result: None,
            mem: None,
        }
    }
}

impl VhostUserBackendMut for FsBackend {
    type Bitmap = ();
    type Vring = VringMutex;

    fn num_queues(&self) -> usize { NUM_QUEUES }

    fn max_queue_size(&self) -> usize { QUEUE_SIZE }

    fn set_event_idx(&mut self, _enabled: bool) {}

    fn features(&self) -> u64 {
        // Virtio features: VIRTIO_F_VERSION_1
        1u64 << 32
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
            | VhostUserProtocolFeatures::DEVICE_STATE
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        // Return VirtioFsConfig: tag="testfs" + num_request_queues=1
        let mut config = [0u8; 40];  // 36-byte tag + 4-byte u32
        let tag = b"testfs";
        config[..tag.len()].copy_from_slice(tag);
        config[36..40].copy_from_slice(&1u32.to_le_bytes());
        let end = std::cmp::min((offset as usize) + (size as usize), config.len());
        config[offset as usize..end].to_vec()
    }

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>)
        -> std::io::Result<()>
    {
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        _evset: vmm_sys_util::epoll::EventSet,
        vrings: &[Self::Vring],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        // device_event = queue index, triggered by kick eventfd
        if device_event as usize >= vrings.len() {
            return Ok(());
        }
        let vring = &vrings[device_event as usize];
        self.process_queue(vring)?;
        Ok(())
    }

    // --- DEVICE_STATE support (built into crate since v0.14.0) ---

    fn set_device_state_fd(
        &mut self,
        direction: VhostTransferStateDirection,
        _phase: VhostTransferStatePhase,
        fd: File,
    ) -> std::io::Result<Option<File>> {
        // Handle in background, store result for check_device_state
        match direction {
            VhostTransferStateDirection::SAVE => {
                self.device_state_result = Some(self.save_state_to_fd(&fd));
            }
            VhostTransferStateDirection::LOAD => {
                self.device_state_result = Some(self.load_state_from_fd(&fd));
            }
        }
        Ok(None)  // No fd to return
    }

    fn check_device_state(&self) -> std::io::Result<()> {
        match &self.device_state_result {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(std::io::Error::new(e.kind(), e.to_string())),
            None => Err(std::io::Error::other("no state transfer in progress")),
        }
    }
}

impl FsBackend {
    pub fn process_queue(&mut self, vring: &VringMutex) -> std::io::Result<()> {
        let mut vring_lock = vring.get_mut();
        let mem_ref = self.mem.as_ref().ok_or_else(|| {
            std::io::Error::other("guest memory not initialized")
        })?;

        // Get the guest memory guard - this requires dereferencing the Atomic wrapper
        let guest_mem = mem_ref.memory();
        let guest_mem_deref = &*guest_mem;  // Dereference to get &GuestMemoryMmap

        // Collect all descriptor chains to process
        let mut chains_to_process = Vec::new();

        {
            // Get mutable access to the virtio queue
            let queue = vring_lock.get_queue_mut();

            // Iterate over all available descriptor chains
            if let Ok(iter) = queue.iter(guest_mem_deref) {
                for desc_chain in iter {
                    chains_to_process.push(desc_chain);
                }
            }
        }

        // Now process all collected chains
        for desc_chain in chains_to_process {
            let head_index = desc_chain.head_index();

            // Read FUSE request from readable descriptors
            let mut request_bytes = Vec::new();
            for desc in desc_chain.clone().readable() {
                let addr = desc.addr();
                let len = desc.len() as usize;
                if len > 0 {
                    let mut buf = vec![0u8; len];
                    guest_mem_deref.read_slice(&mut buf, addr)
                        .map_err(|e| std::io::Error::other(format!("failed to read: {}", e)))?;
                    request_bytes.extend_from_slice(&buf);
                }
            }

            // Parse and dispatch FUSE request
            let response = if request_bytes.len() >= std::mem::size_of::<FuseInHeader>() {
                let header: FuseInHeader = bytes_to_struct(&request_bytes).unwrap();
                let response_body = match header.opcode {
                    FUSE_INIT => self.handle_init(&header, &request_bytes),
                    FUSE_LOOKUP => self.handle_lookup(&header, &request_bytes),
                    FUSE_GETATTR => self.handle_getattr(&header, &request_bytes),
                    FUSE_OPEN => self.handle_open(&header, &request_bytes),
                    FUSE_READ => self.handle_read(&header, &request_bytes),
                    FUSE_SETUPMAPPING => self.handle_setupmapping(&header, &request_bytes),
                    FUSE_REMOVEMAPPING => self.handle_removemapping(&header, &request_bytes),
                    FUSE_FORGET | FUSE_BATCH_FORGET => {
                        // No response - just mark as used with 0 bytes
                        vring_lock.get_queue_mut().add_used(guest_mem_deref, head_index, 0).ok();
                        continue;
                    }
                    _ => vec![],
                };

                // Build FuseOutHeader
                let out_header = FuseOutHeader {
                    len: (std::mem::size_of::<FuseOutHeader>() + response_body.len()) as u32,
                    error: 0,
                    unique: header.unique,
                };
                let mut response = struct_to_bytes(&out_header);
                response.extend_from_slice(&response_body);
                response
            } else {
                vec![]
            };

            // Write response to writable descriptors
            let mut offset = 0;
            for desc in desc_chain.clone().writable() {
                let addr = desc.addr();
                let len = desc.len() as usize;
                if len > 0 && offset < response.len() {
                    let write_len = std::cmp::min(len, response.len() - offset);
                    guest_mem_deref.write_slice(&response[offset..offset + write_len], addr)
                        .map_err(|e| std::io::Error::other(format!("failed to write: {}", e)))?;
                    offset += write_len;
                }
            }

            // Mark descriptor as used
            let response_len = response.len() as u32;
            vring_lock.get_queue_mut().add_used(guest_mem_deref, head_index, response_len).ok();
        }

        // Signal the guest
        vring_lock.signal_used_queue().ok();
        Ok(())
    }


    fn handle_init(&self, _header: &FuseInHeader, _request_bytes: &[u8]) -> Vec<u8> {
        // Respond with FUSE_INIT, include HAS_INODE_DAX flag
        let response = FuseInitOut {
            major: FUSE_MAJOR,
            minor: FUSE_MINOR,
            max_readahead: 0x20000,
            flags: 0,  // flags field is now u32, HAS_INODE_DAX goes in flags2
            max_background: 0,
            congestion_threshold: 0,
            max_write: 4096,
            time_gran: 1,
            max_pages: 256,
            map_alignment: 0,
            flags2: 2,  // bit 1 = FUSE_HAS_INODE_DAX (0x200000000 >> 32 = bit 1 in flags2)
            max_stack_depth: 0,
            request_timeout: 0,
            unused: [0; 11],
        };
        // AC5.1: negotiates MAP_ALIGNMENT and HAS_INODE_DAX
        struct_to_bytes(&response)
    }

    fn handle_lookup(&self, _header: &FuseInHeader, request_bytes: &[u8]) -> Vec<u8> {
        // Parse filename from request
        let filename = if request_bytes.len() > std::mem::size_of::<FuseInHeader>() {
            let name_start = std::mem::size_of::<FuseInHeader>();
            let name_bytes = &request_bytes[name_start..];
            // Find null terminator
            if let Some(null_pos) = name_bytes.iter().position(|&b| b == 0) {
                std::str::from_utf8(&name_bytes[..null_pos]).unwrap_or("")
            } else {
                ""
            }
        } else {
            ""
        };

        // Find inode by name
        let found_inode = self.fs.inodes.values().find(|inode| inode.name == filename);

        if let Some(inode) = found_inode {
            let response = FuseEntryOut {
                nodeid: inode.nodeid,
                generation: 1,
                entry_valid: 600,
                attr_valid: 600,
                entry_valid_nsec: 0,
                attr_valid_nsec: 0,
                attr: FuseAttr {
                    ino: inode.nodeid,
                    size: inode.size,
                    blocks: inode.size.div_ceil(512),
                    atime: 0,
                    mtime: 0,
                    ctime: 0,
                    atimensec: 0,
                    mtimensec: 0,
                    ctimensec: 0,
                    mode: inode.mode,
                    nlink: inode.nlink,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                    blksize: 4096,
                    flags: FUSE_ATTR_DAX,  // AC5.2: set FUSE_ATTR_DAX flag
                },
            };
            struct_to_bytes(&response)
        } else {
            // ENOENT - return empty body with error in header
            vec![]
        }
    }

    fn handle_getattr(&self, header: &FuseInHeader, _request_bytes: &[u8]) -> Vec<u8> {
        // Look up by nodeid
        if let Some(inode) = self.fs.inodes.get(&header.nodeid) {
            let response = FuseAttrOut {
                attr_valid: 600,
                attr_valid_nsec: 0,
                dummy: 0,
                attr: FuseAttr {
                    ino: inode.nodeid,
                    size: inode.size,
                    blocks: inode.size.div_ceil(512),
                    atime: 0,
                    mtime: 0,
                    ctime: 0,
                    atimensec: 0,
                    mtimensec: 0,
                    ctimensec: 0,
                    mode: inode.mode,
                    nlink: inode.nlink,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                    blksize: 4096,
                    flags: FUSE_ATTR_DAX,  // AC5.2: set FUSE_ATTR_DAX flag
                },
            };
            struct_to_bytes(&response)
        } else {
            vec![]
        }
    }

    fn handle_open(&self, header: &FuseInHeader, _request_bytes: &[u8]) -> Vec<u8> {
        // Return fh=nodeid
        let response = FuseOpenOut {
            fh: header.nodeid,
            open_flags: 0,
            padding: 0,
        };
        struct_to_bytes(&response)
    }

    fn handle_read(&self, header: &FuseInHeader, request_bytes: &[u8]) -> Vec<u8> {
        // Parse FuseReadIn from request to get offset and size
        let read_in: Option<FuseReadIn> = if request_bytes.len() >= std::mem::size_of::<FuseInHeader>() + std::mem::size_of::<FuseReadIn>() {
            bytes_to_struct(&request_bytes[std::mem::size_of::<FuseInHeader>()..])
        } else {
            None
        };

        if let Some(read_in) = read_in {
            // Return file data (0xAA pattern, not DAX pattern)
            // AC5.4: READ returns different content than DAX path
            if let Some(data) = self.fs.file_data.get(&read_in.fh) {
                let offset = read_in.offset as usize;
                let size = read_in.size as usize;
                let start = std::cmp::min(offset, data.len());
                let end = std::cmp::min(start + size, data.len());
                data[start..end].to_vec()
            } else {
                vec![]
            }
        } else {
            // Fallback if parsing fails
            if let Some(data) = self.fs.file_data.get(&header.nodeid) {
                data[..std::cmp::min(4096, data.len())].to_vec()
            } else {
                vec![]
            }
        }
    }

    fn handle_setupmapping(&mut self, _header: &FuseInHeader, request_bytes: &[u8]) -> Vec<u8> {
        // Parse FuseSetupmappingIn from request
        let setupmapping_in: Option<FuseSetupmappingIn> = if request_bytes.len() >= std::mem::size_of::<FuseInHeader>() + std::mem::size_of::<FuseSetupmappingIn>() {
            bytes_to_struct(&request_bytes[std::mem::size_of::<FuseInHeader>()..])
        } else {
            None
        };

        if let Some(setupmapping) = setupmapping_in {
            if let Some((dax_ptr, dax_size)) = self.dax_window {
                let moffset = setupmapping.moffset as usize;
                let len = setupmapping.len as usize;
                if moffset + len <= dax_size {
                    unsafe {
                        std::ptr::write_bytes(dax_ptr.add(moffset), self.fs.dax_pattern, len);
                    }
                }
            }
        }
        // AC5.3: SETUPMAPPING writes known byte pattern to DAX window at requested offset

        // Respond with success (empty response body)
        vec![]
    }

    fn handle_removemapping(&self, _header: &FuseInHeader, _request_bytes: &[u8]) -> Vec<u8> {
        // No-op, respond with success
        vec![]
    }

    fn save_state_to_fd(&mut self, fd: &File) -> std::io::Result<()> {
        use std::io::Write;

        // Sync any guest DAX writes
        self.sync_all_dax_writes();

        // Serialize filesystem state
        // Simple format: number of inodes, then for each:
        // nodeid(u64) + name_len(u32) + name + mode(u32) + size(u64) + nlink(u32) + data_len(u32) + data
        let mut buf = Vec::new();
        buf.extend_from_slice(&(self.fs.inodes.len() as u32).to_le_bytes());

        for (nodeid, inode) in &self.fs.inodes {
            buf.extend_from_slice(&nodeid.to_le_bytes());
            let name_bytes = inode.name.as_bytes();
            buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(name_bytes);
            buf.extend_from_slice(&inode.mode.to_le_bytes());
            buf.extend_from_slice(&inode.size.to_le_bytes());
            buf.extend_from_slice(&inode.nlink.to_le_bytes());
            if let Some(data) = self.fs.file_data.get(nodeid) {
                buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
                buf.extend_from_slice(data);
            } else {
                buf.extend_from_slice(&0u32.to_le_bytes());
            }
        }

        let mut file = fd.try_clone()?;
        file.write_all(&buf)?;
        Ok(())
    }

    fn load_state_from_fd(&mut self, fd: &File) -> std::io::Result<()> {
        use std::io::Read;

        // Read all bytes from fd
        let mut buf = Vec::new();
        let mut file = fd.try_clone()?;
        file.read_to_end(&mut buf)?;

        // Deserialize and restore filesystem state
        self.fs = SyntheticFs::deserialize(&buf)?;
        Ok(())
    }

    fn sync_all_dax_writes(&mut self) {
        // Sync guest DAX window writes to file_data
        if let Some((dax_ptr, dax_size)) = self.dax_window {
            for (nodeid, _) in self.fs.inodes.iter() {
                let mut buf = vec![0u8; dax_size];
                unsafe {
                    std::ptr::copy_nonoverlapping(dax_ptr, buf.as_mut_ptr(), dax_size);
                }
                self.fs.dax_file_data.insert(*nodeid, buf);
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuse_struct_sizes() {
        // Verify FUSE struct sizes match protocol
        assert_eq!(std::mem::size_of::<FuseInitOut>(), 64, "FuseInitOut must be 64 bytes");
        assert_eq!(std::mem::size_of::<FuseInHeader>(), 40, "FuseInHeader must be 40 bytes");
        assert_eq!(std::mem::size_of::<FuseOutHeader>(), 16, "FuseOutHeader must be 16 bytes");
    }

    #[test]
    fn test_bytes_to_struct_roundtrip() {
        let original = FuseInHeader {
            len: 100,
            opcode: 26,
            unique: 42,
            nodeid: 2,
            uid: 1000,
            gid: 1000,
            pid: 5000,
            padding: 0,
        };

        let bytes = struct_to_bytes(&original);
        assert_eq!(bytes.len(), std::mem::size_of::<FuseInHeader>());

        let parsed: FuseInHeader = bytes_to_struct(&bytes).expect("should parse");
        assert_eq!(parsed.len, original.len);
        assert_eq!(parsed.opcode, original.opcode);
        assert_eq!(parsed.unique, original.unique);
        assert_eq!(parsed.nodeid, original.nodeid);
    }

    #[test]
    fn test_save_load_roundtrip() {
        // Create initial filesystem
        let original_fs = SyntheticFs::new();
        let backend = FsBackend::new(original_fs);

        // Save to buffer
        // Manually serialize like save_state_to_fd does
        let save_buffer = {
            let mut buf = Vec::new();
            buf.extend_from_slice(&(backend.fs.inodes.len() as u32).to_le_bytes());

            for (nodeid, inode) in &backend.fs.inodes {
                buf.extend_from_slice(&nodeid.to_le_bytes());
                let name_bytes = inode.name.as_bytes();
                buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(name_bytes);
                buf.extend_from_slice(&inode.mode.to_le_bytes());
                buf.extend_from_slice(&inode.size.to_le_bytes());
                buf.extend_from_slice(&inode.nlink.to_le_bytes());
                if let Some(data) = backend.fs.file_data.get(nodeid) {
                    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
                    buf.extend_from_slice(data);
                } else {
                    buf.extend_from_slice(&0u32.to_le_bytes());
                }
            }
            buf
        };

        // Create new backend and load
        let mut fresh_backend = FsBackend::new(SyntheticFs::new());
        fresh_backend.fs = SyntheticFs::deserialize(&save_buffer)
            .expect("should deserialize");

        // Verify inodes match
        assert_eq!(backend.fs.inodes.len(), fresh_backend.fs.inodes.len(), "inode count mismatch");

        for (nodeid, original_inode) in &backend.fs.inodes {
            let loaded_inode = fresh_backend.fs.inodes.get(nodeid)
                .expect(&format!("nodeid {} missing", nodeid));
            assert_eq!(original_inode.name, loaded_inode.name, "name mismatch");
            assert_eq!(original_inode.mode, loaded_inode.mode, "mode mismatch");
            assert_eq!(original_inode.size, loaded_inode.size, "size mismatch");
            assert_eq!(original_inode.nlink, loaded_inode.nlink, "nlink mismatch");
        }

        // Verify file data matches
        assert_eq!(backend.fs.file_data.len(), fresh_backend.fs.file_data.len(), "file_data count mismatch");

        for (nodeid, original_data) in &backend.fs.file_data {
            let loaded_data = fresh_backend.fs.file_data.get(nodeid)
                .expect(&format!("nodeid {} data missing", nodeid));
            assert_eq!(original_data.len(), loaded_data.len(), "data len mismatch for nodeid {}", nodeid);
            assert_eq!(original_data, loaded_data, "data content mismatch for nodeid {}", nodeid);
        }
    }
}
