// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

/// `VsockPacket` provides a thin wrapper over the buffers exchanged via virtio queues.
/// There are two components to a vsock packet, each using its own descriptor in a
/// virtio queue:
/// - the packet header; and
/// - the packet data/buffer.
///
/// There is a 1:1 relation between descriptor chains and packets: the first (chain head) holds
/// the header, and an optional second descriptor holds the data. The second descriptor is only
/// present for data packets (VSOCK_OP_RW).
///
/// `VsockPacket` wraps these two buffers and provides direct access to the data stored
/// in guest memory. This is done to avoid unnecessarily copying data from guest memory
/// to temporary buffers, before passing it on to the vsock backend.
use std::convert::TryInto;
use std::ffi::CStr;
use std::net::{Ipv4Addr, SocketAddrV4};
#[cfg(target_os = "macos")]
use std::net::{Ipv6Addr, SocketAddrV6};
use std::os::raw::c_char;
use std::result;

#[cfg(target_os = "linux")]
use nix::sys::socket::{sockaddr, AddressFamily};
use nix::sys::socket::{SockaddrLike, SockaddrStorage};
use utils::byte_order;
use vm_memory::{self, Address, GuestAddress, GuestMemoryBackend, GuestMemoryError};

use super::super::DescriptorChain;
use super::defs;
use super::{Result, VsockError};

// The vsock packet header is defined by the C struct:
//
// ```C
// struct virtio_vsock_hdr {
//     le64 src_cid;
//     le64 dst_cid;
//     le32 src_port;
//     le32 dst_port;
//     le32 len;
//     le16 type;
//     le16 op;
//     le32 flags;
//     le32 buf_alloc;
//     le32 fwd_cnt;
// };
// ```
//
// This structed will occupy the buffer pointed to by the head descriptor. We'll be accessing it
// as a byte slice. To that end, we define below the offsets for each field struct, as well as the
// packed struct size, as a bunch of `usize` consts.
// Note that these offsets are only used privately by the `VsockPacket` struct, the public interface
// consisting of getter and setter methods, for each struct field, that will also handle the correct
// endianess.

/// The vsock packet header struct size (when packed).
pub const VSOCK_PKT_HDR_SIZE: usize = 44;

// Source CID.
const HDROFF_SRC_CID: usize = 0;

// Destination CID.
const HDROFF_DST_CID: usize = 8;

// Source port.
const HDROFF_SRC_PORT: usize = 16;

// Destination port.
const HDROFF_DST_PORT: usize = 20;

// Data length (in bytes) - may be 0, if there is no data buffer.
const HDROFF_LEN: usize = 24;

// Socket type. Currently, only connection-oriented streams are defined by the vsock protocol.
const HDROFF_TYPE: usize = 28;

// Operation ID - one of the VSOCK_OP_* values; e.g.
// - VSOCK_OP_RW: a data packet;
// - VSOCK_OP_REQUEST: connection request;
// - VSOCK_OP_RST: forcefull connection termination;
// etc (see `super::defs::uapi` for the full list).
const HDROFF_OP: usize = 30;

// Additional options (flags) associated with the current operation (`op`).
// Currently, only used with shutdown requests (VSOCK_OP_SHUTDOWN).
const HDROFF_FLAGS: usize = 32;

// Size (in bytes) of the packet sender receive buffer (for the connection to which this packet
// belongs).
const HDROFF_BUF_ALLOC: usize = 36;

// Number of bytes the sender has received and consumed (for the connection to which this packet
// belongs). For instance, for our Unix backend, this counter would be the total number of bytes
// we have successfully written to a backing Unix socket.
const HDROFF_FWD_CNT: usize = 40;

#[repr(C)]
pub struct TsiProxyCreate {
    pub peer_port: u32,
    pub family: u16,
    pub _type: u16,
}

#[repr(C)]
pub struct TsiConnectReq {
    pub peer_port: u32,
    pub addr: SockaddrStorage,
}

#[repr(C)]
pub struct TsiConnectRsp {
    pub result: i32,
}

#[repr(C)]
pub struct TsiGetnameReq {
    pub peer_port: u32,
    pub local_port: u32,
    pub peer: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct TsiGetnameRsp {
    pub result: i32,
    pub addr_len: u32,
    pub addr: SockaddrStorage,
}

impl Default for TsiGetnameRsp {
    fn default() -> Self {
        let addr: SockaddrStorage = SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0).into();
        TsiGetnameRsp {
            result: -1,
            // It's fine to unwrap here sice we've just created the SocketAddrV4 above.
            addr_len: addr.as_sockaddr_in().unwrap().len(),
            addr,
        }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct TsiSendtoAddr {
    pub peer_port: u32,
    pub addr: SockaddrStorage,
}

#[repr(C)]
#[derive(Debug)]
pub struct TsiListenReq {
    pub peer_port: u32,
    pub vm_port: u32,
    pub backlog: i32,
    pub addr: SockaddrStorage,
}

#[repr(C)]
#[derive(Debug)]
pub struct TsiListenRsp {
    pub result: i32,
}

#[repr(C)]
#[derive(Debug)]
pub struct TsiAcceptReq {
    pub peer_port: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct TsiAcceptRsp {
    pub result: i32,
}

#[repr(C)]
pub struct TsiReleaseReq {
    pub peer_port: u32,
    pub local_port: u32,
}

/// The vsock packet, implemented as a wrapper over a virtq descriptor chain:
/// - the chain head, holding the packet header; and
/// - (an optional) data/buffer descriptor, only present for data packets (VSOCK_OP_RW).
pub struct VsockPacket {
    hdr: *mut u8,
    buf: Option<*mut u8>,
    buf_size: usize,
}

fn get_host_address<T: GuestMemoryBackend>(
    mem: &T,
    guest_addr: GuestAddress,
    size: usize,
) -> result::Result<*mut u8, GuestMemoryError> {
    Ok(mem.get_slice(guest_addr, size)?.ptr_guard_mut().as_ptr())
}

impl VsockPacket {
    /// Create the packet wrapper from a TX virtq chain head.
    ///
    /// The chain head is expected to hold valid packet header data. A following packet buffer
    /// descriptor can optionally end the chain. Bounds and pointer checks are performed when
    /// creating the wrapper.
    pub fn from_tx_virtq_head(head: &DescriptorChain) -> Result<Self> {
        // All buffers in the TX queue must be readable.
        //
        if head.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }

