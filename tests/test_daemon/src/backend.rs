use std::cell::RefCell;
use std::fs::File;

use vhost::vhost_user::message::{
    VhostTransferStateDirection, VhostTransferStatePhase, VhostUserProtocolFeatures,
};
use vhost_user_backend::{VhostUserBackendMut, VringMutex, VringT};
use virtio_queue::{QueueOwnedT, QueueT};
use vm_memory::{
    Bytes, GuestAddressSpace, GuestMemory, GuestMemoryAtomic, GuestMemoryMmap, GuestMemoryRegion,
    MemoryRegionAddress,
};

use crate::filesystem::SyntheticFs;
use crate::fuse::*;

const NUM_QUEUES: usize = 2; // HPQ + 1 request queue
const QUEUE_SIZE: usize = 1024;

pub struct FsBackend {
    /// Synthetic filesystem state.
    /// RefCell because check_device_state(&self) must deserialize into it.
    /// SAFETY: FsBackend is always behind a Mutex in vhost-user-backend, so
    /// the RefCell is only accessed from one thread at a time.
    fs: RefCell<SyntheticFs>,
    /// DAX window pointer (set when ADD_MEM_REGION shares the memfd)
    dax_window: Option<(*mut u8, usize)>,
    /// Pending DEVICE_STATE transfer result
    device_state_result: RefCell<Option<std::io::Result<()>>>,
    /// Deferred LOAD fd: read_to_end must happen AFTER set_device_state_fd replies,
    /// because the frontend writes to the pipe only after receiving our reply.
    pending_load_fd: RefCell<Option<File>>,
    /// Guest memory reference (set by update_memory)
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    /// Number of memory regions seen (used to detect DAX window addition)
    region_count: usize,
}

// SAFETY: FsBackend is only accessed from single-threaded daemon context.
// The dax_window raw pointer is derived from an mmap that lives for
// the daemon's lifetime.
unsafe impl Send for FsBackend {}
unsafe impl Sync for FsBackend {}

impl FsBackend {
    pub fn new(fs: SyntheticFs) -> Self {
        Self {
            fs: RefCell::new(fs),
            dax_window: None,
            device_state_result: RefCell::new(None),
            pending_load_fd: RefCell::new(None),
            mem: None,
            region_count: 0,
        }
    }
}

impl VhostUserBackendMut for FsBackend {
    type Bitmap = ();
    type Vring = VringMutex;

    fn num_queues(&self) -> usize {
        NUM_QUEUES
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE
    }

    fn set_event_idx(&mut self, _enabled: bool) {}

    fn features(&self) -> u64 {
        // VIRTIO_F_VERSION_1 (bit 32) | VHOST_USER_F_PROTOCOL_FEATURES (bit 30)
        (1u64 << 32) | (1u64 << 30)
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
            | VhostUserProtocolFeatures::DEVICE_STATE
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        // Return VirtioFsConfig: tag="testfs" + num_request_queues=1
        let mut config = [0u8; 40]; // 36-byte tag + 4-byte u32
        let tag = b"testfs";
        config[..tag.len()].copy_from_slice(tag);
        config[36..40].copy_from_slice(&1u32.to_le_bytes());
        let end = std::cmp::min((offset as usize) + (size as usize), config.len());
        config[offset as usize..end].to_vec()
    }

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        let guard = mem.memory();
        let new_count = guard.num_regions();

        // When ADD_MEM_REGION adds the DAX memfd, region count increases.
        // The new region is the DAX window — grab its host pointer.
        eprintln!(
            "[test-daemon] update_memory: prev_regions={} new_regions={}",
            self.region_count, new_count
        );
        if new_count > self.region_count && self.region_count > 0 {
            if let Some(region) = guard.iter().last() {
                let ptr = region
                    .get_host_address(MemoryRegionAddress(0))
                    .map_err(|e| std::io::Error::other(format!("DAX region address: {:?}", e)))?;
                let size = region.len() as usize;
                eprintln!(
                    "[test-daemon] DAX window detected: ptr={:?} size={}",
                    ptr, size
                );
                self.dax_window = Some((ptr, size));
            }
        }

