//! Host-owned cross-process lifecycle serialization.
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// Host-wide serialization inode shared by every Holodeck Nitro guest lane.
pub const LIFECYCLE_LOCK_PATH: &str = "/run/holodeck/nitro-lifecycle.lock";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Serialization {
    Teardown,
    StartAndTeardown,
}
#[derive(Clone, Debug)]
pub struct LifecycleLock {
    pub path: PathBuf,
    pub serialization: Serialization,
}
pub struct OpenLock {
    file: std::sync::Arc<File>,
    pub serialization: Serialization,
}
pub struct Guard(std::sync::Arc<File>);
impl LifecycleLock {
    /// The daemon creates this file and must not replace its inode while VMMs
    /// exist. Retain the opened inode even if the pathname is later removed.
    pub fn open(&self) -> io::Result<OpenLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lifecycle lock must be a regular file",
            ));
        }
        Ok(OpenLock {
            file: std::sync::Arc::new(file),
            serialization: self.serialization,
        })
    }
}
impl OpenLock {
    /// Close the daemon's retained VM descriptor under the lifecycle lock.
    /// On lock failure the caller retains the descriptor and must retry.
    pub fn close_vm_descriptor(
        &self,
        descriptor: &mut Option<std::os::fd::OwnedFd>,
    ) -> io::Result<()> {
        let _guard = self.acquire()?;
        drop(descriptor.take());
        Ok(())
    }
    pub fn acquire(&self) -> io::Result<Guard> {
        loop {
            if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Guard(self.file.clone()));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_open_descriptions_contend_and_release() {
        let path =
            std::env::temp_dir().join(format!("nitro-lifecycle-{}.lock", std::process::id()));
        File::create(&path).unwrap();
        let config = LifecycleLock {
            path: path.clone(),
            serialization: Serialization::Teardown,
        };
        let first = config.open().unwrap();
        let second = config.open().unwrap();
        let guard = first.acquire().unwrap();
        let try_second =
            || unsafe { libc::flock(second.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(try_second(), -1);
        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
        drop(guard);
        assert_eq!(try_second(), 0);
        unsafe {
            libc::flock(second.file.as_raw_fd(), libc::LOCK_UN);
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn daemon_close_consumes_descriptor_under_lock() {
        let path =
            std::env::temp_dir().join(format!("nitro-daemon-close-{}.lock", std::process::id()));
        File::create(&path).unwrap();
        let lock = LifecycleLock {
            path: path.clone(),
            serialization: Serialization::Teardown,
        }
        .open()
        .unwrap();
        let mut descriptor = Some(File::open("/dev/null").unwrap().into());
        lock.close_vm_descriptor(&mut descriptor).unwrap();
        assert!(descriptor.is_none());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn missing_daemon_lock_is_not_created() {
        let path = std::env::temp_dir().join(format!("nitro-missing-{}.lock", std::process::id()));
        let config = LifecycleLock {
            path: path.clone(),
            serialization: Serialization::Teardown,
        };
        assert!(config.open().is_err());
        assert!(!path.exists());
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
