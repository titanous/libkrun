#![no_main]

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;

use devices::virtio::descriptor_utils::{Reader, Writer};
use devices::virtio::fs::filesystem::FileSystem;
use devices::Server;
use devices::virtio::queue::DescriptorChain;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

/// A minimal FileSystem implementation that returns ENOSYS for all operations.
/// No methods need to be implemented — the FileSystem trait provides default ENOSYS impls.
struct NullFs;

impl FileSystem for NullFs {}

/// Guest memory layout:
///   [0x0000, 0x0100): descriptor table (1 readable + 1 writable descriptor)
///   [0x0100, 0x1100): readable data buffer (fuzz input — FUSE request)
///   [0x1100, 0x2100): writable data buffer (FUSE response)
const DESC_TABLE_ADDR: u64 = 0x0;
const READ_BUF_ADDR: u64 = 0x100;
const WRITE_BUF_ADDR: u64 = 0x1100;
const READ_BUF_LEN: u32 = 0x1000; // 4096 bytes for FUSE request
const WRITE_BUF_LEN: u32 = 0x1000; // 4096 bytes for FUSE response
const MEM_SIZE: usize = 0x4000;

fuzz_target!(|data: &[u8]| {
    // Set up guest memory.
    let mem = match GuestMemoryMmap::from_ranges(&[(GuestAddress(0x0), MEM_SIZE)]) {
        Ok(m) => m,
        Err(_) => return,
    };

    // Write fuzz data as the FUSE request into the readable buffer.
    let write_len = data.len().min(READ_BUF_LEN as usize);
    if write_len > 0 {
        let _ = mem.write_slice(&data[..write_len], GuestAddress(READ_BUF_ADDR));
    }

    // Build the descriptor table:
    //   Descriptor 0 (readable): addr=READ_BUF_ADDR, len=READ_BUF_LEN, flags=NEXT(1), next=1
    //   Descriptor 1 (writable): addr=WRITE_BUF_ADDR, len=WRITE_BUF_LEN, flags=WRITE(2), next=0
    //
    // Descriptor layout: addr(u64) + len(u32) + flags(u16) + next(u16) = 16 bytes
    let desc0: [u8; 16] = {
        let mut d = [0u8; 16];
        d[0..8].copy_from_slice(&READ_BUF_ADDR.to_le_bytes());
        d[8..12].copy_from_slice(&READ_BUF_LEN.to_le_bytes());
        d[12..14].copy_from_slice(&1u16.to_le_bytes()); // flags = VIRTQ_DESC_F_NEXT
        d[14..16].copy_from_slice(&1u16.to_le_bytes()); // next = 1
        d
    };
    let desc1: [u8; 16] = {
        let mut d = [0u8; 16];
        d[0..8].copy_from_slice(&WRITE_BUF_ADDR.to_le_bytes());
        d[8..12].copy_from_slice(&WRITE_BUF_LEN.to_le_bytes());
        d[12..14].copy_from_slice(&2u16.to_le_bytes()); // flags = VIRTQ_DESC_F_WRITE
        d[14..16].copy_from_slice(&0u16.to_le_bytes()); // next = 0
        d
    };
    let _ = mem.write_slice(&desc0, GuestAddress(DESC_TABLE_ADDR));
    let _ = mem.write_slice(&desc1, GuestAddress(DESC_TABLE_ADDR + 16));

    // Construct the descriptor chain starting at index 0.
    let chain = match DescriptorChain::checked_new(
        &mem,
        GuestAddress(DESC_TABLE_ADDR),
        16, // queue_size
        0,  // index
    ) {
        Some(c) => c,
        None => return,
    };

    // Build Reader and Writer over the chain.
    let reader = match Reader::new(&mem, chain.clone()) {
        Ok(r) => r,
        Err(_) => return,
    };
    let writer = match Writer::new(&mem, chain) {
        Ok(w) => w,
        Err(_) => return,
    };

    // Run the FUSE server message dispatcher.
    let server = Server::new(Box::new(NullFs));
    let exit_code = Arc::new(AtomicI32::new(0));
    // shm_region is None — DAX is not exercised.
    let _ = server.handle_message(reader, writer, &None, &exit_code);
});