        self.region_count = new_count;
        drop(guard);
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
        match direction {
            VhostTransferStateDirection::SAVE => {
                *self.device_state_result.get_mut() = Some(self.save_state_to_fd(&fd));
            }
            VhostTransferStateDirection::LOAD => {
                // Defer read to check_device_state: the frontend writes to the
                // pipe only AFTER receiving our reply, so reading here deadlocks.
                *self.pending_load_fd.get_mut() = Some(fd);
                *self.device_state_result.get_mut() = None;
            }
        }
        Ok(None) // No fd to return
    }

    fn check_device_state(&self) -> std::io::Result<()> {
        // Complete deferred LOAD if pending
        if let Some(fd) = self.pending_load_fd.borrow_mut().take() {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut file = fd.try_clone()?;
            file.read_to_end(&mut buf)?;
            let result = SyntheticFs::deserialize(&buf);
            match result {
                Ok(fs) => {
                    *self.fs.borrow_mut() = fs;
                    *self.device_state_result.borrow_mut() = Some(Ok(()));
                }
                Err(e) => {
                    *self.device_state_result.borrow_mut() = Some(Err(e));
                }
            }
        }
        let result = self.device_state_result.borrow();
        match result.as_ref() {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(std::io::Error::new(e.kind(), e.to_string())),
            None => Err(std::io::Error::other("no state transfer in progress")),
        }
    }
}

