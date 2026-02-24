use macros::{guest, host};

pub struct TestNetProxy;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
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

            // Write port to a file in the guest filesystem so the guest knows where to connect
            let root_dir = test_setup.tmp_dir.join("root");
            std::fs::write(root_dir.join("host_port"), host_port.to_string())?;

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
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
    use std::process::Command;

    impl Test for TestNetProxy {
        fn in_guest(self: Box<Self>) {
            // Configure the network interface if not already done by init
            // The guest network setup: IP 192.168.100.2/24, gateway 192.168.100.1
            let ip_link_res = Command::new("ip")
                .args(["link", "set", "eth0", "up"])
                .status();
            eprintln!("ip link set eth0 up: {:?}", ip_link_res);

            let ip_addr_res = Command::new("ip")
                .args(["addr", "add", "192.168.100.2/24", "dev", "eth0"])
                .status();
            eprintln!("ip addr add: {:?}", ip_addr_res);

            let ip_route_res = Command::new("ip")
                .args(["route", "add", "default", "via", "192.168.100.1"])
                .status();
            eprintln!("ip route add: {:?}", ip_route_res);

            // Read host port from the file written by the host
            let port_str = match std::fs::read_to_string("/host_port") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to read /host_port: {:?}", e);
                    panic!("Failed to read /host_port: {:?}", e);
                }
            };
            let port: u16 = match port_str.trim().parse() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Invalid port number: {:?}", e);
                    panic!("Invalid port number: {:?}", e);
                }
            };
            eprintln!("Connecting to 127.0.0.1:{}", port);

            // Connect to host TCP listener through the smoltcp proxy
            // The proxy intercepts this SYN and connects a real TcpStream to 127.0.0.1:port
            let mut stream = match TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to connect to 127.0.0.1:{}: {:?}", port, e);
                    panic!("Failed to connect: {:?}", e);
                }
            };
            eprintln!("Connected successfully");

            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Send PING
            if let Err(e) = stream.write_all(b"PING") {
                eprintln!("Failed to send PING: {:?}", e);
                panic!("Failed to send PING: {:?}", e);
            }
            eprintln!("Sent PING");

            // Receive PONG
            let mut buf = vec![0u8; 4];
            if let Err(e) = stream.read_exact(&mut buf) {
                eprintln!("Failed to receive PONG: {:?}", e);
                panic!("Failed to receive PONG: {:?}", e);
            }
            assert_eq!(&buf, b"PONG", "Expected PONG, got {:?}", &buf);
            eprintln!("Received PONG");

            println!("OK");
        }
    }
}
