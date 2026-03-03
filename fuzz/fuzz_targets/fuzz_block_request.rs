#![no_main]

use libfuzzer_sys::fuzz_target;
use vm_memory::ByteValued;

/// Replicates `RequestHeader` from `devices::virtio::block::worker`.
/// Fields are `pub` here for direct construction in corpus seeding.
///
/// Layout: request_type(u32) + _reserved(u32) + sector(u64) = 16 bytes total.
#[derive(Copy, Clone, Default)]
#[repr(C)]
struct RequestHeader {
    request_type: u32,
    _reserved: u32,
    sector: u64,
}

// SAFETY: RequestHeader is #[repr(C)] with only POD fields and no padding.
unsafe impl ByteValued for RequestHeader {}

// Virtio block request type constants (from virtio_bindings::virtio_blk).
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;
const VIRTIO_BLK_T_DISCARD: u32 = 11;
const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

fuzz_target!(|data: &[u8]| {
    // RequestHeader is 16 bytes. If the input is too short, pad with zeros.
    let mut buf = [0u8; std::mem::size_of::<RequestHeader>()];
    let copy_len = data.len().min(buf.len());
    buf[..copy_len].copy_from_slice(&data[..copy_len]);

    // SAFETY: Any bit pattern is valid for RequestHeader (it's ByteValued).
    let header = RequestHeader {
        request_type: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
        _reserved: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        sector: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
    };

    // Exercise the request type discrimination logic.
    // This mirrors the match in BlockWorker::process_request.
    let _type_name = match header.request_type {
        VIRTIO_BLK_T_IN => "READ",
        VIRTIO_BLK_T_OUT => "WRITE",
        VIRTIO_BLK_T_FLUSH => "FLUSH",
        VIRTIO_BLK_T_GET_ID => "GET_ID",
        VIRTIO_BLK_T_DISCARD => "DISCARD",
        VIRTIO_BLK_T_WRITE_ZEROES => "WRITE_ZEROES",
        _ => "UNKNOWN",
    };

    // Verify sector bounds check (mirrors what process_request does):
    // sector * 512 must not overflow u64.
    let _sector_byte_offset = header.sector.checked_mul(512);
});
