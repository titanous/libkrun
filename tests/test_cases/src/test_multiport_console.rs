use macros::{guest, host};

pub struct TestMultiportConsole;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::{mem, thread};
    use krun::ConsoleDeviceInfo;

    fn spawn_ping_pong_responder(stream: UnixStream) {
        thread::spawn(move || {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok() && !line.is_empty() {
                let response = line.replace("PING", "PONG");
                writer.write_all(response.as_bytes()).unwrap();
                writer.flush().unwrap();
                line.clear();
            }
        });
    }

    fn test_port(
        builder: &mut krun::Builder,
        console_info: &ConsoleDeviceInfo,
        name: &str,
    ) -> anyhow::Result<()> {
        let (guest, host) = UnixStream::pair()?;
        builder.add_port_fd(console_info, name, guest.as_raw_fd(), guest.as_raw_fd());
        mem::forget(guest);
        spawn_ping_pong_responder(host);
        Ok(())
    }

    impl Test for TestMultiportConsole {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();

            builder.disable_implicit_console()?;

            // Add a default console routing output to stdout (replaces krun_add_virtio_console_default)
            let default_console_info = builder.add_virtio_console();
            builder.add_port_console_fd(
                &default_console_info,
                -1,
                std::io::stdout().as_raw_fd(),
                80,
                24,
            );
            // Named port so the guest can see "krun-stdout" in sysfs (mirrors what
            // autoconfigure_console_ports creates when the implicit console is active).
            builder.add_port_fd(
                &default_console_info,
                "krun-stdout",
                -1,
                std::io::stdout().as_raw_fd(),
            );

            // Add the multiport console (replaces krun_add_virtio_console_multiport)
            let multiport_console_info = builder.add_virtio_console();

            test_port(&mut builder, &multiport_console_info, "test-port-alpha")?;
            test_port(&mut builder, &multiport_console_info, "test-port-beta")?;
            test_port(&mut builder, &multiport_console_info, "test-port-gamma")?;

            builder.vm_config(1, 1024)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;
            context.run()?;
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};

    fn test_port(port_map: &std::collections::HashMap<String, String>, name: &str, message: &str) {
        let device_path = format!("/dev/{}", port_map.get(name).unwrap());
        let mut port = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device_path)
            .unwrap();

        port.write_all(message.as_bytes()).unwrap();
        port.flush().unwrap();

        let mut reader = BufReader::new(port);
        let mut response = String::new();
        reader.read_line(&mut response).unwrap();

        let expected = message.replace("PING", "PONG").to_string();
        assert_eq!(response, expected, "{}: wrong response", name);
    }

    impl Test for TestMultiportConsole {
        fn in_guest(self: Box<Self>) {
            let ports_dir = "/sys/class/virtio-ports";

            let mut port_map = std::collections::HashMap::new();

            for entry in fs::read_dir(ports_dir).unwrap() {
                let entry = entry.unwrap();
                let port_name_path = entry.path().join("name");

                if port_name_path.exists() {
                    let port_name = fs::read_to_string(&port_name_path)
                        .unwrap()
                        .trim()
                        .to_string();

                    if !port_name.is_empty() {
                        let device_name = entry.file_name().to_string_lossy().to_string();
                        port_map.insert(port_name, device_name);
                    }
                }
            }

            assert!(
                port_map.contains_key("krun-stdout"),
                "krun-stdout not found"
            );
            assert!(
                port_map.contains_key("test-port-alpha"),
                "test-port-alpha not found"
            );
            assert!(
                port_map.contains_key("test-port-beta"),
                "test-port-beta not found"
            );
            assert!(
                port_map.contains_key("test-port-gamma"),
                "test-port-gamma not found"
            );

            // We shouldn't have any more than configured here
            assert_eq!(port_map.len(), 4);

            test_port(&port_map, "test-port-alpha", "PING-ALPHA\n");
            test_port(&port_map, "test-port-beta", "PING-BETA\n");
            test_port(&port_map, "test-port-gamma", "PING-GAMMA\n");

            println!("OK");
        }
    }
}
