use std::fs::File;

use vhost_user_backend::{VhostUserBackendMut, VringMutex};
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::epoll::EventSet;
use vhost::vhost_user::message::{VhostTransferStateDirection, VhostTransferStatePhase, VhostUserProtocolFeatures};

use crate::filesystem::SyntheticFs;

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
        _evset: EventSet,
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
    pub fn process_queue(&mut self, _vring: &VringMutex) -> std::io::Result<()> {
        // Process FUSE messages from the virtqueue
        // Stub for now - will be implemented in Task 3
        Ok(())
    }

    fn save_state_to_fd(&mut self, _fd: &File) -> std::io::Result<()> {
        // Sync any guest DAX writes
        self.sync_all_dax_writes();
        // Serialization logic will be in Task 4
        Ok(())
    }

    fn load_state_from_fd(&mut self, _fd: &File) -> std::io::Result<()> {
        // Deserialization logic will be in Task 4
        Ok(())
    }

    fn sync_all_dax_writes(&mut self) {
        // Placeholder for syncing DAX writes during save
    }

    pub fn sync_dax_writes(&mut self, _nodeid: u64, _moffset: usize, _len: usize) {
        // Placeholder for syncing individual DAX writes
    }
}
