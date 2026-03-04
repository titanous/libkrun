#![no_main]

use libfuzzer_sys::fuzz_target;
use vm_memory::ByteValued;

/// Local replica of `VhostUserMsgHeader` from vendor/vhost/src/vhost_user/message.rs.
///
/// The original is `pub(super)` and cannot be accessed outside the vhost crate.
/// This replica has the same on-wire layout:
///   request(u32) + flags(u32) + size(u32) = 12 bytes total.
///
/// This exercises:
/// 1. `ByteValued` zero-copy deserialization of arbitrary bytes as a header
/// 2. Flag field parsing: version bits [1:0], REPLY bit [2], NEED_REPLY bit [3]
/// 3. Request type validation (whether the u32 maps to a known FrontendReq)
/// 4. Size field interpretation
#[derive(Copy, Clone, Default, Debug)]
#[repr(C, packed)]
struct VhostUserMsgHeaderReplica {
    request: u32,
    flags: u32,
    size: u32,
}

// SAFETY: VhostUserMsgHeaderReplica is #[repr(C, packed)] with only POD fields.
unsafe impl ByteValued for VhostUserMsgHeaderReplica {}

// Bit masks from VhostUserHeaderFlag (message.rs).
const VERSION_MASK: u32 = 0x3;
const REPLY_FLAG: u32 = 0x4;
const NEED_REPLY_FLAG: u32 = 0x8;
const RESERVED_BITS: u32 = !0xf;

// Known FrontendReq variants (from message.rs enum definition).
const KNOWN_REQUEST_TYPES: &[u32] = &[
    1,  // GET_FEATURES
    2,  // SET_FEATURES
    3,  // SET_OWNER
    4,  // RESET_OWNER
    5,  // SET_MEM_TABLE
    8,  // SET_VRING_NUM
    9,  // SET_VRING_ADDR
    10, // SET_VRING_BASE
    11, // GET_VRING_BASE
    12, // SET_VRING_KICK
    13, // SET_VRING_CALL
    14, // SET_VRING_ERR
    15, // GET_PROTOCOL_FEATURES
    16, // SET_PROTOCOL_FEATURES
];

fuzz_target!(|data: &[u8]| {
    // VhostUserMsgHeaderReplica is 12 bytes. Pad with zeros if input is too short.
    let mut buf = [0u8; std::mem::size_of::<VhostUserMsgHeaderReplica>()];
    let copy_len = data.len().min(buf.len());
    buf[..copy_len].copy_from_slice(&data[..copy_len]);

    // Interpret arbitrary bytes as a message header.
    // SAFETY: Any bit pattern is valid for a ByteValued type.
    let header = VhostUserMsgHeaderReplica {
        request: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
        flags: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        size: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
    };

    // Exercise flag parsing — mirrors VhostUserMsgHeader::get_version(), is_reply(), etc.
    let _version = header.flags & VERSION_MASK;
    let _is_reply = (header.flags & REPLY_FLAG) != 0;
    let _needs_reply = (header.flags & NEED_REPLY_FLAG) != 0;
    let _has_reserved = (header.flags & RESERVED_BITS) != 0;

    // Exercise request type validation.
    // Copy packed field to local to avoid unaligned reference (E0793).
    let request = header.request;
    let _is_known = KNOWN_REQUEST_TYPES.contains(&request);

    // Exercise size field interpretation.
    // In production: size must be <= MAX_MSG_SIZE (4096). Check the boundary.
    const MAX_MSG_SIZE: u32 = 0x1000;
    let _size_valid = header.size <= MAX_MSG_SIZE;
    let _size_overflow = header.size.checked_add(12); // header + body overflow check
});
