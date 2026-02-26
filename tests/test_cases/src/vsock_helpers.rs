/// Guest-side vsock helpers (compiled only for guest).
use nix::libc::VMADDR_CID_HOST;
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Connect to a vsock port on the host, retrying on transient errors.
///
/// Under CPU stress the initial connect can hit ETIMEDOUT. This helper
/// retries up to `max_retries` times with a short sleep between attempts.
pub fn vsock_connect(port: u32) -> UnixStream {
    vsock_connect_with_retries(port, 5)
}

fn vsock_connect_with_retries(port: u32, max_retries: u32) -> UnixStream {
    let mut last_err = None;
    for attempt in 0..=max_retries {
        let sock = socket(
            AddressFamily::Vsock,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .expect("failed to create vsock socket");

        let addr = VsockAddr::new(VMADDR_CID_HOST, port);
        match connect(sock.as_raw_fd(), &addr) {
            Ok(()) => {
                let stream = UnixStream::from(sock);
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                return stream;
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < max_retries {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
    panic!(
        "vsock connect to port {port} failed after {} attempts: {}",
        max_retries + 1,
        last_err.unwrap()
    );
}
