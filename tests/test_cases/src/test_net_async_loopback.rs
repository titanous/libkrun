//! Integration test for async net loopback backend.
//!
//! Tests:
//! - AC3.1: Uses CustomAsyncFactory with LoopbackFactory
//! - AC3.2: Guest configures eth0 with 192.168.100.2/24 and pings 192.168.100.1
//! - AC3.3: Guest receives ICMP echo reply within timeout

use macros::{guest, host};

pub struct TestNetAsyncLoopback;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::loopback_net::LoopbackFactory;
    use crate::{Test, TestSetup};
    use std::thread;

    impl Test for TestNetAsyncLoopback {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;

            // AC3.1: Use CustomAsyncFactory with LoopbackFactory
            builder.add_net_device(
                krun::VirtioNetBackend::CustomAsyncFactory(
                    Box::new(LoopbackFactory::new()),
                ),
                [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee], // Guest MAC
                0, // features
            );

            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());
            vm_thread.join().ok();
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    /// Configure eth0 with 192.168.100.2/24 using raw libc ioctls.
    /// The minimal guest environment has no `ip` or `sh`, so this is necessary.
    fn configure_eth0() {
        use std::mem;

        // IOCTL numbers for Linux x86_64 (same on glibc and musl)
        const SIOCSIFADDR: u32 = 0x8916;
        const SIOCSIFNETMASK: u32 = 0x891c;
        const SIOCSIFFLAGS: u32 = 0x8914;

        unsafe {
            let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            assert!(sock >= 0, "socket() failed");

            let ifname: [libc::c_char; libc::IFNAMSIZ] = {
                let mut arr = [0; libc::IFNAMSIZ];
                for (i, c) in b"eth0".iter().enumerate() {
                    arr[i] = *c as libc::c_char;
                }
                arr
            };

            // Set IP address: 192.168.100.2
            // Cast from ifr_ifru (aligned at offset 16 of ifreq) not from sa_data (offset 2).
            let mut ifr: libc::ifreq = mem::zeroed();
            ifr.ifr_name = ifname;
            let sin = std::ptr::addr_of_mut!(ifr.ifr_ifru) as *mut libc::sockaddr_in;
            (*sin).sin_family = libc::AF_INET as libc::sa_family_t;
            (*sin).sin_addr.s_addr = 0xc0a86402_u32.to_be(); // 192.168.100.2
            let ret = libc::ioctl(sock, SIOCSIFADDR as _, &ifr as *const _);
            assert!(ret == 0, "SIOCSIFADDR failed: {}", *libc::__errno_location());

            // Set netmask: 255.255.255.0
            let mut ifr2: libc::ifreq = mem::zeroed();
            ifr2.ifr_name = ifname;
            let sin2 = std::ptr::addr_of_mut!(ifr2.ifr_ifru) as *mut libc::sockaddr_in;
            (*sin2).sin_family = libc::AF_INET as libc::sa_family_t;
            (*sin2).sin_addr.s_addr = 0xffffff00_u32.to_be(); // 255.255.255.0
            let ret = libc::ioctl(sock, SIOCSIFNETMASK as _, &ifr2 as *const _);
            assert!(ret == 0, "SIOCSIFNETMASK failed: {}", *libc::__errno_location());

            // Bring interface up
            let mut ifr3: libc::ifreq = mem::zeroed();
            ifr3.ifr_name = ifname;
            ifr3.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            let ret = libc::ioctl(sock, SIOCSIFFLAGS as _, &ifr3 as *const _);
            assert!(ret == 0, "SIOCSIFFLAGS failed: {}", *libc::__errno_location());

            libc::close(sock);
        }
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

    impl Test for TestNetAsyncLoopback {
        fn in_guest(self: Box<Self>) {
            configure_eth0();

            // AC3.2: Open ICMP datagram socket (unprivileged, no CAP_NET_RAW)
            unsafe {
                let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP);
                assert!(sock >= 0, "socket(SOCK_DGRAM, IPPROTO_ICMP) failed");

                // Set receive timeout
                let tv = libc::timeval { tv_sec: 5, tv_usec: 0 };
                let ret = libc::setsockopt(
                    sock,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &tv as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                );
                assert!(ret == 0, "setsockopt SO_RCVTIMEO failed");

                // Build ICMP echo request manually:
                // Type=8 (echo request), Code=0, Checksum, Identifier, Sequence, Payload
                let id: u16 = 0x1234;
                let seq: u16 = 1;
                let payload = b"loopback";

                let mut icmp_buf = vec![0u8; 8 + payload.len()];
                icmp_buf[0] = 8; // type = echo request
                icmp_buf[1] = 0; // code = 0
                // checksum at [2..4], set to 0 first
                icmp_buf[4] = (id >> 8) as u8;
                icmp_buf[5] = (id & 0xff) as u8;
                icmp_buf[6] = (seq >> 8) as u8;
                icmp_buf[7] = (seq & 0xff) as u8;
                icmp_buf[8..].copy_from_slice(payload);

                // Compute checksum
                let cksum = icmp_checksum(&icmp_buf);
                icmp_buf[2] = (cksum >> 8) as u8;
                icmp_buf[3] = (cksum & 0xff) as u8;

                // Send to 192.168.100.1
                let dst = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: 0,
                    sin_addr: libc::in_addr {
                        s_addr: 0xc0a86401_u32.to_be(), // 192.168.100.1
                    },
                    sin_zero: [0; 8],
                };

                let sent = libc::sendto(
                    sock,
                    icmp_buf.as_ptr() as *const libc::c_void,
                    icmp_buf.len(),
                    0,
                    &dst as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                );
                assert!(sent == icmp_buf.len() as isize, "sendto failed");

                // AC3.3: Receive ICMP echo reply
                let mut recv_buf = vec![0u8; 256];
                let received = libc::recvfrom(
                    sock,
                    recv_buf.as_mut_ptr() as *mut libc::c_void,
                    recv_buf.len(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                assert!(received > 0, "recvfrom failed or timed out");

                // The kernel strips the IP header for SOCK_DGRAM sockets,
                // so recv_buf starts with the ICMP header
                assert_eq!(recv_buf[0], 0, "Expected ICMP type 0 (echo reply)");
                assert_eq!(recv_buf[1], 0, "Expected ICMP code 0");
                // Check sequence matches
                let reply_seq = ((recv_buf[6] as u16) << 8) | (recv_buf[7] as u16);
                assert_eq!(reply_seq, seq, "ICMP sequence mismatch");

                libc::close(sock);
            }

            println!("OK");
        }
    }
}
