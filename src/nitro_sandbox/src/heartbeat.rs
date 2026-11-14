//! Guest-originated vsock heartbeat, carried through a dedicated Unix mapping.
use std::io::{self, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct Heartbeat {
    listener: UnixListener,
    stream: Option<UnixStream>,
    path: PathBuf,
    timeout: Duration,
    last: Option<Instant>,
}
impl Heartbeat {
    pub fn bind(path: PathBuf, timeout: Duration) -> io::Result<Self> {
        if timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "heartbeat deadline must be nonzero",
            ));
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            stream: None,
            path,
            timeout,
            last: None,
        })
    }
    pub fn poll_fds(&self) -> Vec<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        match &self.stream {
            Some(stream) => vec![stream.as_raw_fd()],
            None => vec![self.listener.as_raw_fd()],
        }
    }
    pub fn start(&mut self, now: Instant) {
        self.last = Some(now);
    }
    pub fn refresh(&mut self, now: Instant) -> io::Result<bool> {
        if self.stream.is_none() {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    self.stream = Some(stream);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        if let Some(stream) = &mut self.stream {
            let mut buffer = [0u8; 256];
            // One bounded read per dispatch. Every 'H' is a heartbeat; no parsing
            // allocation and no guest-supplied length controls host work.
            match stream.read(&mut buffer) {
                Ok(0) => self.stream = None,
                Ok(count) => {
                    if buffer[..count].contains(&b'H') {
                        self.last = Some(now);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => self.stream = None,
            }
        }
        Ok(self
            .last
            .is_some_and(|last| now.duration_since(last) >= self.timeout))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn heartbeat_refreshes_only_on_h_and_expires_after_deadline() {
        let path =
            std::env::temp_dir().join(format!("nitro-heartbeat-{}.sock", std::process::id()));
        let mut heartbeat = Heartbeat::bind(path.clone(), Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        heartbeat.start(start);
        let mut peer = UnixStream::connect(path).unwrap();
        peer.write_all(b"H").unwrap();
        assert!(!heartbeat.refresh(start + Duration::from_secs(1)).unwrap());
        peer.write_all(b"not a pet").unwrap();
        assert!(!heartbeat.refresh(start + Duration::from_secs(2)).unwrap());
        assert!(heartbeat.refresh(start + Duration::from_secs(3)).unwrap());
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
