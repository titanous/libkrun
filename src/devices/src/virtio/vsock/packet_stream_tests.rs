use super::packet::VsockPacket;
use crate::virtio::queue::{Descriptor, DescriptorChain};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

#[test]
fn tx_scatter_gather_payload_uses_header_length() {
    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
    let descriptors = [
        Descriptor {
            addr: 0x4000,
            len: 44,
            flags: 1,
            next: 1,
        },
        Descriptor {
            addr: 0x5000,
            len: 3,
            flags: 1,
            next: 2,
        },
        Descriptor {
            addr: 0x6000,
            len: 8,
            flags: 0,
            next: 0,
        },
    ];
    for (i, descriptor) in descriptors.into_iter().enumerate() {
        mem.write_obj(descriptor, GuestAddress(0x1000 + i as u64 * 16))
            .unwrap();
    }
    // virtio-vsock len is the u32 at byte 24 of the 44-byte header.
    mem.write_obj(7u32.to_le(), GuestAddress(0x4018)).unwrap();
    mem.write_slice(b"abc", GuestAddress(0x5000)).unwrap();
    mem.write_slice(b"defgJUNK", GuestAddress(0x6000)).unwrap();
    let chain = DescriptorChain::checked_new(&mem, GuestAddress(0x1000), 8, 0).unwrap();
    let packet = VsockPacket::from_tx_virtq_head(&chain).unwrap();
    assert_eq!(packet.buf().unwrap(), b"abcdefg");
}
