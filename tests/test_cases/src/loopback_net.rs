#![cfg(feature = "host")]

use std::io;
use bytes::Bytes;
use pnet::packet::ethernet::{EthernetPacket, EtherTypes, MutableEthernetPacket};
use pnet::packet::ipv4::{Ipv4Packet, MutableIpv4Packet};
use pnet::packet::arp::{ArpPacket, MutableArpPacket, ArpOperations};
use pnet::packet::icmp::{IcmpPacket, IcmpTypes, MutableIcmpPacket};
use pnet::packet::Packet;
use pnet::util::MacAddr;
use std::net::Ipv4Addr;
use tokio::sync::mpsc;

use krun::{
    AsyncNetBackend, AsyncNetBackendFactory, NetBackendHandle, NetSendBoxFuture,
};

pub struct LoopbackFactory;

impl LoopbackFactory {
    pub fn new() -> Self {
        Self
    }
}

impl AsyncNetBackendFactory for LoopbackFactory {
    fn create(self: Box<Self>) -> NetSendBoxFuture<'static, io::Result<NetBackendHandle>> {
        Box::pin(async {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let backend = LoopbackBackend::new(tx);
            Ok(NetBackendHandle {
                backend: Box::new(backend),
                to_guest_rx: rx,
                wake_rx: None,
            })
        })
    }
}

pub struct LoopbackBackend {
    to_guest: mpsc::Sender<Bytes>,
}

impl LoopbackBackend {
    fn backend_mac() -> MacAddr {
        MacAddr(0x02, 0x00, 0x00, 0x00, 0x00, 0x01)
    }

    const BACKEND_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 100, 1);

    fn new(to_guest: mpsc::Sender<Bytes>) -> Self {
        Self { to_guest }
    }

    fn icmp_checksum(data: &[u8]) -> u16 {
        let mut sum: u32 = 0;
        let mut i = 0;
        while i + 1 < data.len() {
            sum += ((data[i] as u32) << 8) | (data[i + 1] as u32);
            i += 2;
        }
        if i < data.len() {
            sum += (data[i] as u32) << 8;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !sum as u16
    }

    fn ipv4_checksum(data: &[u8]) -> u16 {
        let mut sum: u32 = 0;
        let mut i = 0;
        while i + 1 < data.len() {
            sum += ((data[i] as u32) << 8) | (data[i + 1] as u32);
            i += 2;
        }
        if i < data.len() {
            sum += (data[i] as u32) << 8;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !sum as u16
    }
}

impl AsyncNetBackend for LoopbackBackend {
    fn handle_guest_tx(&mut self, packet: &[u8]) {
        if let Some(eth) = EthernetPacket::new(packet) {
            match eth.get_ethertype() {
                EtherTypes::Arp => {
                    // Handle ARP requests
                    if let Some(arp) = ArpPacket::new(eth.payload()) {
                        if arp.get_operation() == ArpOperations::Request {
                            let target_proto_addr = arp.get_target_proto_addr();
                            if target_proto_addr == Self::BACKEND_IP {
                                // Build ARP reply
                                let mut reply_buf = vec![0u8; 14 + 28];

                                // Ethernet header
                                {
                                    let mut eth_reply = MutableEthernetPacket::new(&mut reply_buf[..14])
                                        .unwrap();
                                    eth_reply.set_destination(eth.get_source());
                                    eth_reply.set_source(Self::backend_mac());
                                    eth_reply.set_ethertype(EtherTypes::Arp);
                                }

                                // ARP payload
                                {
                                    let mut arp_reply = MutableArpPacket::new(&mut reply_buf[14..])
                                        .unwrap();
                                    arp_reply.set_hardware_type(arp.get_hardware_type());
                                    arp_reply.set_protocol_type(arp.get_protocol_type());
                                    arp_reply.set_hw_addr_len(arp.get_hw_addr_len());
                                    arp_reply.set_proto_addr_len(arp.get_proto_addr_len());
                                    arp_reply.set_operation(ArpOperations::Reply);
                                    arp_reply.set_sender_hw_addr(Self::backend_mac());
                                    arp_reply.set_sender_proto_addr(Self::BACKEND_IP);
                                    arp_reply.set_target_hw_addr(arp.get_sender_hw_addr());
                                    arp_reply.set_target_proto_addr(arp.get_sender_proto_addr());
                                }

                                let _ = self.to_guest.try_send(Bytes::from(reply_buf));
                            }
                        }
                    }
                }
                EtherTypes::Ipv4 => {
                    // Handle ICMP echo requests
                    if let Some(ipv4) = Ipv4Packet::new(eth.payload()) {
                        if ipv4.get_next_level_protocol() == pnet::packet::ip::IpNextHeaderProtocols::Icmp
                            && ipv4.get_destination() == Self::BACKEND_IP
                        {
                            if let Some(icmp) = IcmpPacket::new(ipv4.payload()) {
                                if icmp.get_icmp_type() == IcmpTypes::EchoRequest {
                                    // Build full ICMP echo reply
                                    let mut reply_buf = packet.to_vec();

                                    // Swap Ethernet MACs
                                    {
                                        let mut eth_reply =
                                            MutableEthernetPacket::new(&mut reply_buf).unwrap();
                                        let src = eth_reply.get_source();
                                        let dst = eth_reply.get_destination();
                                        eth_reply.set_source(dst);
                                        eth_reply.set_destination(src);
                                    }

                                    // Modify IPv4 header (starting at offset 14)
                                    let ipv4_len = ((reply_buf[14] & 0x0f) as usize * 4);
                                    {
                                        let mut ipv4_reply =
                                            MutableIpv4Packet::new(&mut reply_buf[14..]).unwrap();
                                        let src = ipv4_reply.get_source();
                                        let dst = ipv4_reply.get_destination();
                                        ipv4_reply.set_source(dst);
                                        ipv4_reply.set_destination(src);
                                        ipv4_reply.set_ttl(64);
                                        ipv4_reply.set_checksum(0);
                                    }
                                    // Compute IPv4 checksum on a copy
                                    let ipv4_cksum = Self::ipv4_checksum(&reply_buf[14..14 + ipv4_len]);
                                    {
                                        let mut ipv4_reply =
                                            MutableIpv4Packet::new(&mut reply_buf[14..]).unwrap();
                                        ipv4_reply.set_checksum(ipv4_cksum);
                                    }

                                    // Modify ICMP (starting at offset 14 + ipv4_header_len)
                                    let icmp_offset = 14 + ipv4_len;
                                    {
                                        let mut icmp_reply =
                                            MutableIcmpPacket::new(&mut reply_buf[icmp_offset..])
                                                .unwrap();
                                        icmp_reply.set_icmp_type(IcmpTypes::EchoReply);
                                        icmp_reply.set_checksum(0);
                                    }
                                    // Compute ICMP checksum on a copy
                                    let icmp_cksum = Self::icmp_checksum(&reply_buf[icmp_offset..]);
                                    {
                                        let mut icmp_reply =
                                            MutableIcmpPacket::new(&mut reply_buf[icmp_offset..])
                                                .unwrap();
                                        icmp_reply.set_checksum(icmp_cksum);
                                    }

                                    let _ = self.to_guest.try_send(Bytes::from(reply_buf));
                                }
                            }
                        }
                    }
                }
                _ => {
                    // Silently drop other packet types
                }
            }
        }
    }

    fn poll(&mut self) {
        // No-op
    }

    fn on_exit(&mut self) {
        // No-op
    }
}
