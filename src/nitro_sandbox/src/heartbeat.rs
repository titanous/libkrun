//! Guest-originated vsock heartbeat, carried through a dedicated Unix mapping.
use std::io::{self, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
pub struct DeadlineControl(std::sync::Arc<std::sync::Mutex<(bool, Option<Instant>)>>);
impl DeadlineControl {
    pub fn suspend(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).0 = true;
    }
    pub fn resume(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *state = (false, Some(Instant::now()));
    }
}
pub struct Heartbeat {
    listener: UnixListener,
    stream: Option<UnixStream>,
    path: PathBuf,
    timeout: Duration,
    last: Option<Instant>,
    control: DeadlineControl,
}
impl Heartbeat {
    pub fn bind(path: PathBuf, timeout: Duration) -> io::Result<Self> {
        if timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "heartbeat deadline must be nonzero",
            ));
        }
        use std::os::unix::fs::MetadataExt;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("heartbeat requires a private directory"))?;
        let metadata = std::fs::symlink_metadata(parent)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "heartbeat directory must be owned by the VMM uid with owner-only permissions",
            ));
        }
        // bind refuses existing paths, including a preclaimed socket or symlink.
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            stream: None,
            path,
            timeout,
            last: None,
            control: DeadlineControl::default(),
        })
    }
    pub fn control(&self) -> DeadlineControl {
        self.control.clone()
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
        let mut control = self.control.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(resumed) = control.1.take() {
            self.last = Some(resumed);
        }
        if control.0 {
            return Ok(false);
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
        use std::os::unix::fs::DirBuilderExt;
        let directory =
            std::env::temp_dir().join(format!("nitro-heartbeat-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let path = directory.join("heartbeat.sock");
        let mut heartbeat = Heartbeat::bind(path.clone(), Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        heartbeat.start(start);
        let mut peer = UnixStream::connect(path).unwrap();
        peer.write_all(b"H").unwrap();
        assert!(!heartbeat.refresh(start + Duration::from_secs(1)).unwrap());
        peer.write_all(b"not a pet").unwrap();
        assert!(!heartbeat.refresh(start + Duration::from_secs(2)).unwrap());
        assert!(heartbeat.refresh(start + Duration::from_secs(3)).unwrap());
        drop(heartbeat);
        assert!(!directory.join("heartbeat.sock").exists());
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn private_path_claim_and_suspended_deadline() {
        use std::os::unix::fs::DirBuilderExt;
        use std::os::unix::fs::PermissionsExt;
        let insecure = std::env::temp_dir().join(format!("nitro-insecure-{}", std::process::id()));
        std::fs::create_dir(&insecure).unwrap();
        std::fs::set_permissions(&insecure, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(Heartbeat::bind(insecure.join("untrusted.sock"), Duration::from_secs(1)).is_err());
        std::fs::remove_dir(insecure).unwrap();
        let directory =
            std::env::temp_dir().join(format!("nitro-heartbeat-control-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let path = directory.join("heartbeat.sock");
        let mut heartbeat = Heartbeat::bind(path.clone(), Duration::from_secs(1)).unwrap();
        assert!(Heartbeat::bind(path.clone(), Duration::from_secs(1)).is_err());
        heartbeat.start(Instant::now());
        let control = heartbeat.control();
        control.suspend();
        assert!(!heartbeat
            .refresh(Instant::now() + Duration::from_secs(5))
            .unwrap());
        control.resume();
        assert!(!heartbeat.refresh(Instant::now()).unwrap());
        assert!(heartbeat
            .refresh(Instant::now() + Duration::from_secs(2))
            .unwrap());
        drop(heartbeat);
        assert!(!path.exists());
        std::fs::remove_dir(directory).unwrap();
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