impl FsBackend {
    pub fn process_queue(&mut self, vring: &VringMutex) -> std::io::Result<()> {
        let mut vring_lock = vring.get_mut();
        let mem_ref = self
            .mem
            .as_ref()
            .ok_or_else(|| std::io::Error::other("guest memory not initialized"))?;

        // Get the guest memory guard - this requires dereferencing the Atomic wrapper
        let guest_mem = mem_ref.memory();
        let guest_mem_deref = &*guest_mem; // Dereference to get &GuestMemoryMmap

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
                    guest_mem_deref
                        .read_slice(&mut buf, addr)
                        .map_err(|e| std::io::Error::other(format!("failed to read: {}", e)))?;
                    request_bytes.extend_from_slice(&buf);
                }
            }

            // Parse and dispatch FUSE request
            let response = if request_bytes.len() >= std::mem::size_of::<FuseInHeader>() {
                let header: FuseInHeader = bytes_to_struct(&request_bytes).unwrap();
                eprintln!(
                    "[test-daemon] FUSE opcode={} nodeid={} unique={} len={}",
                    header.opcode, header.nodeid, header.unique, header.len
                );
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
                        vring_lock
                            .get_queue_mut()
                            .add_used(guest_mem_deref, head_index, 0)
                            .ok();
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
                    guest_mem_deref
                        .write_slice(&response[offset..offset + write_len], addr)
                        .map_err(|e| std::io::Error::other(format!("failed to write: {}", e)))?;
                    offset += write_len;
                }
            }

            // Mark descriptor as used
            let response_len = response.len() as u32;
            vring_lock
                .get_queue_mut()
                .add_used(guest_mem_deref, head_index, response_len)
                .ok();
        }

        // Signal the guest
        vring_lock.signal_used_queue().ok();
        Ok(())
    }

    fn handle_init(&self, _header: &FuseInHeader, request_bytes: &[u8]) -> Vec<u8> {
        if let Some(init_in) =
            bytes_to_struct::<FuseInitIn>(&request_bytes[std::mem::size_of::<FuseInHeader>()..])
        {
            eprintln!(
                "[test-daemon] FUSE_INIT request: major={} minor={} flags=0x{:08x} flags2=0x{:08x}",
                init_in.major, init_in.minor, init_in.flags, init_in.flags2
            );
        }
        // FUSE_INIT_EXT is required for the kernel to read flags2 at all.
        // Without it, FUSE_HAS_INODE_DAX (bit 33 = bit 1 of flags2) is ignored
        // and per-inode DAX won't work.
        let response = FuseInitOut {
            major: FUSE_MAJOR,
            minor: FUSE_MINOR,
            max_readahead: 0x20000,
            flags: FUSE_MAP_ALIGNMENT | FUSE_INIT_EXT,
            max_background: 0,
            congestion_threshold: 0,
            max_write: 4096,
            time_gran: 1,
            max_pages: 256,
            map_alignment: 12, // 2^12 = 4096 (PAGE_SIZE alignment)
            flags2: 2,         // HAS_INODE_DAX (bit 1 of flags2 = bit 33 of combined flags)
            max_stack_depth: 0,
            request_timeout: 0,
            unused: [0; 11],
        };
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
        let fs = self.fs.borrow();
        let found_inode = fs.inodes.values().find(|inode| inode.name == filename);

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
                    flags: if inode.dax_enabled { FUSE_ATTR_DAX } else { 0 },
                },
            };
            let bytes = struct_to_bytes(&response);
            // Debug: verify flags field position (should be at offset 124 of FuseEntryOut)
            if bytes.len() >= 128 {
                let flags_bytes = &bytes[124..128];
                let flags_val = u32::from_le_bytes([
                    flags_bytes[0],
                    flags_bytes[1],
                    flags_bytes[2],
                    flags_bytes[3],
                ]);
                eprintln!("[test-daemon] LOOKUP response: nodeid={} dax_enabled={} attr.flags={:#x} response_len={}",
                    inode.nodeid, inode.dax_enabled, flags_val, bytes.len());
            }
            bytes
        } else {
            // ENOENT - return empty body with error in header
            vec![]
        }
    }

    fn handle_getattr(&self, header: &FuseInHeader, _request_bytes: &[u8]) -> Vec<u8> {
        let fs = self.fs.borrow();
        // Look up by nodeid
        if let Some(inode) = fs.inodes.get(&header.nodeid) {
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
                    flags: if inode.dax_enabled { FUSE_ATTR_DAX } else { 0 },
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
        let read_in: Option<FuseReadIn> = if request_bytes.len()
            >= std::mem::size_of::<FuseInHeader>() + std::mem::size_of::<FuseReadIn>()
        {
            bytes_to_struct(&request_bytes[std::mem::size_of::<FuseInHeader>()..])
        } else {
            None
        };

        let fs = self.fs.borrow();
        if let Some(read_in) = read_in {
            // Return file data (0xAA pattern, not DAX pattern)
            // AC5.4: READ returns different content than DAX path
            if let Some(data) = fs.file_data.get(&read_in.fh) {
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
            if let Some(data) = fs.file_data.get(&header.nodeid) {
                data[..std::cmp::min(4096, data.len())].to_vec()
            } else {
                vec![]
            }
        }
    }

    fn handle_setupmapping(&mut self, _header: &FuseInHeader, request_bytes: &[u8]) -> Vec<u8> {
        // Parse FuseSetupmappingIn from request
        let setupmapping_in: Option<FuseSetupmappingIn> = if request_bytes.len()
            >= std::mem::size_of::<FuseInHeader>() + std::mem::size_of::<FuseSetupmappingIn>()
        {
            bytes_to_struct(&request_bytes[std::mem::size_of::<FuseInHeader>()..])
        } else {
            None
        };

        if let Some(setupmapping) = setupmapping_in {
            let fs = self.fs.get_mut();
            eprintln!("[test-daemon] SETUPMAPPING: fh={} foffset={} len={} moffset={} flags={} dax_window={:?}",
                setupmapping.fh, setupmapping.foffset, setupmapping.len, setupmapping.moffset, setupmapping.flags,
                self.dax_window.map(|(_, sz)| sz));
            if let Some((dax_ptr, dax_size)) = self.dax_window {
                let moffset = setupmapping.moffset as usize;
                let len = setupmapping.len as usize;
                if moffset + len <= dax_size {
                    unsafe {
                        std::ptr::write_bytes(dax_ptr.add(moffset), fs.dax_pattern, len);
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

        let fs = self.fs.get_mut();

        // Serialize filesystem state
        // Simple format: number of inodes, then for each:
        // nodeid(u64) + name_len(u32) + name + mode(u32) + size(u64) + nlink(u32) + data_len(u32) + data
        let mut buf = Vec::new();
        buf.extend_from_slice(&(fs.inodes.len() as u32).to_le_bytes());

        for (nodeid, inode) in &fs.inodes {
            buf.extend_from_slice(&nodeid.to_le_bytes());
            let name_bytes = inode.name.as_bytes();
            buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(name_bytes);
            buf.extend_from_slice(&inode.mode.to_le_bytes());
            buf.extend_from_slice(&inode.size.to_le_bytes());
            buf.extend_from_slice(&inode.nlink.to_le_bytes());
            buf.push(if inode.dax_enabled { 1 } else { 0 });
            if let Some(data) = fs.file_data.get(nodeid) {
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

    fn sync_all_dax_writes(&mut self) {
        // Sync guest DAX window writes to file_data.
        // NOTE: This implementation copies the entire DAX window for all inodes.
        // This approach only works for the single-file synthetic filesystem used in testing.
        // A production implementation would track dirty ranges per inode and copy selectively.
        if let Some((dax_ptr, dax_size)) = self.dax_window {
            let fs = self.fs.get_mut();
            for (nodeid, _) in fs.inodes.iter() {
                let mut buf = vec![0u8; dax_size];
                unsafe {
                    std::ptr::copy_nonoverlapping(dax_ptr, buf.as_mut_ptr(), dax_size);
                }
                fs.dax_file_data.insert(*nodeid, buf);
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
        assert_eq!(
            std::mem::size_of::<FuseInitOut>(),
            64,
            "FuseInitOut must be 64 bytes"
        );
        assert_eq!(
            std::mem::size_of::<FuseInHeader>(),
            40,
            "FuseInHeader must be 40 bytes"
        );
        assert_eq!(
            std::mem::size_of::<FuseOutHeader>(),
            16,
            "FuseOutHeader must be 16 bytes"
        );
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
            let fs = backend.fs.borrow();
            let mut buf = Vec::new();
            buf.extend_from_slice(&(fs.inodes.len() as u32).to_le_bytes());

            for (nodeid, inode) in &fs.inodes {
                buf.extend_from_slice(&nodeid.to_le_bytes());
                let name_bytes = inode.name.as_bytes();
                buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(name_bytes);
                buf.extend_from_slice(&inode.mode.to_le_bytes());
                buf.extend_from_slice(&inode.size.to_le_bytes());
                buf.extend_from_slice(&inode.nlink.to_le_bytes());
                buf.push(if inode.dax_enabled { 1 } else { 0 });
                if let Some(data) = fs.file_data.get(nodeid) {
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
        *fresh_backend.fs.get_mut() =
            SyntheticFs::deserialize(&save_buffer).expect("should deserialize");

        // Verify inodes match
        let orig_fs = backend.fs.borrow();
        let loaded_fs = fresh_backend.fs.borrow();
        assert_eq!(
            orig_fs.inodes.len(),
            loaded_fs.inodes.len(),
            "inode count mismatch"
        );

        for (nodeid, original_inode) in &orig_fs.inodes {
            let loaded_inode = loaded_fs
                .inodes
                .get(nodeid)
                .expect(&format!("nodeid {} missing", nodeid));
            assert_eq!(original_inode.name, loaded_inode.name, "name mismatch");
            assert_eq!(original_inode.mode, loaded_inode.mode, "mode mismatch");
            assert_eq!(original_inode.size, loaded_inode.size, "size mismatch");
            assert_eq!(original_inode.nlink, loaded_inode.nlink, "nlink mismatch");
        }

        // Verify file data matches
        assert_eq!(
            orig_fs.file_data.len(),
            loaded_fs.file_data.len(),
            "file_data count mismatch"
        );

        for (nodeid, original_data) in &orig_fs.file_data {
            let loaded_data = loaded_fs
                .file_data
                .get(nodeid)
                .expect(&format!("nodeid {} data missing", nodeid));
            assert_eq!(
                original_data.len(),
                loaded_data.len(),
                "data len mismatch for nodeid {}",
                nodeid
            );
            assert_eq!(
                original_data, loaded_data,
                "data content mismatch for nodeid {}",
                nodeid
            );
        }
    }
}
