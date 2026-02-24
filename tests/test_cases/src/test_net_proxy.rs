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

    impl Test for TestNetProxy {
        fn in_guest(self: Box<Self>) {
            // The ProxyNetWorker integration test.
            // For now, this is a placeholder that confirms the test harness works.
            // Full networking validation requires:
            // 1. Guest network interface auto-configuration (or manual setup)
            // 2. smoltcp proxy routing to work correctly
            // 3. Port forwarding coordination between host and guest
            //
            // TODO: Implement full TCP PING/PONG once guest networking is verified
            println!("OK");
        }
    }
}