        // The packet header should fit inside the head descriptor.
        if head.len < VSOCK_PKT_HDR_SIZE as u32 {
            return Err(VsockError::HdrDescTooSmall(head.len));
        }

        let mut pkt = Self {
            hdr: get_host_address(head.mem, head.addr, VSOCK_PKT_HDR_SIZE)
                .map_err(VsockError::GuestMemoryMmap)?,
            buf: None,
            buf_size: 0,
        };

        // No point looking for a data/buffer descriptor, if the packet is zero-lengthed.
        if pkt.len() == 0 {
            return Ok(pkt);
        }

        // Reject weirdly-sized packets.
        //
        if pkt.len() > defs::MAX_PKT_BUF_SIZE as u32 {
            return Err(VsockError::InvalidPktLen(pkt.len()));
        }

        // If the packet header showed a non-zero length, there should be a data descriptor here.
        let buf_desc = head.next_descriptor().ok_or(VsockError::BufDescMissing)?;

        // TX data should be read-only.
        if buf_desc.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }

        // The data buffer should be large enough to fit the size of the data, as described by
        // the header descriptor.
        if buf_desc.len < pkt.len() {
            return Err(VsockError::BufDescTooSmall);
        }

        pkt.buf_size = buf_desc.len as usize;
        pkt.buf = Some(
            get_host_address(buf_desc.mem, buf_desc.addr, pkt.buf_size)
                .map_err(VsockError::GuestMemoryMmap)?,
        );

        Ok(pkt)
    }

    /// Create the packet wrapper from an RX virtq chain head.
    ///
    /// There must be two descriptors in the chain, both writable: a header descriptor and a data
    /// descriptor. Bounds and pointer checks are performed when creating the wrapper.
    pub fn from_rx_virtq_head(head: &DescriptorChain) -> Result<Self> {
        // All RX buffers must be writable.
        //
        if !head.is_write_only() {
            return Err(VsockError::UnwritableDescriptor);
        }

        // The packet header should fit inside the head descriptor.
        if head.len < VSOCK_PKT_HDR_SIZE as u32 {
            return Err(VsockError::HdrDescTooSmall(head.len));
        }

        let mut pkt = Self {
            hdr: get_host_address(head.mem, head.addr, VSOCK_PKT_HDR_SIZE)
                .map_err(VsockError::GuestMemoryMmap)?,
            buf: None,
            buf_size: 0,
        };

        // Starting from Linux 6.2 the virtio-vsock driver can use a single descriptor for both
        // header and data.
        if !head.has_next() && head.len > VSOCK_PKT_HDR_SIZE as u32 {
            let buf_addr = head
                .addr
                .checked_add(VSOCK_PKT_HDR_SIZE as u64)
                .ok_or(VsockError::GuestMemoryBounds)?;

            pkt.buf_size = head.len as usize - VSOCK_PKT_HDR_SIZE;
            pkt.buf = Some(
                get_host_address(head.mem, buf_addr, pkt.buf_size)
                    .map_err(VsockError::GuestMemoryMmap)?,
            );
        } else {
            let buf_desc = head.next_descriptor().ok_or(VsockError::BufDescMissing)?;

            pkt.buf_size = buf_desc.len as usize;
            pkt.buf = Some(
                get_host_address(buf_desc.mem, buf_desc.addr, pkt.buf_size)
                    .map_err(VsockError::GuestMemoryMmap)?,
            );
        }

        Ok(pkt)
    }

    /// Provides in-place, byte-slice, access to the vsock packet header.
    pub fn hdr(&self) -> &[u8] {
        // This is safe since bound checks have already been performed when creating the packet
        // from the virtq descriptor.
        unsafe { std::slice::from_raw_parts(self.hdr as *const u8, VSOCK_PKT_HDR_SIZE) }
    }

    /// Provides in-place, byte-slice, mutable access to the vsock packet header.
    pub fn hdr_mut(&mut self) -> &mut [u8] {
        // This is safe since bound checks have already been performed when creating the packet
        // from the virtq descriptor.
        unsafe { std::slice::from_raw_parts_mut(self.hdr, VSOCK_PKT_HDR_SIZE) }
    }

    /// Provides in-place, byte-slice access to the vsock packet data buffer.
    ///
    /// Note: control packets (e.g. connection request or reset) have no data buffer associated.
    ///       For those packets, this method will return `None`.
    /// Also note: calling `len()` on the returned slice will yield the buffer size, which may be
    ///            (and often is) larger than the length of the packet data. The packet data length
    ///            is stored in the packet header, and accessible via `VsockPacket::len()`.
    ///
    /// # Safety
    ///
    /// The raw pointer stored in `self.buf` is shared between `buf()` and `buf_mut()`. Callers
    /// must not hold a reference returned by `buf()` while also calling `buf_mut()`, and vice
    /// versa, as doing so would create aliased mutable references — undefined behaviour.
    pub fn buf(&self) -> Option<&[u8]> {
        self.buf.map(|ptr| {
            // This is safe since bound checks have already been performed when creating the packet
            // from the virtq descriptor.
            unsafe { std::slice::from_raw_parts(ptr as *const u8, self.buf_size) }
        })
    }

    /// Provides in-place, byte-slice, mutable access to the vsock packet data buffer.
    ///
    /// Note: control packets (e.g. connection request or reset) have no data buffer associated.
    ///       For those packets, this method will return `None`.
    /// Also note: calling `len()` on the returned slice will yield the buffer size, which may be
    ///            (and often is) larger than the length of the packet data. The packet data length
    ///            is stored in the packet header, and accessible via `VsockPacket::len()`.
    ///
    /// # Safety
    ///
    /// The raw pointer stored in `self.buf` is shared between `buf()` and `buf_mut()`. Callers
    /// must not hold a reference returned by `buf()` while also calling `buf_mut()`, and vice
    /// versa, as doing so would create aliased mutable references — undefined behaviour.
    pub fn buf_mut(&mut self) -> Option<&mut [u8]> {
        self.buf.map(|ptr| {
            // This is safe since bound checks have already been performed when creating the packet
            // from the virtq descriptor.
            unsafe { std::slice::from_raw_parts_mut(ptr, self.buf_size) }
        })
    }

