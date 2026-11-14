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
        let guard = self.acquire()?;
        let lock_ns = monotonic_ns();
        let close_start_ns = monotonic_ns();
        drop(descriptor.take());
        let close_end_ns = monotonic_ns();
        drop(guard);
        trace(format_args!(
            "TEARDOWN holder=daemon pid={} vm=- locked=1 lock_ns={lock_ns} close_start_ns={close_start_ns} close_end_ns={close_end_ns} unlock_ns={}",
            std::process::id(),
            monotonic_ns()
        ));
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
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// System-wide monotonic clock (CLOCK_MONOTONIC) in nanoseconds, comparable
/// between processes on one host.
pub fn monotonic_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid pointer to a timespec; CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// Append one line to the teardown trace named by `NITRO_TEARDOWN_TRACE`, if
/// set. Acceptance runs use it to check, from the lock holder's side, that no
/// two teardowns overlapped. One `write` per line on an `O_APPEND` file.
pub fn trace(line: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let Some(path) = std::env::var_os("NITRO_TEARDOWN_TRACE") else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().append(true).create(true).open(path) {
        let _ = file.write_all(format!("{line}\n").as_bytes());
    }
}

/// Payload sent with the duplicate VM descriptor (one `SCM_RIGHTS` message).
pub const VM_DESCRIPTOR_MESSAGE: &[u8] = b"vm_descriptor\n";

/// Send a close-on-exec duplicate of `vm` over `socket` as one `SCM_RIGHTS` message whose
/// payload is [`VM_DESCRIPTOR_MESSAGE`]. Once `sendmsg` returns, the descriptor in flight is a
/// reference of its own: the VM is not released by this process's close, even before the
/// receiver reads the message. The local duplicate is closed before returning.
pub fn send_vm_descriptor(
    socket: &std::os::unix::net::UnixStream,
    vm: &impl std::os::fd::AsFd,
) -> io::Result<()> {
    let duplicate = vm.as_fd().try_clone_to_owned()?;
    let mut payload = VM_DESCRIPTOR_MESSAGE.to_vec();
    let mut vector = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    // usize storage provides cmsghdr alignment.
    let mut control = [0usize; 8];
    // SAFETY: msghdr is plain data; every pointer refers to storage that outlives sendmsg.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as _) } as _;
    // SAFETY: the control buffer holds one cmsghdr with room for one descriptor.
    let sent = unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as _) as _;
        std::ptr::write(libc::CMSG_DATA(header).cast::<i32>(), duplicate.as_raw_fd());
        loop {
            let sent = libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL);
            if sent < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break sent;
        }
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != payload.len() {
        return Err(io::Error::other("short VM descriptor message"));
    }
    Ok(())
}

/// Receive one message sent by [`send_vm_descriptor`] (daemon side; for tests and embedders
/// that do not have their own receiver). Returns the descriptor, close-on-exec.
pub fn receive_vm_descriptor(
    socket: &std::os::unix::net::UnixStream,
) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let mut payload = [0u8; 64];
    let mut vector = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut control = [0usize; 8];
    // SAFETY: as in send_vm_descriptor; the kernel fills at most the given lengths.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control) as _;
    let received = loop {
        let received =
            unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
        if received < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        break received;
    };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: walk the control messages the kernel wrote.
    let mut descriptor = None;
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let raw = std::ptr::read(libc::CMSG_DATA(header).cast::<i32>());
                // Keep the first; any further descriptor is owned and closed at once.
                let owned = std::os::fd::OwnedFd::from_raw_fd(raw);
                if descriptor.is_none() {
                    descriptor = Some(owned);
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    let descriptor =
        descriptor.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no descriptor"))?;
    if &payload[..received as usize] != VM_DESCRIPTOR_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected VM descriptor message",
        ));
    }
    Ok(descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_message_carries_a_duplicate() {
        use std::io::{Read, Seek, Write};
        let (sender, receiver) = std::os::unix::net::UnixStream::pair().unwrap();
        let path = std::env::temp_dir().join(format!("nitro-handoff-{}", std::process::id()));
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.write_all(b"same open file").unwrap();
        send_vm_descriptor(&sender, &file).unwrap();
        drop(file);
        let mut received = File::from(receive_vm_descriptor(&receiver).unwrap());
        received.rewind().unwrap();
        let mut text = String::new();
        received.read_to_string(&mut text).unwrap();
        assert_eq!(text, "same open file", "the duplicate shares the open file");
        let flags = unsafe { libc::fcntl(received.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(
            flags & libc::FD_CLOEXEC,
            0,
            "received descriptor is close-on-exec"
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn send_to_a_closed_peer_fails_without_signal() {
        let (sender, receiver) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(receiver);
        let file = File::open("/dev/null").unwrap();
        assert!(send_vm_descriptor(&sender, &file).is_err());
    }
    #[test]
    fn plain_bytes_are_not_a_descriptor_message() {
        use std::io::Write;
        let (mut sender, receiver) = std::os::unix::net::UnixStream::pair().unwrap();
        sender.write_all(VM_DESCRIPTOR_MESSAGE).unwrap();
        assert!(receive_vm_descriptor(&receiver).is_err());
    }
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
