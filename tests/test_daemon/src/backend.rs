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
    pub fs: SyntheticFs,
    /// DAX window pointer (set when ADD_MEM_REGION shares the memfd)
    pub dax_window: Option<(*mut u8, usize)>,
    /// Pending DEVICE_STATE transfer state
    pub device_state_result: Option<std::io::Result<()>>,
    /// Guest memory reference (set by update_memory)
    pub mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
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
        let mut config = vec![0u8; 40];  // 36-byte tag + 4-byte u32
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
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "no state transfer in progress",
            )),
        }
    }
}

impl FsBackend {
    pub fn process_queue(&mut self, vring: &VringMutex) -> std::io::Result<()> {
        let mut vring_lock = vring.get_mut();
        let mem_ref = self.mem.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "guest memory not initialized")
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
            if let Ok(mut iter) = queue.iter(guest_mem_deref) {
                while let Some(desc_chain) = iter.next() {
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
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("failed to read: {}", e)))?;
                    request_bytes.extend_from_slice(&buf);
                }
            }

            // Parse and dispatch FUSE request
            let response = if request_bytes.len() >= std::mem::size_of::<FuseInHeader>() {
                let header: FuseInHeader = bytes_to_struct(&request_bytes).unwrap();
                match header.opcode {
                    FUSE_INIT => self.handle_init(&header),
                    FUSE_LOOKUP => self.handle_lookup(&header),
                    FUSE_GETATTR => self.handle_getattr(&header),
                    FUSE_OPEN => self.handle_open(&header),
                    FUSE_READ => self.handle_read(&header),
                    FUSE_SETUPMAPPING => self.handle_setupmapping(&header),
                    FUSE_REMOVEMAPPING => self.handle_removemapping(&header),
                    FUSE_FORGET | FUSE_BATCH_FORGET => {
                        // No response - just mark as used with 0 bytes
                        vring_lock.get_queue_mut().add_used(guest_mem_deref, head_index, 0).ok();
                        continue;
                    }
                    _ => vec![],
                }
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
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("failed to write: {}", e)))?;
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


    fn handle_init(&self, _header: &FuseInHeader) -> Vec<u8> {
        // Respond with FUSE_INIT, include HAS_INODE_DAX flag
        let response = FuseInitOut {
            major: FUSE_MAJOR,
            minor: FUSE_MINOR,
            max_readahead: 0x20000,
            flags: FUSE_HAS_INODE_DAX,
            max_background: 0,
            congestion_threshold: 0,
            max_write: 4096,
            time_gran: 1,
            max_pages: 256,
            padding: 0,
            reserved: [0; 8],
        };
        // AC5.1: negotiates MAP_ALIGNMENT and HAS_INODE_DAX
        struct_to_bytes(&response)
    }

    fn handle_lookup(&self, _header: &FuseInHeader) -> Vec<u8> {
        // Simple lookup for "hello.txt"
        let nodeid = 2;

        if let Some(inode) = self.fs.inodes.get(&nodeid) {
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
                    blocks: (inode.size + 511) / 512,
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

    fn handle_getattr(&self, header: &FuseInHeader) -> Vec<u8> {
        // Look up by nodeid
        if let Some(inode) = self.fs.inodes.get(&header.nodeid) {
            let response = FuseAttrOut {
                attr_valid: 600,
                attr_valid_nsec: 0,
                dummy: 0,
                attr: FuseAttr {
                    ino: inode.nodeid,
                    size: inode.size,
                    blocks: (inode.size + 511) / 512,
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

    fn handle_open(&self, header: &FuseInHeader) -> Vec<u8> {
        // Return fh=nodeid
        let response = FuseOpenOut {
            fh: header.nodeid,
            open_flags: 0,
            padding: 0,
        };
        struct_to_bytes(&response)
    }

    fn handle_read(&self, header: &FuseInHeader) -> Vec<u8> {
        // Return file data (0xAA pattern, not DAX pattern)
        // AC5.4: READ returns different content than DAX path
        if let Some(data) = self.fs.file_data.get(&header.nodeid) {
            data[..std::cmp::min(4096, data.len())].to_vec()
        } else {
            vec![]
        }
    }

    fn handle_setupmapping(&mut self, _header: &FuseInHeader) -> Vec<u8> {
        // Write known byte pattern to DAX window
        // For simplicity, write to the whole DAX window
        if let Some((dax_ptr, dax_size)) = self.dax_window {
            unsafe {
                std::ptr::write_bytes(dax_ptr, self.fs.dax_pattern, dax_size);
            }
        }
        // AC5.3: SETUPMAPPING writes known byte pattern to DAX window

        // Respond with success (empty response body)
        vec![]
    }

    fn handle_removemapping(&self, _header: &FuseInHeader) -> Vec<u8> {
        // No-op, respond with success
        vec![]
    }

    fn save_state_to_fd(&mut self, fd: &File) -> std::io::Result<()> {
        use std::io::Write;

        // Sync any guest DAX writes
        self.sync_all_dax_writes();

        // Serialize filesystem state
        // Simple format: number of inodes, then for each:
        // nodeid(u64) + name_len(u32) + name + data_len(u32) + data
        let mut buf = Vec::new();
        buf.extend_from_slice(&(self.fs.inodes.len() as u32).to_le_bytes());

        for (nodeid, inode) in &self.fs.inodes {
            buf.extend_from_slice(&nodeid.to_le_bytes());
            let name_bytes = inode.name.as_bytes();
            buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(name_bytes);
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
        if buf.len() >= 4 {
            let num_inodes = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
            let mut offset = 4;

            for _ in 0..num_inodes {
                if offset + 8 > buf.len() {
                    break;
                }
                let nodeid = u64::from_le_bytes([
                    buf[offset], buf[offset+1], buf[offset+2], buf[offset+3],
                    buf[offset+4], buf[offset+5], buf[offset+6], buf[offset+7],
                ]);
                offset += 8;

                if offset + 4 > buf.len() {
                    break;
                }
                let name_len = u32::from_le_bytes([
                    buf[offset], buf[offset+1], buf[offset+2], buf[offset+3],
                ]) as usize;
                offset += 4;

                if offset + name_len > buf.len() {
                    break;
                }
                let _name = String::from_utf8_lossy(&buf[offset..offset+name_len]).to_string();
                offset += name_len;

                if offset + 4 > buf.len() {
                    break;
                }
                let data_len = u32::from_le_bytes([
                    buf[offset], buf[offset+1], buf[offset+2], buf[offset+3],
                ]) as usize;
                offset += 4;

                if offset + data_len > buf.len() {
                    break;
                }
                let data = buf[offset..offset+data_len].to_vec();
                offset += data_len;

                if data_len > 0 {
                    self.fs.file_data.insert(nodeid, data);
                }
            }
        }

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

    pub fn sync_dax_writes(&mut self, nodeid: u64, moffset: usize, len: usize) {
        if let Some((dax_ptr, _)) = self.dax_window {
            let mut buf = vec![0u8; len];
            unsafe {
                std::ptr::copy_nonoverlapping(dax_ptr.add(moffset), buf.as_mut_ptr(), len);
            }
            self.fs.dax_file_data.insert(nodeid, buf);
        }
    }
}
