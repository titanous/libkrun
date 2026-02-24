use macros::{guest, host};

pub struct TestNetProxy;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;
    use std::thread;

    fn server(listener: TcpListener) {
        listener.set_nonblocking(false).unwrap();
        let (mut stream, _addr) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

        let mut buf = vec![0u8; 4];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"PING");

        stream.write_all(b"PONG").unwrap();
    }

    impl Test for TestNetProxy {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            // Bind TCP listener on an ephemeral port
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let host_port = listener.local_addr().unwrap().port();

            // Spawn server thread to handle the guest's connection
            thread::spawn(move || server(listener));

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;

            // Write port to a file in the guest filesystem so the guest knows where to connect
            // Must be done after setup_fs_builder, which creates the root directory.
            let root_dir = test_setup.tmp_dir.join("root");
            std::fs::write(root_dir.join("host_port"), host_port.to_string())?;
            builder.add_net_device(
                krun::VirtioNetBackend::Proxy { listeners: vec![] },
                [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee], // MAC address
                0, // features (0 = no extra features)
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
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

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

    impl Test for TestNetProxy {
        fn in_guest(self: Box<Self>) {
            // Configure eth0 with 192.168.100.2/24 via raw ioctls (no `ip`/`sh` in guest).
            configure_eth0();

            // Read host port from the file written by the host
            let port_str = std::fs::read_to_string("/host_port")
                .expect("Failed to read /host_port");
            let port: u16 = port_str.trim().parse().expect("Invalid port number");

            // Connect to the proxy gateway IP (192.168.100.1 = PROXY_IP).
            // The proxy intercepts this SYN and maps PROXY_IP → 127.0.0.1 on the host,
            // so it connects a real TcpStream to 127.0.0.1:port on the host side.
            let mut stream = TcpStream::connect(("192.168.100.1", port))
                .expect("Failed to connect to host TCP listener via proxy");
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Send PING
            stream.write_all(b"PING").expect("Failed to send PING");

            // Receive PONG
            let mut buf = vec![0u8; 4];
            stream.read_exact(&mut buf).expect("Failed to receive PONG");
            assert_eq!(&buf, b"PONG", "Expected PONG, got {:?}", &buf);

            println!("OK");
        }
    }
}
