//! Bounded guest-to-host buffering shared by mapped Unix and forwarded TCP streams.
use std::collections::VecDeque;
use std::os::fd::RawFd;

use nix::errno::Errno;
use nix::sys::socket::{send, MsgFlags};

#[derive(Default)]
pub(super) struct StreamBuffer {
    pending: VecDeque<u8>,
    shutdown: bool,
}

impl StreamBuffer {
    pub fn enqueue(&mut self, data: &[u8]) -> Result<(), Errno> {
        if data.len() > super::defs::CONN_TX_BUF_SIZE - self.pending.len() {
            return Err(Errno::ENOBUFS);
        }
        self.pending.extend(data);
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Stop at would-block; OUT is armed only while bytes remain pending.
    pub fn flush(&mut self, fd: RawFd) -> Result<usize, Errno> {
        let mut forwarded = 0;
        while !self.pending.is_empty() {
            let (first, _) = self.pending.as_slices();
            match send(fd, first, MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL) {
                Ok(0) => return Err(Errno::EPIPE),
                Ok(count) => {
                    self.pending.drain(..count);
                    forwarded += count;
                }
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(error),
            }
        }
        if self.pending.is_empty() && self.shutdown {
            nix::sys::socket::shutdown(fd, nix::sys::socket::Shutdown::Write)?;
            self.shutdown = false;
        }
        Ok(forwarded)
    }

    pub fn shutdown_write(&mut self, fd: RawFd) {
        if self.pending.is_empty() {
            let _ = nix::sys::socket::shutdown(fd, nix::sys::socket::Shutdown::Write);
        } else {
            self.shutdown = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn bounded_by_advertised_allocation() {
        let mut buffer = StreamBuffer::default();
        buffer
            .enqueue(&vec![7; super::super::defs::CONN_TX_BUF_SIZE])
            .unwrap();
        assert_eq!(buffer.enqueue(&[1]), Err(Errno::ENOBUFS));
    }

    #[test]
    fn partial_send_would_block_retry_and_shutdown() {
        let (sender, mut receiver) = UnixStream::pair().unwrap();
        nix::sys::socket::setsockopt(&sender, nix::sys::socket::sockopt::SndBuf, &4096).unwrap();
        let data: Vec<u8> = (0..1048576).map(|i| (i % 251) as u8).collect();
        let mut buffer = StreamBuffer::default();
        buffer.enqueue(&data).unwrap();
        let first = buffer.flush(sender.as_raw_fd()).unwrap();
        assert!(first > 0 && first < data.len());
        assert_eq!(buffer.flush(sender.as_raw_fd()).unwrap(), 0);
        buffer.shutdown_write(sender.as_raw_fd());
        let reader = std::thread::spawn(move || {
            receiver
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut received = Vec::new();
            receiver.read_to_end(&mut received).unwrap();
            received
        });
        let mut forwarded = first;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !buffer.is_empty() {
            assert!(std::time::Instant::now() < deadline);
            forwarded += buffer.flush(sender.as_raw_fd()).unwrap();
            std::thread::yield_now();
        }
        assert_eq!(forwarded, data.len());
        assert_eq!(reader.join().unwrap(), data);
    }
}
