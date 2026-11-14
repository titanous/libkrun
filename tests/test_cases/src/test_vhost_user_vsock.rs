use macros::{guest, host};

pub struct TestVhostUserVsockEcho;
pub struct TestVhostUserVsockFd;

#[allow(dead_code)]
const ECHO_PORT: u32 = 9999;
#[allow(dead_code)]
const COUNTER_PORT: u32 = 9998;

#[host]
mod host_helpers {
    use std::path::Path;
    use std::process::{Child, Command};
    use std::thread;
    use std::time::Duration;

    /// Start the test proxy and return the child process handle.
    pub fn start_proxy(socket_path: &Path, guest_cid: u64) -> Child {
        let proxy_path = std::env::var("KRUN_TEST_VSOCK_PROXY_PATH").unwrap_or_else(|_| {
            let exe = std::env::current_exe().unwrap();
            exe.parent()
                .unwrap()
                .join("test-vsock-proxy")
                .to_string_lossy()
                .to_string()
        });

        let child = Command::new(&proxy_path)
            .arg("--socket-path")
            .arg(socket_path)
            .arg("--guest-cid")
            .arg(guest_cid.to_string())
            .spawn()
            .expect("Failed to start test-vsock-proxy");

        for _ in 0..50 {
            if socket_path.exists() {
                return child;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("test-vsock-proxy did not create socket within 5 seconds");
    }
}

#[guest]
mod guest_helpers {
    use std::io::{Read, Write};

    /// Echo roundtrip: connect, send data, read response.
    pub fn echo_roundtrip(port: u32, data: &[u8]) -> Vec<u8> {
        let mut sock = crate::vsock_helpers::vsock_connect(port);
        sock.write_all(data).expect("failed to write echo data");
        let mut response = vec![0u8; data.len()];
        sock.read_exact(&mut response)
            .expect("failed to read echo response");
        response
    }

}

// =============================================================================
// Test 1: VhostUserVsockEcho
// =============================================================================

#[host]
mod echo_host {
    use super::*;
    use crate::{Test, TestSetup};

    impl Test for TestVhostUserVsockEcho {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            use crate::krun_rust::setup_fs_builder;
            use std::thread;

            let socket_path = test_setup.tmp_dir.join("proxy.sock");
            let mut proxy = host_helpers::start_proxy(&socket_path, 3);

            let mut builder = krun::Builder::new();
            // This fixture's external proxy owns its socket policy.
            builder.internet_socket_forwarding(true);
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_vsock_vhost_user(socket_path.to_str().unwrap())?;

            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());

            vm_thread.join().ok();
            proxy.kill().ok();
            proxy.wait().ok();

            Ok(())
        }
    }
}

#[guest]
mod echo_guest {
    use super::*;
    use crate::Test;

    impl Test for TestVhostUserVsockEcho {
        fn in_guest(self: Box<Self>) {
            // AC3.1: Echo roundtrip
            let resp = guest_helpers::echo_roundtrip(super::ECHO_PORT, b"hello");
            assert_eq!(&resp, b"hello", "echo roundtrip failed");

            // AC3.2: Multiple concurrent connections
            let mut s1 = crate::vsock_helpers::vsock_connect(super::ECHO_PORT);
            let mut s2 = crate::vsock_helpers::vsock_connect(super::ECHO_PORT);

            use std::io::{Read, Write};
            s1.write_all(b"first").unwrap();
            s2.write_all(b"second").unwrap();

            let mut buf1 = [0u8; 5];
            let mut buf2 = [0u8; 6];
            s1.read_exact(&mut buf1).unwrap();
            s2.read_exact(&mut buf2).unwrap();

            assert_eq!(&buf1, b"first", "first connection failed");
            assert_eq!(&buf2, b"second", "second connection failed");

            println!("OK");
        }
    }
}

// =============================================================================
// Test 2: VhostUserVsockFd
// =============================================================================

#[host]
mod fd_host {
    use super::*;
    use crate::{Test, TestSetup};

    impl Test for TestVhostUserVsockFd {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            use crate::krun_rust::setup_fs_builder;
            use std::os::unix::net::UnixStream;
            use std::thread;

            let socket_path = test_setup.tmp_dir.join("proxy.sock");
            let mut proxy = host_helpers::start_proxy(&socket_path, 3);

            // Connect to proxy to get a UnixStream
            let stream = UnixStream::connect(&socket_path)?;

            let mut builder = krun::Builder::new();
            // This fixture's external proxy owns its socket policy.
            builder.internet_socket_forwarding(true);
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_vsock_vhost_user_fd(stream)?;

            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());

            vm_thread.join().ok();
            proxy.kill().ok();
            proxy.wait().ok();

            Ok(())
        }
    }
}

#[guest]
mod fd_guest {
    use super::*;
    use crate::Test;

    impl Test for TestVhostUserVsockFd {
        fn in_guest(self: Box<Self>) {
            // AC3.1: Basic echo test
            let resp = guest_helpers::echo_roundtrip(super::ECHO_PORT, b"hello");
            assert_eq!(&resp, b"hello", "echo roundtrip failed");

            println!("OK");
        }
    }
}
