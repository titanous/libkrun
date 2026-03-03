use macros::{guest, host};
use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub struct TestVsockGuestConnect;

fn stream_expect_msg(stream: &mut UnixStream, expected: &[u8]) {
    let mut buf = vec![0; expected.len()];
    stream.read_exact(&mut buf[..]).unwrap();
    assert_eq!(&buf[..], expected);
}

fn stream_expect_wouldblock(stream: &mut UnixStream) {
    stream.set_nonblocking(true).unwrap();
    let err = stream.read(&mut [0u8; 1]).unwrap_err();
    stream.set_nonblocking(false).unwrap();
    assert_eq!(err.kind(), ErrorKind::WouldBlock);
}

fn stream_set_timeouts(stream: &mut UnixStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
}

const VSOCK_PORT: u32 = 1234;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::{mem, thread};

    fn server(listener: UnixListener) {
        let (mut stream, _addr) = listener.accept().unwrap();
        stream_set_timeouts(&mut stream);
        stream.write_all(b"ping!").unwrap();
        stream_expect_msg(&mut stream, b"pong!");
        stream_expect_wouldblock(&mut stream);
        stream.write_all(b"bye!").unwrap();
        // Leak the socket fd to not close it early when we exit the thread
        mem::forget(stream);
    }

    impl Test for TestVsockGuestConnect {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("test.sock");
            let listener = UnixListener::bind(&sock_path).unwrap();
            thread::spawn(move || server(listener));

            let mut builder = krun::Builder::new();
            builder.add_vsock_port(VSOCK_PORT, sock_path, false);
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
    use crate::vsock_helpers::vsock_connect;
    use crate::Test;
    use std::io::Write;

    impl Test for TestVsockGuestConnect {
        fn in_guest(self: Box<Self>) {
            let mut stream = vsock_connect(VSOCK_PORT);
            stream_set_timeouts(&mut stream);

            stream_expect_msg(&mut stream, b"ping!");
            stream_expect_wouldblock(&mut stream);
            stream.write_all(b"pong!").unwrap();
            stream_expect_msg(&mut stream, b"bye!");

            println!("OK");
        }
    }
}
