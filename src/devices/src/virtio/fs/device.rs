use std::cmp;
use std::io::Write;
use std::sync::atomic::AtomicI32;
use std::sync::Arc;
use std::thread::JoinHandle;

use utils::eventfd::{EventFd, EFD_NONBLOCK};
use virtio_bindings::{virtio_config::VIRTIO_F_VERSION_1, virtio_ring::VIRTIO_RING_F_EVENT_IDX};
use vm_memory::{ByteValued, GuestMemoryMmap};

use super::super::{
    ActivateResult, DeviceQueue, DeviceState, FsError, QueueConfig, VirtioDevice, VirtioShmRegion,
};
use super::filesystem::FileSystem;
use super::worker::FsWorker;
use super::ExportTable;
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;

#[derive(Copy, Clone)]
#[repr(C, packed)]
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

// SAFETY: VirtioFsConfig is #[repr(C, packed)] with no padding bytes; all bit patterns are valid for all fields.
unsafe impl ByteValued for VirtioFsConfig {}

pub struct Fs {
    avail_features: u64,
    acked_features: u64,
    device_state: DeviceState,
    config: VirtioFsConfig,
    shm_region: Option<VirtioShmRegion>,
    fs_backend: Option<Box<dyn FileSystem + Send + Sync>>,
    worker_thread: Option<JoinHandle<Box<dyn FileSystem + Send + Sync>>>,
    worker_stopfd: EventFd,
    exit_code: Arc<AtomicI32>,
}

impl Fs {
    pub fn new(
        fs_id: String,
        fs_backend: Box<dyn FileSystem + Send + Sync>,
        exit_code: Arc<AtomicI32>,
    ) -> super::Result<Fs> {
        let avail_features = (1u64 << VIRTIO_F_VERSION_1) | (1u64 << VIRTIO_RING_F_EVENT_IDX);

        let tag = fs_id.into_bytes();
        let mut config = VirtioFsConfig::default();
        config.tag[..tag.len()].copy_from_slice(tag.as_slice());
        config.num_request_queues = 1;

        Ok(Fs {
            avail_features,
            acked_features: 0,
            device_state: DeviceState::Inactive,
            config,
            shm_region: None,
            fs_backend: Some(fs_backend),
            worker_thread: None,
            worker_stopfd: EventFd::new(EFD_NONBLOCK).map_err(FsError::EventFd)?,
            exit_code,
        })
    }

    pub fn id(&self) -> &str {
        defs::FS_DEV_ID
    }

    pub fn set_shm_region(&mut self, shm_region: VirtioShmRegion) {
        self.shm_region = Some(shm_region);
    }

    pub fn set_export_table(&mut self, export_table: ExportTable) -> u64 {
        self.fs_backend
            .as_mut()
            .expect("fs_backend already taken")
            .set_export_table(export_table)
    }
}

impl VirtioDevice for Fs {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_FS
    }

    fn device_name(&self) -> &str {
        "fs"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "fs: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if self.worker_thread.is_some() {
            panic!("virtio_fs: worker thread already exists");
        }

        // Extract queues and eventfds from DeviceQueues.
        let mut worker_queues = Vec::with_capacity(queues.len());
        let mut queue_evts = Vec::with_capacity(queues.len());
        for dq in queues {
            worker_queues.push(dq.queue);
            queue_evts.push(dq.event);
        }

        let fs_backend = self.fs_backend.take().expect("fs_backend already taken");

        let worker = FsWorker::new(
            worker_queues,
            queue_evts,
            interrupt.clone(),
            mem.clone(),
            self.shm_region.clone(),
            fs_backend,
            self.worker_stopfd.try_clone().unwrap(),
            self.exit_code.clone(),
        );
        self.worker_thread = Some(worker.run());

        self.device_state = DeviceState::Activated(mem, interrupt);
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn shm_region(&self) -> Option<&VirtioShmRegion> {
        self.shm_region.as_ref()
    }

    fn reset(&mut self) -> bool {
        if let Some(worker) = self.worker_thread.take() {
            let _ = self.worker_stopfd.write(1);
            match worker.join() {
                Ok(fs_backend) => {
                    self.fs_backend = Some(fs_backend);
                }
                Err(e) => {
                    error!("error waiting for worker thread: {e:?}");
                }
            }
        }
        self.device_state = DeviceState::Inactive;
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicI32;
    use std::sync::Arc;

    use vm_memory::{GuestAddress, GuestMemoryMmap};

    use super::*;
    use utils::eventfd::{EventFd, EFD_NONBLOCK};

    use crate::legacy::DummyIrqChip;
    use crate::virtio::mmio::InterruptTransport;
    use crate::virtio::queue::tests::VirtQueue;

    /// A minimal FileSystem backend for testing.
    struct NullFs;

    impl FileSystem for NullFs {}

    fn make_test_fs() -> Fs {
        Fs::new(
            "test".to_string(),
            Box::new(NullFs),
            Arc::new(AtomicI32::new(0)),
        )
        .unwrap()
    }

    fn make_activate_args(mem: &GuestMemoryMmap) -> (InterruptTransport, Vec<DeviceQueue>) {
        let irqchip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test".into()).unwrap();

        // Fs needs 2 queues (HPQ + REQ), each with valid descriptor table layout.
        let vq0 = VirtQueue::new(GuestAddress(0), mem, 16);
        // Align vq1 start to 16 bytes (descriptor table alignment requirement).
        let vq1_start = GuestAddress((vq0.end().0 + 0xf) & !0xf);
        let vq1 = VirtQueue::new(vq1_start, mem, 16);

        let queues = vec![
            DeviceQueue::new(
                vq0.create_queue(),
                Arc::new(EventFd::new(EFD_NONBLOCK).unwrap()),
            ),
            DeviceQueue::new(
                vq1.create_queue(),
                Arc::new(EventFd::new(EFD_NONBLOCK).unwrap()),
            ),
        ];

        (interrupt, queues)
    }

    #[test]
    fn test_reactivate_after_reset_restores_backend() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut fs = make_test_fs();

        // First activation.
        let (interrupt, queues) = make_activate_args(&mem);
        fs.activate(mem.clone(), interrupt, queues).unwrap();
        assert!(fs.is_activated());

        // Reset (simulates guest writing status=0).
        assert!(fs.reset());
        assert!(!fs.is_activated());

        // Second activation must not panic.
        let (interrupt2, queues2) = make_activate_args(&mem);
        fs.activate(mem.clone(), interrupt2, queues2).unwrap();
        assert!(fs.is_activated());

        // Clean up worker thread.
        fs.reset();
    }
}