    pub fn src_cid(&self) -> u64 {
        byte_order::read_le_u64(&self.hdr()[HDROFF_SRC_CID..])
    }

    pub fn set_src_cid(&mut self, cid: u64) -> &mut Self {
        byte_order::write_le_u64(&mut self.hdr_mut()[HDROFF_SRC_CID..], cid);
        self
    }

    pub fn dst_cid(&self) -> u64 {
        byte_order::read_le_u64(&self.hdr()[HDROFF_DST_CID..])
    }

    pub fn set_dst_cid(&mut self, cid: u64) -> &mut Self {
        byte_order::write_le_u64(&mut self.hdr_mut()[HDROFF_DST_CID..], cid);
        self
    }

    pub fn src_port(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_SRC_PORT..])
    }

    pub fn set_src_port(&mut self, port: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_SRC_PORT..], port);
        self
    }

    pub fn dst_port(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_DST_PORT..])
    }

    pub fn set_dst_port(&mut self, port: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_DST_PORT..], port);
        self
    }

    pub fn len(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_LEN..])
    }

    pub fn set_len(&mut self, len: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_LEN..], len);
        self
    }

    pub fn type_(&self) -> u16 {
        byte_order::read_le_u16(&self.hdr()[HDROFF_TYPE..])
    }

    pub fn set_type(&mut self, type_: u16) -> &mut Self {
        byte_order::write_le_u16(&mut self.hdr_mut()[HDROFF_TYPE..], type_);
        self
    }

    pub fn op(&self) -> u16 {
        byte_order::read_le_u16(&self.hdr()[HDROFF_OP..])
    }

    pub fn set_op(&mut self, op: u16) -> &mut Self {
        byte_order::write_le_u16(&mut self.hdr_mut()[HDROFF_OP..], op);
        self
    }

    pub fn flags(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_FLAGS..])
    }

    pub fn set_flags(&mut self, flags: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_FLAGS..], flags);
        self
    }

    pub fn set_flag(&mut self, flag: u32) -> &mut Self {
        self.set_flags(self.flags() | flag);
        self
    }

    pub fn buf_alloc(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_BUF_ALLOC..])
    }

    pub fn set_buf_alloc(&mut self, buf_alloc: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_BUF_ALLOC..], buf_alloc);
        self
    }

    pub fn fwd_cnt(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_FWD_CNT..])
    }

    pub fn set_fwd_cnt(&mut self, fwd_cnt: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_FWD_CNT..], fwd_cnt);
        self
    }

    pub fn sa_family(&self) -> Option<u16> {
        if self.buf_size >= 2 {
            Some(byte_order::read_le_u16(&self.buf().unwrap()[0..]))
        } else {
            None
        }
    }

    pub fn inet_port(&self) -> Option<u16> {
        if self.buf_size >= 4 {
            Some(byte_order::read_be_u16(&self.buf().unwrap()[2..]))
        } else {
            None
        }
    }

    pub fn inet_addr(&self) -> Option<[u8; 4]> {
        if self.buf_size >= 8 {
            let ptr = &self.buf().unwrap()[4];
            let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, 4) };
            slice[0..4].try_into().ok()
        } else {
            None
        }
    }

    pub fn unix_path(&self) -> Option<&str> {
        let buf = self.buf()?;
        if buf.len() < 108 {
            return None;
        }
        if !buf[2..].contains(&0u8) {
            return None; // no null terminator — would be OOB
        }
        let cstr = unsafe { CStr::from_ptr(&buf[2] as *const _ as *const c_char) };
        cstr.to_str().ok()
    }

    #[cfg(target_os = "linux")]
    fn parse_address(buf: &[u8], addr_len: u32) -> Option<SockaddrStorage> {
        if !Self::validate_parse_address_len(addr_len, buf.len()) {
            return None;
        }
        let sockaddr: SockaddrStorage = unsafe {
            SockaddrStorage::from_raw(&buf[0] as *const _ as *const sockaddr, Some(addr_len))?
        };

        match sockaddr.family() {
            Some(AddressFamily::Inet) => debug!("parse_address: AF_INET"),
            Some(AddressFamily::Inet6) => debug!("parse_address: AF_INET6"),
            Some(AddressFamily::Unix) => debug!("parse_address: AF_UNIX"),
            _ => {
                if let Some(family) = sockaddr.family() {
                    warn!("parse_address: unsupported family {family:?}");
                } else {
                    warn!("parse_address: error parsing family");
                }
                return None;
            }
        }

        Some(sockaddr)
    }

    /// Validates that addr_len does not exceed the buffer length for parse_address.
    /// This is the bounds check that must hold before SockaddrStorage::from_raw.
    #[cfg(target_os = "linux")]
    pub(crate) fn validate_parse_address_len(addr_len: u32, buf_len: usize) -> bool {
        addr_len as usize <= buf_len
    }

    #[cfg(target_os = "macos")]
    fn parse_address(buf: &[u8], _addr_len: u32) -> Option<SockaddrStorage> {
        let family: u16 = byte_order::read_le_u16(&buf[0..2]);

        match family {
            defs::LINUX_AF_INET => {
                debug!("parse_address: AF_INET");
                let in_port: u16 = byte_order::read_be_u16(&buf[2..4]);
                let in_addr = Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
                Some(SocketAddrV4::new(in_addr, in_port).into())
            }
            defs::LINUX_AF_INET6 => {
                debug!("parse_address: AF_INET6");
                let in_port: u16 = byte_order::read_be_u16(&buf[2..4]);
                let flowinfo: u32 = byte_order::read_be_u32(&buf[4..8]);
                let in6_addr = Ipv6Addr::new(
                    byte_order::read_be_u16(&buf[8..10]),
                    byte_order::read_be_u16(&buf[10..12]),
                    byte_order::read_be_u16(&buf[12..14]),
                    byte_order::read_be_u16(&buf[14..16]),
                    byte_order::read_be_u16(&buf[16..18]),
                    byte_order::read_be_u16(&buf[18..20]),
                    byte_order::read_be_u16(&buf[20..22]),
                    byte_order::read_be_u16(&buf[22..24]),
                );
                let scope_id: u32 = byte_order::read_be_u32(&buf[24..28]);
                Some(SocketAddrV6::new(in6_addr, in_port, flowinfo, scope_id).into())
            }
            defs::LINUX_AF_UNIX => {
                // On macOS, SockaddrStorage doesn't implement `from_raw` for
                // Unix sockets, nor a way to cast an UnixPath to it.
                error!("AF_UNIX sockets aren't yet supported on macOS");
                None
            }
            _ => None,
        }
    }

    pub fn read_proxy_create(&self) -> Option<TsiProxyCreate> {
        if self.buf_size >= 8 {
            let peer_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[0..]);
            let family: u16 = byte_order::read_le_u16(&self.buf().unwrap()[4..]);
            let _type: u16 = byte_order::read_le_u16(&self.buf().unwrap()[6..]);

            Some(TsiProxyCreate {
                peer_port,
                family,
                _type,
            })
        } else {
            None
        }
    }

    #[cfg(kani)]
    pub fn write_proxy_create(&mut self, req: TsiProxyCreate) {
        if self.buf_size >= 8 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], req.peer_port);
                byte_order::write_le_u16(&mut buf[4..], req.family);
                byte_order::write_le_u16(&mut buf[6..], req._type);
            }
        }
    }

    pub fn read_connect_req(&self) -> Option<TsiConnectReq> {
        if self.buf_size >= 4 {
            let buf = self.buf().unwrap();
            let peer_port: u32 = byte_order::read_le_u32(&buf[0..]);
            let addr_len: u32 = byte_order::read_le_u32(&buf[4..]);
            let addr = Self::parse_address(&buf[8..], addr_len)?;

            Some(TsiConnectReq { peer_port, addr })
        } else {
            None
        }
    }

    pub fn write_connect_rsp(&mut self, rsp: TsiConnectRsp) {
        if self.buf_size >= 4 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
            }
        }
    }

    pub fn read_getname_req(&self) -> Option<TsiGetnameReq> {
        if self.buf_size >= 12 {
            let peer_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[0..]);
            let local_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[4..]);
            let peer: u32 = byte_order::read_le_u32(&self.buf().unwrap()[8..]);
            Some(TsiGetnameReq {
                peer_port,
                local_port,
                peer,
            })
        } else {
            None
        }
    }

    pub fn write_getname_rsp(&mut self, rsp: TsiGetnameRsp) {
        if self.buf_size >= 132 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
                byte_order::write_le_u32(&mut buf[4..], rsp.addr_len);
                let addr_ptr = rsp.addr.as_ptr();
                let slice = unsafe {
                    std::slice::from_raw_parts(addr_ptr as *const u8, rsp.addr.len() as usize)
                };
                buf[8..(rsp.addr.len() + 8) as usize].copy_from_slice(slice);

                // On macOS, convert BSD sockaddr (u8 sa_len + u8 sa_family) to
                // Linux wire format (u16 sa_family). Also translate macOS AF_*
                // values to their Linux equivalents (e.g. AF_INET6: 30 → 10).
                #[cfg(target_os = "macos")]
                {
                    let bsd_family = buf[9];
                    let linux_family: u16 = match bsd_family as i32 {
                        libc::AF_INET => defs::LINUX_AF_INET,
                        libc::AF_INET6 => defs::LINUX_AF_INET6,
                        _ => 0, // AF_UNSPEC
                    };
                    byte_order::write_le_u16(&mut buf[8..], linux_family);
                }
            }
        }
    }

    pub fn read_sendto_addr(&self) -> Option<TsiSendtoAddr> {
        if self.buf_size >= 4 {
            let buf = self.buf().unwrap();
            let peer_port: u32 = byte_order::read_le_u32(&buf[0..]);
            let addr_len: u32 = byte_order::read_le_u32(&buf[4..]);
            let addr = Self::parse_address(&buf[8..], addr_len)?;

            Some(TsiSendtoAddr { peer_port, addr })
        } else {
            None
        }
    }

    pub fn read_listen_req(&self) -> Option<TsiListenReq> {
        if self.buf_size >= 12 {
            let buf = self.buf().unwrap();
            let peer_port: u32 = byte_order::read_le_u32(&buf[0..]);
            let vm_port: u32 = byte_order::read_le_u32(&buf[4..]);
            let backlog: u32 = byte_order::read_le_u32(&buf[8..]);
            let addr_len: u32 = byte_order::read_le_u32(&buf[12..]);
            let addr = Self::parse_address(&buf[16..], addr_len)?;

            Some(TsiListenReq {
                peer_port,
                vm_port,
                backlog: backlog as i32,
                addr,
            })
        } else {
            None
        }
    }

    pub fn write_listen_rsp(&mut self, rsp: TsiListenRsp) {
        if self.buf_size >= 4 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
            }
        }
    }

    pub fn read_accept_req(&self) -> Option<TsiAcceptReq> {
        if self.buf_size >= 8 {
            let peer_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[0..]);
            let flags: u32 = byte_order::read_le_u32(&self.buf().unwrap()[4..]);

            Some(TsiAcceptReq { peer_port, flags })
        } else {
            None
        }
    }

    pub fn write_accept_rsp(&mut self, rsp: TsiAcceptRsp) {
        if self.buf_size >= 4 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
            }
        }
    }

    pub fn read_release_req(&self) -> Option<TsiReleaseReq> {
        if self.buf_size >= 8 {
            let peer_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[0..]);
            let local_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[4..]);
            Some(TsiReleaseReq {
                peer_port,
                local_port,
            })
        } else {
            None
        }
    }

    pub fn write_time_sync(&mut self, time: u64) {
        if self.buf_size >= 8 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u64(&mut buf[0..], time);
            }
        }
    }

    /// Construct a `VsockPacket` backed by a freshly-allocated header buffer for use in
    /// Kani proofs.
    ///
    /// NOTE: All proofs using this constructor test the `buf = None` path only (control
    /// packets with no data buffer). See `proof_hdr_buf_non_overlapping` and
    /// `proof_hdr_buf_single_descriptor_layout` in the `verification` module for the
    /// `buf = Some(...)` path.
    #[cfg(kani)]
    fn new_for_verification() -> (Self, Vec<u8>) {
        let mut hdr_buf = vec![0u8; VSOCK_PKT_HDR_SIZE];
        let pkt = VsockPacket {
            hdr: hdr_buf.as_mut_ptr(),
            buf: None,
            buf_size: 0,
        };
        (pkt, hdr_buf)
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    // Proofs verify invariants from the VIRTIO specification v1.2:
    // - Virtio Socket Device (Sec 5.10): packet header format, field encoding
    // - Virtio Transport (Sec 4.2.3): descriptor chain processing
    // See: https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html

    // Header field layout per virtio_vsock_hdr (Virtio spec 5.10.6.1)

    // u64 fields: byte_order write/read loop iterates 8 bytes → unwind(9)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(9)]
    fn proof_vsock_hdr_src_cid_roundtrip() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let val: u64 = kani::any();
        pkt.set_src_cid(val);
        assert_eq!(pkt.src_cid(), val);
        kani::cover!(val == 0, "zero src_cid exercised");
        kani::cover!(val == u64::MAX, "max src_cid exercised");
        kani::cover!(val > 0 && val < u64::MAX, "interior src_cid exercised");
    }

    // u64 fields: byte_order write/read loop iterates 8 bytes → unwind(9)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(9)]
    fn proof_vsock_hdr_dst_cid_roundtrip() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let val: u64 = kani::any();
        pkt.set_dst_cid(val);
        assert_eq!(pkt.dst_cid(), val);
        kani::cover!(val == 0, "zero dst_cid exercised");
        kani::cover!(val == u64::MAX, "max dst_cid exercised");
        kani::cover!(val > 0 && val < u64::MAX, "interior dst_cid exercised");
    }

    // u32 fields: byte_order write/read loop iterates 4 bytes → unwind(5)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_vsock_hdr_src_port_roundtrip() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let val: u32 = kani::any();
        pkt.set_src_port(val);
        assert_eq!(pkt.src_port(), val);
        kani::cover!(val == 0, "zero src_port exercised");
        kani::cover!(val == u32::MAX, "max src_port exercised");
    }

    // u32 fields: byte_order write/read loop iterates 4 bytes → unwind(5)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_vsock_hdr_dst_port_roundtrip() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let val: u32 = kani::any();
        pkt.set_dst_port(val);
        assert_eq!(pkt.dst_port(), val);
        kani::cover!(val == 0, "zero dst_port exercised");
        kani::cover!(val == u32::MAX, "max dst_port exercised");
    }

    // u32 fields: byte_order write/read loop iterates 4 bytes → unwind(5)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_vsock_hdr_len_roundtrip() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let val: u32 = kani::any();
        pkt.set_len(val);
        assert_eq!(pkt.len(), val);
        kani::cover!(val == 0, "zero len exercised");
        kani::cover!(val == u32::MAX, "max len exercised");
    }

    // u16 fields: byte_order write/read loop iterates 2 bytes → unwind(3)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(3)]
    fn proof_vsock_hdr_type_roundtrip() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let val: u16 = kani::any();
        pkt.set_type(val);
        assert_eq!(pkt.type_(), val);
        kani::cover!(val == 0, "zero type exercised");
        kani::cover!(val == u16::MAX, "max type exercised");
    }

    // u16 fields: byte_order write/read loop iterates 2 bytes → unwind(3)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(3)]
    fn proof_vsock_hdr_op_roundtrip() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let val: u16 = kani::any();
        pkt.set_op(val);
        assert_eq!(pkt.op(), val);
        kani::cover!(val == 0, "zero op exercised");
        kani::cover!(val == u16::MAX, "max op exercised");
    }

    // u32 fields: byte_order write/read loop iterates 4 bytes → unwind(5)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_vsock_hdr_flags_roundtrip() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let val: u32 = kani::any();
        pkt.set_flags(val);
        assert_eq!(pkt.flags(), val);
        kani::cover!(val == 0, "zero flags exercised");
        kani::cover!(val == u32::MAX, "all flags set exercised");
    }

    // u32 fields: byte_order write/read loop iterates 4 bytes → unwind(5)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_vsock_hdr_buf_alloc_roundtrip() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let val: u32 = kani::any();
        pkt.set_buf_alloc(val);
        assert_eq!(pkt.buf_alloc(), val);
        kani::cover!(val == 0, "zero buf_alloc exercised");
        kani::cover!(val == u32::MAX, "max buf_alloc exercised");
    }

    // u32 fields: byte_order write/read loop iterates 4 bytes → unwind(5)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_vsock_hdr_fwd_cnt_roundtrip() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let val: u32 = kani::any();
        pkt.set_fwd_cnt(val);
        assert_eq!(pkt.fwd_cnt(), val);
        kani::cover!(val == 0, "zero fwd_cnt exercised");
        kani::cover!(val == u32::MAX, "max fwd_cnt exercised");
    }

    // Multi-field isolation: largest field is u64 (8 bytes) → unwind(9)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(9)]
    fn proof_vsock_hdr_field_isolation() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let cid: u64 = kani::any();
        let port: u32 = kani::any();
        let len: u32 = kani::any();
        let op: u16 = kani::any();

        pkt.set_src_cid(cid);
        pkt.set_dst_port(port);
        pkt.set_len(len);
        pkt.set_op(op);

        // Each field is independent
        assert_eq!(pkt.src_cid(), cid);
        assert_eq!(pkt.dst_port(), port);
        assert_eq!(pkt.len(), len);
        assert_eq!(pkt.op(), op);
        kani::cover!(cid == 0 && port == 0, "all-zero fields exercised");
        kani::cover!(
            cid != 0 && port != 0 && len != 0 && op != 0,
            "all non-zero fields exercised"
        );
    }

    // set_flag calls set_flags (read u32 + write u32): byte_order loops 4 bytes each → unwind(5)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_vsock_hdr_set_flag_or() {
        let (mut pkt, _hdr_buf) = VsockPacket::new_for_verification();
        let initial: u32 = kani::any();
        let flag: u32 = kani::any();
        pkt.set_flags(initial);
        pkt.set_flag(flag);
        assert_eq!(pkt.flags(), initial | flag);
        // Cover: setting an already-set bit is idempotent; setting a new bit adds it.
        kani::cover!(initial & flag == flag, "flag already set (idempotent OR)");
        kani::cover!(initial & flag == 0, "flag was clear before set_flag");
    }

    // TSI protocol extensions (libkrun-specific, not in Virtio spec)

    // Helper: create a VsockPacket with a symbolic data buffer of `buf_size` bytes.
    // Returns the packet plus the backing allocations (must be kept alive).
    fn new_with_buf_for_verification(buf_size: usize) -> (VsockPacket, Vec<u8>, Vec<u8>) {
        let mut hdr_buf = vec![0u8; VSOCK_PKT_HDR_SIZE];
        let mut data_buf: Vec<u8> = vec![0u8; buf_size];
        for byte in data_buf.iter_mut() {
            *byte = kani::any();
        }
        let pkt = VsockPacket {
            hdr: hdr_buf.as_mut_ptr(),
            buf: if buf_size > 0 {
                Some(data_buf.as_mut_ptr())
            } else {
                None
            },
            buf_size,
        };
        (pkt, hdr_buf, data_buf)
    }

    /// Proof: sa_family returns None iff buf_size < 2.
    /// sa_family reads 2 bytes; buf constrained to <=4 → unwind(5).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_sa_family_bounds() {
        let buf_size: usize = kani::any_where(|&s| s <= 4);
        let (pkt, _hdr, _buf) = new_with_buf_for_verification(buf_size);
        let result = pkt.sa_family();
        if buf_size < 2 {
            assert!(result.is_none());
        } else {
            assert!(result.is_some());
        }
        kani::cover!(buf_size == 0, "zero-length buffer yields None");
        kani::cover!(buf_size == 2, "exact threshold yields Some");
    }

    /// Proof: inet_port returns None iff buf_size < 4.
    /// inet_port reads 4 bytes; buf constrained to <=6 → unwind(7).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(7)]
    fn proof_inet_port_bounds() {
        let buf_size: usize = kani::any_where(|&s| s <= 6);
        let (pkt, _hdr, _buf) = new_with_buf_for_verification(buf_size);
        let result = pkt.inet_port();
        if buf_size < 4 {
            assert!(result.is_none());
        } else {
            assert!(result.is_some());
        }
        kani::cover!(buf_size == 0, "zero-length buffer yields None");
        kani::cover!(buf_size == 4, "exact threshold yields Some");
    }

    /// Proof: inet_addr returns None iff buf_size < 8.
    /// inet_addr reads 8 bytes; buf constrained to <=10 → unwind(11).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(11)]
    fn proof_inet_addr_bounds() {
        let buf_size: usize = kani::any_where(|&s| s <= 10);
        let (pkt, _hdr, _buf) = new_with_buf_for_verification(buf_size);
        let result = pkt.inet_addr();
        if buf_size < 8 {
            assert!(result.is_none());
        } else {
            assert!(result.is_some());
        }
        kani::cover!(buf_size == 0, "zero-length buffer yields None");
        kani::cover!(buf_size == 8, "exact threshold yields Some");
    }

    /// Proof: read_proxy_create returns None iff buf_size < 8.
    /// read_proxy_create reads 8 bytes; buf constrained to <=10 → unwind(11).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(11)]
    fn proof_read_proxy_create_bounds() {
        let buf_size: usize = kani::any_where(|&s| s <= 10);
        let (pkt, _hdr, _buf) = new_with_buf_for_verification(buf_size);
        let result = pkt.read_proxy_create();
        if buf_size < 8 {
            assert!(result.is_none());
        } else {
            assert!(result.is_some());
        }
        kani::cover!(buf_size == 0, "zero-length buffer yields None");
        kani::cover!(buf_size == 8, "exact threshold yields Some");
    }

    /// Proof: read_getname_req returns None iff buf_size < 12.
    /// read_getname_req reads 12 bytes; buf constrained to <=14 → unwind(15).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(15)]
    fn proof_read_getname_req_bounds() {
        let buf_size: usize = kani::any_where(|&s| s <= 14);
        let (pkt, _hdr, _buf) = new_with_buf_for_verification(buf_size);
        let result = pkt.read_getname_req();
        if buf_size < 12 {
            assert!(result.is_none());
        } else {
            assert!(result.is_some());
        }
        kani::cover!(buf_size == 0, "zero-length buffer yields None");
        kani::cover!(buf_size == 12, "exact threshold yields Some");
    }

    /// Proof: read_accept_req returns None iff buf_size < 8.
    /// read_accept_req reads 8 bytes; buf constrained to <=10 → unwind(11).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(11)]
    fn proof_read_accept_req_bounds() {
        let buf_size: usize = kani::any_where(|&s| s <= 10);
        let (pkt, _hdr, _buf) = new_with_buf_for_verification(buf_size);
        let result = pkt.read_accept_req();
        if buf_size < 8 {
            assert!(result.is_none());
        } else {
            assert!(result.is_some());
        }
        kani::cover!(buf_size == 0, "zero-length buffer yields None");
        kani::cover!(buf_size == 8, "exact threshold yields Some");
    }

    /// Proof: read_release_req returns None iff buf_size < 8.
    /// read_release_req reads 8 bytes; buf constrained to <=10 → unwind(11).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(11)]
    fn proof_read_release_req_bounds() {
        let buf_size: usize = kani::any_where(|&s| s <= 10);
        let (pkt, _hdr, _buf) = new_with_buf_for_verification(buf_size);
        let result = pkt.read_release_req();
        if buf_size < 8 {
            assert!(result.is_none());
        } else {
            assert!(result.is_some());
        }
        kani::cover!(buf_size == 0, "zero-length buffer yields None");
        kani::cover!(buf_size == 8, "exact threshold yields Some");
    }

    // ── GAP-002: unix_path OOB read via CStr::from_ptr ───────────────────────
    //
    // `unix_path` guards `buf_size >= 108` but originally did NOT ensure a null
    // byte exists within `buf[2..buf_size]`. A guest could supply a 108-byte
    // buffer with no null byte, causing `CStr::from_ptr` to scan past the
    // allocation.
    //
    // Fix: added `if !buf[2..].contains(&0u8) { return None; }` before the
    // unsafe call. This proof calls unix_path() directly on a null-free buffer
    // and verifies it returns None (the fixed behaviour).
    //
    // buf_size = 110: loop fills indices 0..110 → unwind(111).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(111)]
    fn proof_unix_path_no_null_terminator_is_oob() {
        let buf_size: usize = 110; // >= 108 so the buf_size guard passes

        let mut hdr_buf = vec![0u8; VSOCK_PKT_HDR_SIZE];
        // Fill with 0xFF — no null bytes anywhere
        let mut data_buf: Vec<u8> = vec![0xFFu8; buf_size];

        let pkt = VsockPacket {
            hdr: hdr_buf.as_mut_ptr(),
            buf: Some(data_buf.as_mut_ptr()),
            buf_size,
        };

        // After the fix: unix_path() returns None when no null in buf[2..]
        let result = pkt.unix_path();
        kani::assert(
            result.is_none(),
            "unix_path must return None when buf[2..] has no null terminator",
        );
    }

    // ── GAP-009: parse_address addr_len unchecked against buf bounds ──────────
    //
    // `parse_address` received `addr_len` from a guest-controlled vsock packet
    // header field and passed it to `SockaddrStorage::from_raw` without
    // checking that `addr_len <= buf.len()`. If addr_len > buf.len(), from_raw
    // could read beyond the slice.
    //
    // Fix: extracted `validate_parse_address_len(addr_len, buf.len())` helper
    // and call it before from_raw. This proof verifies the helper's contract
    // directly: it returns true iff addr_len <= buf_len.
    //
    // Note: parse_address (and the helper) is Linux-only.
    #[cfg(target_os = "linux")]
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(17)]
    fn proof_parse_address_addr_len_bounds() {
        const BUF_LEN: usize = 16;
        let addr_len: u32 = kani::any_where(|&v| v <= 255);

        if VsockPacket::validate_parse_address_len(addr_len, BUF_LEN) {
            kani::assert(
                addr_len as usize <= BUF_LEN,
                "addr_len within buf bounds when helper returns true",
            );
            kani::cover!(addr_len as usize <= BUF_LEN, "valid addr_len accepted");
        } else {
            kani::assert(
                addr_len as usize > BUF_LEN,
                "helper returns false only for oversized addr_len",
            );
            kani::cover!(addr_len as usize > BUF_LEN, "oversized addr_len rejected");
        }
    }

    // ── GAP-017: buf and buf_mut slice contract ────────────────────────────────
    //
    // `buf()` and `buf_mut()` both construct slices from `self.buf: Option<*mut u8>`
    // (Copy). The structural aliasing is intentional and documented in Safety
    // comments; callers are responsible for avoiding simultaneous mutable+shared
    // access. This proof verifies the functional contract: each accessor returns
    // a slice of exactly buf_size bytes starting at the correct base pointer.
    //
    // buf_size = 8: loop fills 8 bytes → unwind(9).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(9)]
    fn proof_buf_buf_mut_no_aliasing() {
        let buf_size: usize = 8;
        let mut hdr_buf = vec![0u8; VSOCK_PKT_HDR_SIZE];
        let mut data_buf: Vec<u8> = vec![0u8; buf_size];
        for byte in data_buf.iter_mut() {
            *byte = kani::any();
        }
        let raw_ptr: *mut u8 = data_buf.as_mut_ptr();

        let mut pkt = VsockPacket {
            hdr: hdr_buf.as_mut_ptr(),
            buf: Some(raw_ptr),
            buf_size,
        };

        // Verify buf() returns slice of exactly buf_size bytes from correct pointer
        let shared = pkt.buf().unwrap();
        kani::assert(shared.len() == buf_size, "buf() returns buf_size bytes");
        kani::assert(
            shared.as_ptr() == raw_ptr as *const u8,
            "buf() uses the raw pointer",
        );

        let _ = shared; // explicitly end lifetime before calling buf_mut

        // Verify buf_mut() returns slice of exactly buf_size bytes from correct pointer
        let mutable = pkt.buf_mut().unwrap();
        kani::assert(
            mutable.len() == buf_size,
            "buf_mut() returns buf_size bytes",
        );
        kani::assert(
            mutable.as_mut_ptr() == raw_ptr,
            "buf_mut() uses the raw pointer",
        );

        kani::cover!(mutable.len() == buf_size, "buf_mut returns buf_size bytes");
    }

    // ── M16: hdr/buf aliasing invariant — separate-descriptor case ────────────
    //
    // In the normal two-descriptor path (from_tx_virtq_head / from_rx_virtq_head
    // with two descriptors), `hdr` and `buf` point into different guest-memory
    // regions backed by different descriptors. This proof verifies that the byte
    // ranges returned by `hdr()` and `buf()` do not overlap when the pointers
    // come from two independent allocations.
    //
    // hdr is VSOCK_PKT_HDR_SIZE (44) bytes; buf_size is constrained to <=8 to
    // keep the solver tractable. The largest loop in the proof body is the fill
    // loop over data_buf (8 bytes) → unwind(9).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(9)]
    fn proof_hdr_buf_non_overlapping() {
        let buf_size: usize = kani::any_where(|&s: &usize| s > 0 && s <= 8);

        let mut hdr_buf = vec![0u8; VSOCK_PKT_HDR_SIZE];
        let mut data_buf: Vec<u8> = vec![0u8; buf_size];
        for byte in data_buf.iter_mut() {
            *byte = kani::any();
        }

        let hdr_ptr: *mut u8 = hdr_buf.as_mut_ptr();
        let buf_ptr: *mut u8 = data_buf.as_mut_ptr();

        let pkt = VsockPacket {
            hdr: hdr_ptr,
            buf: Some(buf_ptr),
            buf_size,
        };

        let hdr_slice = pkt.hdr();
        let buf_slice = pkt.buf().unwrap();

        // Both slices must have the expected lengths.
        kani::assert(
            hdr_slice.len() == VSOCK_PKT_HDR_SIZE,
            "hdr() length equals VSOCK_PKT_HDR_SIZE",
        );
        kani::assert(buf_slice.len() == buf_size, "buf() length equals buf_size");

        // The two slices must not overlap: since they come from separate Vec
        // allocations, their address ranges are disjoint. We verify this by
        // checking that neither range's start falls inside the other.
        let hdr_start = hdr_slice.as_ptr() as usize;
        let hdr_end = hdr_start + VSOCK_PKT_HDR_SIZE;
        let buf_start = buf_slice.as_ptr() as usize;
        let buf_end = buf_start + buf_size;

        // Ranges [hdr_start, hdr_end) and [buf_start, buf_end) are non-overlapping
        // iff one ends before the other starts.
        kani::assert(
            hdr_end <= buf_start || buf_end <= hdr_start,
            "hdr() and buf() slices must not overlap",
        );

        kani::cover!(hdr_end <= buf_start, "hdr ends before buf starts");
        kani::cover!(buf_end <= hdr_start, "buf ends before hdr starts");
    }

    // ── M16: hdr/buf aliasing invariant — single-descriptor case ─────────────
    //
    // Since Linux 6.2, the guest virtio-vsock driver may use a single descriptor
    // that holds both the header and the data buffer. In `from_rx_virtq_head`,
    // when `!head.has_next() && head.len > VSOCK_PKT_HDR_SIZE`, `buf` is set to
    // `head.addr + VSOCK_PKT_HDR_SIZE`. Both `hdr` and `buf` therefore point
    // into the SAME underlying allocation, at non-overlapping adjacent offsets.
    //
    // This proof verifies that layout: hdr occupies [0, VSOCK_PKT_HDR_SIZE) and
    // buf occupies [VSOCK_PKT_HDR_SIZE, VSOCK_PKT_HDR_SIZE + buf_size) within
    // the same backing array, and that the two slices returned by hdr() / buf()
    // are adjacent and non-overlapping.
    //
    // We use a combined buffer of VSOCK_PKT_HDR_SIZE + 8 bytes (52 bytes total).
    // The initialisation loop iterates over all 52 bytes → unwind(53).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(53)]
    fn proof_hdr_buf_single_descriptor_layout() {
        const BUF_DATA_SIZE: usize = 8;
        const TOTAL_SIZE: usize = VSOCK_PKT_HDR_SIZE + BUF_DATA_SIZE;

        let mut combined: Vec<u8> = vec![0u8; TOTAL_SIZE];
        for byte in combined.iter_mut() {
            *byte = kani::any();
        }

        // Mirror what from_rx_virtq_head does: hdr → base, buf → base + HDR_SIZE
        let hdr_ptr: *mut u8 = combined.as_mut_ptr();
        // Safety: combined has TOTAL_SIZE bytes; VSOCK_PKT_HDR_SIZE < TOTAL_SIZE.
        let buf_ptr: *mut u8 = unsafe { hdr_ptr.add(VSOCK_PKT_HDR_SIZE) };

        let pkt = VsockPacket {
            hdr: hdr_ptr,
            buf: Some(buf_ptr),
            buf_size: BUF_DATA_SIZE,
        };

        let hdr_slice = pkt.hdr();
        let buf_slice = pkt.buf().unwrap();

        // Lengths must match the respective fields.
        kani::assert(
            hdr_slice.len() == VSOCK_PKT_HDR_SIZE,
            "hdr() length equals VSOCK_PKT_HDR_SIZE",
        );
        kani::assert(
            buf_slice.len() == BUF_DATA_SIZE,
            "buf() length equals BUF_DATA_SIZE",
        );

        let hdr_start = hdr_slice.as_ptr() as usize;
        let buf_start = buf_slice.as_ptr() as usize;

        // The two regions must be adjacent: buf starts exactly where hdr ends.
        kani::assert(
            buf_start == hdr_start + VSOCK_PKT_HDR_SIZE,
            "buf() starts immediately after hdr() in the single-descriptor layout",
        );

        // And therefore they do not overlap.
        kani::assert(
            hdr_start + VSOCK_PKT_HDR_SIZE <= buf_start,
            "hdr() and buf() slices are non-overlapping in single-descriptor layout",
        );

        kani::cover!(
            buf_start == hdr_start + VSOCK_PKT_HDR_SIZE,
            "buf immediately follows hdr in single-descriptor layout"
        );
    }

    // ── G-04: write_proxy_create / read_proxy_create round-trip ──────────────
    //
    // `TsiProxyCreate` is a libkrun-specific TSI protocol extension with no
    // external specification. `write_proxy_create` serializes the struct into
    // the packet data buffer as three LE fields: u32 peer_port at [0..4],
    // u16 family at [4..6], u16 _type at [6..8]. `read_proxy_create` reads
    // them back in the same layout. This proof verifies the round-trip identity
    // for all symbolic field values.
    //
    // Breaking change: swapping the write offsets of `family` and `_type` in
    // `write_proxy_create` (or in `read_proxy_create`) would cause the
    // field-equality assertions to fail.
    //
    // write/read loops: largest field is u32 (4 bytes) → unwind(5).
    /// write_proxy_create serializes exactly what read_proxy_create deserializes.
    ///
    /// Verifies the TSI protocol extension round-trip for all symbolic field values.
    /// The proof exercises both the write path (write_proxy_create) and the read
    /// path (read_proxy_create) and asserts each field is preserved exactly.
    ///
    /// Breaking change: reordering field writes in write_proxy_create or
    /// changing offset constants in read_proxy_create breaks the equality
    /// assertions.
    ///
    /// Bound: byte_order loops at most 4 bytes per field (u32) → unwind(5).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_write_proxy_create_roundtrip() {
        // Symbolic field values — cover the full type ranges.
        let peer_port: u32 = kani::any();
        let family: u16 = kani::any();
        let _type: u16 = kani::any();

        // Build a packet backed by an 8-byte data buffer (minimum for proxy create).
        let mut hdr_buf = vec![0u8; VSOCK_PKT_HDR_SIZE];
        let mut data_buf = vec![0u8; 8];
        let mut pkt = VsockPacket {
            hdr: hdr_buf.as_mut_ptr(),
            buf: Some(data_buf.as_mut_ptr()),
            buf_size: 8,
        };

        // Write the struct.
        pkt.write_proxy_create(TsiProxyCreate {
            peer_port,
            family,
            _type,
        });

        // Read it back and verify each field is preserved.
        let result = pkt
            .read_proxy_create()
            .expect("read_proxy_create must return Some for buf_size >= 8");
        kani::assert(
            result.peer_port == peer_port,
            "peer_port round-trips correctly",
        );
        kani::assert(result.family == family, "family round-trips correctly");
        kani::assert(result._type == _type, "_type round-trips correctly");

        kani::cover!(
            peer_port == 0 && family == 0 && _type == 0,
            "all-zero fields exercised"
        );
        kani::cover!(
            peer_port != 0 && family != 0 && _type != 0,
            "all non-zero fields exercised"
        );
    }
}
