#![no_main]

use libfuzzer_sys::fuzz_target;

use devices::virtio::DescriptorChain;
use devices::virtio::block::request::{DiscardWriteData, RequestHeader};
use devices::virtio::descriptor_utils::{Reader, Writer};
use devices::virtio::VolatileSliceGuard;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

// Virtio block request type constants (from virtio spec).
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_DISCARD: u32 = 11;
const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

// Descriptor table at 0x0; readable data at 0x100; writable data at 0x500.
const DESC_TABLE_ADDR: u64 = 0x0;
const READ_BUF_ADDR: u64 = 0x100;
const WRITE_BUF_ADDR: u64 = 0x500;

// READ_BUF_LEN = 512 + 16 = 528:
//   After reading the 16-byte RequestHeader, 512 bytes remain.
//   VIRTIO_BLK_T_OUT: data_len = 512 (multiple of 512) → exercises get_slices.
//   VIRTIO_BLK_T_DISCARD/WRITE_ZEROES: 512 bytes remain after header → read_obj::<DiscardWriteData> succeeds.
const READ_BUF_LEN: u32 = 528;

// WRITE_BUF_LEN = 512 + 1 = 513:
//   available_bytes() = 513; data_len = 513 - 1 = 512 (multiple of 512).
//   VIRTIO_BLK_T_IN: exercises get_slices(512).
//   GET_ID: exercises get_slices(512).
const WRITE_BUF_LEN: u32 = 513;

const MEM_SIZE: usize = 0x2000;

fuzz_target!(|data: &[u8]| {
    let mem = match GuestMemoryMmap::from_ranges(&[(GuestAddress(0x0), MEM_SIZE)]) {
        Ok(m) => m,
        Err(_) => return,
    };

    // Write fuzz bytes as the virtio block request (readable by the device).
    let write_len = data.len().min(READ_BUF_LEN as usize);
    if write_len > 0 {
        let _ = mem.write_slice(&data[..write_len], GuestAddress(READ_BUF_ADDR));
    }

    // Descriptor 0 (readable): fuzz request data.
    // Descriptor 1 (writable): response buffer (device writes status + data here).
    //
    // Descriptor layout: addr(u64) + len(u32) + flags(u16) + next(u16) = 16 bytes.
    let desc0: [u8; 16] = {
        let mut d = [0u8; 16];
        d[0..8].copy_from_slice(&READ_BUF_ADDR.to_le_bytes());
        d[8..12].copy_from_slice(&READ_BUF_LEN.to_le_bytes());
        d[12..14].copy_from_slice(&1u16.to_le_bytes()); // VIRTQ_DESC_F_NEXT
        d[14..16].copy_from_slice(&1u16.to_le_bytes()); // next = 1
        d
    };
    let desc1: [u8; 16] = {
        let mut d = [0u8; 16];
        d[0..8].copy_from_slice(&WRITE_BUF_ADDR.to_le_bytes());
        d[8..12].copy_from_slice(&WRITE_BUF_LEN.to_le_bytes());
        d[12..14].copy_from_slice(&2u16.to_le_bytes()); // VIRTQ_DESC_F_WRITE
        d[14..16].copy_from_slice(&0u16.to_le_bytes()); // next = 0
        d
    };
    let _ = mem.write_slice(&desc0, GuestAddress(DESC_TABLE_ADDR));
    let _ = mem.write_slice(&desc1, GuestAddress(DESC_TABLE_ADDR + 16));

    let chain = match DescriptorChain::checked_new(&mem, GuestAddress(DESC_TABLE_ADDR), 16, 0) {
        Some(c) => c,
        None => return,
    };

    let mut reader = match Reader::new(&mem, chain.clone()) {
        Ok(r) => r,
        Err(_) => return,
    };
    let writer = match Writer::new(&mem, chain) {
        Ok(w) => w,
        Err(_) => return,
    };

    // Parse the request header — mirrors parse_request() in async_worker.rs.
    let request_header: RequestHeader = match reader.read_obj() {
        Ok(h) => h,
        Err(_) => return,
    };

    // Guard: writable region must have at least one byte for the status byte.
    if writer.available_bytes() == 0 {
        return;
    }

    // Exercise get_status_ptr: unsafe pointer arithmetic into the writable region.
    let _status_ptr = writer.get_status_ptr();

    // Dispatch on request type, exercising the same parsing paths as parse_request().
    match request_header.request_type {
        VIRTIO_BLK_T_IN => {
            let data_len = writer.available_bytes() - 1;
            if data_len.is_multiple_of(512) {
                // Exercise Writer::get_slices + VolatileSliceGuard::from_volatile_slices.
                // SAFETY: GuestMemoryMmap backing lives for the duration of this call.
                let _guards =
                    unsafe { VolatileSliceGuard::from_volatile_slices(writer.get_slices(data_len)) };
            }
        }
        VIRTIO_BLK_T_OUT => {
            let data_len = reader.available_bytes();
            if data_len.is_multiple_of(512) {
                // Exercise Reader::get_slices + VolatileSliceGuard::from_volatile_slices.
                // SAFETY: GuestMemoryMmap backing lives for the duration of this call.
                let _guards =
                    unsafe { VolatileSliceGuard::from_volatile_slices(reader.get_slices(data_len)) };
            }
        }
        VIRTIO_BLK_T_DISCARD | VIRTIO_BLK_T_WRITE_ZEROES => {
            // Exercise DiscardWriteData deserialization from the reader.
            let _: Result<DiscardWriteData, _> = reader.read_obj();
        }
        // FLUSH, GET_ID, and unknown types: header parsing above is the only production path.
        _ => {}
    }
});
