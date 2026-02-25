//! Integration test for network connectivity after snapshot/restore.
//!
//! Tests AC6.4: Guest network connectivity works after snapshot/restore

use macros::{guest, host};

pub struct TestSnapshotNet;

const VSOCK_PORT: u32 = 5683;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::loopback_net::LoopbackFactory;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    use std::thread;

    impl Test for TestSnapshotNet {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("snap_net_control.sock");
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;

            // Add loopback network backend
            builder.add_net_device(
                krun::VirtioNetBackend::CustomAsyncFactory(
                    Box::new(LoopbackFactory::new()),
                ),
                [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee], // Guest MAC
                0, // features
            );

            builder.add_vsock_port(VSOCK_PORT, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();

            let vm_thread = thread::spawn(move || context.run());

            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Phase 1: Wait for guest to confirm networking works pre-snapshot
            let mut buf = vec![0u8; 6];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"NET_OK");

            // Take full snapshot
            handle.snapshot(&snap_dir)?;

            // Hot-restore the snapshot
            handle.restore_snapshot(&snap_dir)?;

            // Phase 2: Signal guest to verify networking post-restore
            stream.write_all(b"RESTORED").unwrap();

            // Wait for guest to confirm networking works post-restore
            let mut buf = vec![0u8; 16];
            let n = stream.read(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"NET_OK_AFTER_RESTORE");

            vm_thread.join().ok();
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use nix::libc::VMADDR_CID_HOST;
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use std::io::{Read, Write};
    use std::mem;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// Configure eth0 with 192.168.100.2/24 using raw libc ioctls.
    fn configure_eth0() {
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

    fn test_ping() {
        unsafe {
            let sock = libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP);
            assert!(sock >= 0, "socket(SOCK_RAW, IPPROTO_ICMP) failed");

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

            // Build ICMP echo request
            let id: u16 = 0x1234;
            let seq: u16 = 1;
            let payload = b"snapshot-net";

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

            // Receive ICMP echo reply
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

            // SOCK_RAW delivers full IP packet; skip IP header (variable length)
            let ip_hdr_len = ((recv_buf[0] & 0x0f) as usize) * 4;
            let icmp = &recv_buf[ip_hdr_len..];
            assert_eq!(icmp[0], 0, "Expected ICMP type 0 (echo reply)");
            assert_eq!(icmp[1], 0, "Expected ICMP code 0");
            // Check sequence matches
            let reply_seq = ((icmp[6] as u16) << 8) | (icmp[7] as u16);
            assert_eq!(reply_seq, seq, "ICMP sequence mismatch");

            libc::close(sock);
        }
    }

    impl Test for TestSnapshotNet {
        fn in_guest(self: Box<Self>) {
            // Configure network interface
            configure_eth0();

            let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
                .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Phase 1: Test networking pre-snapshot
            test_ping();

            // Signal host that networking works
            stream.write_all(b"NET_OK").unwrap();

            // Wait for RESTORED signal (after snapshot+restore)
            let mut buf = vec![0u8; 8];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"RESTORED");

            // Phase 2: Test networking post-snapshot/restore
            test_ping();

            // Signal host that networking still works after restore
            stream.write_all(b"NET_OK_AFTER_RESTORE").unwrap();

            println!("OK");
        }
    }
}
