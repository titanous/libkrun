use super::muxer::VsockMuxer;
use super::TsiFlags;
use crate::virtio::{InterruptTransport, Queue};
use std::sync::{Arc, Condvar, Mutex};
use utils::eventfd::EventFd;
use vm_memory::{GuestAddress, GuestMemoryMmap};

#[test]
fn muxer_shutdown_joins_memory_using_worker() {
    let mut muxer = VsockMuxer::new(200, None, None, TsiFlags::empty());
    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0x40000000), 0x10000)]).unwrap();
    let queue = Arc::new(Mutex::new(Queue::new(256)));
    let interrupt =
        InterruptTransport::new_nitro(vec![EventFd::new(0).unwrap()], "shutdown-test".into())
            .unwrap();
    muxer.activate(
        memory,
        queue,
        interrupt,
        EventFd::new(0x800).unwrap(),
        EventFd::new(0x800).unwrap(),
        EventFd::new(0).unwrap(),
        Arc::new((Mutex::new(false), Condvar::new())),
        EventFd::new(0x800).unwrap(),
        EventFd::new(0).unwrap(),
        Arc::new((Mutex::new(false), Condvar::new())),
    );
    muxer.shutdown();
    // A second shutdown must be harmless and cannot wait on detached workers.
    muxer.shutdown();
}
