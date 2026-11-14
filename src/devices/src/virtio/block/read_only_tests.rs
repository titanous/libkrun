use super::*;
use crate::virtio::block::VolatileSlice;
use crate::virtio::queue::{Descriptor, DescriptorChain, Queue};
use std::sync::Arc;
use vm_memory::{Bytes, GuestAddress};

struct NoWrites;
impl BlockBackend for NoWrites {
    fn cache_type(&self) -> CacheType {
        CacheType::Writeback
    }
    fn nsectors(&self) -> u64 {
        8
    }
    fn image_id(&self) -> &[u8] {
        b"read-only"
    }
    fn read_vectored_at(&self, _: &[VolatileSlice], _: u64) -> io::Result<usize> {
        Ok(0)
    }
    fn write_vectored_at(&self, _: &[VolatileSlice], _: u64) -> io::Result<usize> {
        panic!("write reached read-only backend")
    }
    fn flush(&self) -> io::Result<()> {
        panic!("flush reached read-only backend")
    }
    fn discard(&self, _: u64, _: u64) -> io::Result<()> {
        panic!("discard reached read-only backend")
    }
    fn write_zeroes(&self, _: u64, _: u64, _: bool) -> io::Result<()> {
        panic!("zeroes reached read-only backend")
    }
}

#[test]
fn worker_rejects_read_only_write_before_backend() {
    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
    mem.write_obj(
        Descriptor {
            addr: 0x4000,
            len: 512,
            flags: 1,
            next: 1,
        },
        GuestAddress(0x1000),
    )
    .unwrap();
    mem.write_obj(
        Descriptor {
            addr: 0x5000,
            len: 1,
            flags: 2,
            next: 0,
        },
        GuestAddress(0x1010),
    )
    .unwrap();
    let chain = DescriptorChain::checked_new(&mem, GuestAddress(0x1000), 8, 0).unwrap();
    let mut reader = Reader::new(&mem, chain.clone()).unwrap();
    let mut writer = Writer::new(&mem, chain).unwrap();
    let irqchip = crate::legacy::DummyIrqChip::new().into();
    let interrupt = InterruptTransport::new(irqchip, "read-only-test".into()).unwrap();
    let queue = DeviceQueue {
        queue: Queue::new(8),
        event: Arc::new(EventFd::new(0).unwrap()),
    };
    let mut worker = BlockWorker::new(
        queue,
        interrupt,
        mem.clone(),
        NoWrites,
        EventFd::new(0).unwrap(),
    );
    worker.read_only = true;
    for request_type in [
        VIRTIO_BLK_T_OUT,
        VIRTIO_BLK_T_DISCARD,
        VIRTIO_BLK_T_WRITE_ZEROES,
        VIRTIO_BLK_T_FLUSH,
    ] {
        assert!(worker
            .process_request(
                RequestHeader {
                    request_type,
                    ..Default::default()
                },
                &mut reader,
                &mut writer,
            )
            .is_err());
    }
}
