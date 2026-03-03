#![no_main]

use std::io::Read;

use libfuzzer_sys::fuzz_target;

use devices::virtio::descriptor_utils::{Reader, Writer};
use devices::virtio::queue::DescriptorChain;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

/// Guest memory layout:
///   [0x0000, 0x0100): descriptor table (16 bytes per descriptor * 16 = 256 bytes)
///   [0x0100, 0x8000): data buffers (pointed to by descriptors)
///
/// We write the fuzzer input into the descriptor table region, then let the
/// descriptor chain parser interpret whatever bytes were placed there.
const DESC_TABLE_ADDR: u64 = 0x0;
const QUEUE_SIZE: u16 = 16;
const MEM_SIZE: usize = 0x8000;

fuzz_target!(|data: &[u8]| {
    // Allocate a fresh guest memory region for each fuzzing iteration.
    let mem = match GuestMemoryMmap::from_ranges(&[(GuestAddress(0x0), MEM_SIZE)]) {
        Ok(m) => m,
        Err(_) => return,
    };

    // Write the fuzzer-provided bytes into the descriptor table region.
    // Limit to the descriptor table area (QUEUE_SIZE * 16 bytes = 256 bytes).
    let desc_table_size = (QUEUE_SIZE as usize) * 16;
    let write_len = data.len().min(desc_table_size);
    if write_len > 0 {
        // Ignore write errors — the memory region is always valid.
        let _ = mem.write_slice(&data[..write_len], GuestAddress(DESC_TABLE_ADDR));
    }

    // Attempt to construct a descriptor chain from index 0.
    // checked_new returns None for invalid chains — that's expected.
    let chain = DescriptorChain::checked_new(
        &mem,
        GuestAddress(DESC_TABLE_ADDR),
        QUEUE_SIZE,
        0, // start at index 0
    );

    let Some(chain) = chain else {
        // Invalid descriptor table — this is expected for most random inputs.
        return;
    };

    // Try constructing a Reader over the chain.
    // Reader::new contains unsafe code (VolatileSlice pointer arithmetic).
    // Any panic here is a bug.
    let chain_for_reader = chain.clone();
    let reader_result = Reader::new(&mem, chain_for_reader);

    // Try constructing a Writer over the chain.
    let writer_result = Writer::new(&mem, chain);

    // If both succeeded, exercise the Reader by reading bytes.
    if let (Ok(mut reader), Ok(_writer)) = (reader_result, writer_result) {
        // Read up to 64 bytes; ignore I/O errors (expected for malformed chains).
        let mut buf = [0u8; 64];
        let _ = reader.read(&mut buf);
    }
});
