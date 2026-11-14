//! Host-owned cross-process lifecycle serialization.
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Serialization {
    Teardown,
    StartAndTeardown,
}

/// The host-wide lifecycle lock of the Nitro backend. Two teardowns on one host must never
/// overlap, and every final close of a VM descriptor happens while holding an flock on this one
/// file, so every program that starts Nitro guests on a host must use this same path: a second
/// path would be a second lock. Use this constant instead of repeating the string. The host sets
/// up the directory (owned by root, not writable by group or others) and the file.
pub const LIFECYCLE_LOCK_PATH: &str = "/run/holodeck/nitro-lifecycle.lock";

#[derive(Clone, Debug)]
pub struct LifecycleLock {
    pub path: PathBuf,
    pub serialization: Serialization,
}
/// An opened lifecycle lock. Every `OpenLock` of the same file in one process shares one
/// [`Holder`], so several lifecycles in one process (a daemon with many guests, two sandboxes in
/// one VMM, a test) serialise among themselves with a mutex and take the flock once, instead of
/// contending through separate open file descriptions of one file, where a thread that already
/// holds the lock and asks again would wait for itself forever.
pub struct OpenLock {
    holder: std::sync::Arc<Holder>,
    pub serialization: Serialization,
}
/// The process-wide holder of one lock file.
struct Holder {
    path: PathBuf,
    state: std::sync::Mutex<HolderState>,
    released: std::sync::Condvar,
}
struct HolderState {
    /// The open lock file (one open file description for the whole process). Replaced when the
    /// path turns out to name another file (see `OpenLock::acquire`).
    file: std::sync::Arc<File>,
    /// The thread holding the lock, while it is held in this process.
    owner: Option<std::thread::ThreadId>,
}
/// Live holders of this process, one per lock file (found by the device and inode of the file).
static HOLDERS: std::sync::Mutex<Vec<std::sync::Weak<Holder>>> = std::sync::Mutex::new(Vec::new());
/// Held lifecycle lock; dropping it releases the flock and lets the next user in this process in.
pub struct Guard {
    holder: std::sync::Arc<Holder>,
    file: std::sync::Arc<File>,
}

/// Open the lock file at `path` without following a symlink and refuse one that anybody but its
/// owner could replace or that the wrong user owns:
/// - the file must be a regular file owned by root or by this process's effective user, with no
///   execute, set-user-ID, set-group-ID or sticky bit (read and write bits are not checked: an
///   flock needs only an open descriptor, and hosts that run several users' programs keep the
///   file writable by all, mode 0666);
/// - its directory must be owned by root or by this process's effective user and not writable
///   by group or others, so nobody else can remove or replace the file under running programs.
fn open_checked(path: &std::path::Path) -> io::Result<File> {
    use std::os::unix::fs::MetadataExt;
    let refuse = |what: String| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("lifecycle lock {}: {what}", path.display()),
        )
    };
    // SAFETY: plain syscall without arguments.
    let me = unsafe { libc::geteuid() };
    let directory = path.parent().unwrap_or(std::path::Path::new("/"));
    let directory = if directory.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        directory
    };
    let parent = std::fs::symlink_metadata(directory)
        .map_err(|error| refuse(format!("directory {}: {error}", directory.display())))?;
    if !parent.is_dir() {
        return Err(refuse(format!(
            "{} is not a directory",
            directory.display()
        )));
    }
    if parent.uid() != 0 && parent.uid() != me {
        return Err(refuse(format!(
            "directory {} is owned by uid {} (must be root or uid {me})",
            directory.display(),
            parent.uid()
        )));
    }
    if parent.mode() & 0o022 != 0 {
        return Err(refuse(format!(
            "directory {} is writable by group or others (mode {:o}): the file could be replaced",
            directory.display(),
            parent.mode() & 0o7777
        )));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| refuse(error.to_string()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("lifecycle lock {} must be a regular file", path.display()),
        ));
    }
    if metadata.uid() != 0 && metadata.uid() != me {
        return Err(refuse(format!(
            "owned by uid {} (must be root or uid {me})",
            metadata.uid()
        )));
    }
    if metadata.mode() & 0o7111 != 0 {
        return Err(refuse(format!(
            "mode {:o} has execute or special bits",
            metadata.mode() & 0o7777
        )));
    }
    Ok(file)
}
/// Whether `file` is the file `path` names now (same device and inode, not a symlink).
fn names(file: &File, path: &std::path::Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let open = file.metadata()?;
    let current = std::fs::symlink_metadata(path)?;
    Ok(current.is_file() && open.dev() == current.dev() && open.ino() == current.ino())
}
impl LifecycleLock {
    /// Open the host's lock file (see `open_checked` for what is refused). It is never created
    /// here. The opened inode is retained, and `acquire` notices if the path is replaced. An
    /// `OpenLock` for a file this process already has open shares that file's holder.
    pub fn open(&self) -> io::Result<OpenLock> {
        use std::os::unix::fs::MetadataExt;
        let file = open_checked(&self.path)?;
        let opened = file.metadata()?;
        let mut holders = HOLDERS.lock().unwrap_or_else(|e| e.into_inner());
        holders.retain(|holder| holder.strong_count() > 0);
        let existing = holders
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .find(|holder| {
                let state = holder.state.lock().unwrap_or_else(|e| e.into_inner());
                state
                    .file
                    .metadata()
                    .is_ok_and(|held| held.dev() == opened.dev() && held.ino() == opened.ino())
            });
        let holder = match existing {
            Some(holder) => holder,
            None => {
                let holder = std::sync::Arc::new(Holder {
                    path: self.path.clone(),
                    state: std::sync::Mutex::new(HolderState {
                        file: std::sync::Arc::new(file),
                        owner: None,
                    }),
                    released: std::sync::Condvar::new(),
                });
                holders.push(std::sync::Arc::downgrade(&holder));
                holder
            }
        };
        Ok(OpenLock {
            holder,
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
    /// Take the lock (blocking): first among this process's users of the file (a mutex), then
    /// against other processes (the flock, taken once by the process). A thread that already
    /// holds it gets an error at once rather than waiting for itself.
    ///
    /// After the flock succeeds the holder checks that the open file is still the file the path
    /// names: if the file was removed or replaced (a cleanup of the directory, a recreated file),
    /// other programs now lock the new file, so this process unlocks, reopens the path (with the
    /// same checks) and locks that file instead. A path that no longer names an acceptable file
    /// is an error: the caller keeps what it would have closed and retries.
    pub fn acquire(&self) -> io::Result<Guard> {
        let holder = &self.holder;
        let me = std::thread::current().id();
        let mut state = holder.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match state.owner {
                None => break,
                Some(owner) if owner == me => {
                    return Err(io::Error::other(format!(
                        "lifecycle lock {} is already held by this thread",
                        holder.path.display()
                    )));
                }
                Some(_) => {
                    state = holder
                        .released
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner());
                }
            }
        }
        state.owner = Some(me);
        let mut file = state.file.clone();
        drop(state);
        // From here this thread owns the holder; every early return gives it back.
        let give_back = |error: io::Error| {
            holder.state.lock().unwrap_or_else(|e| e.into_inner()).owner = None;
            holder.released.notify_all();
            error
        };
        loop {
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(give_back(error));
                }
            }
            if matches!(names(&file, &holder.path), Ok(true)) {
                return Ok(Guard {
                    holder: holder.clone(),
                    file,
                });
            }
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
            let reopened = open_checked(&holder.path).map_err(|error| {
                give_back(io::Error::new(
                    error.kind(),
                    format!("lifecycle lock file was replaced or removed; reopening: {error}"),
                ))
            })?;
            file = std::sync::Arc::new(reopened);
            holder.state.lock().unwrap_or_else(|e| e.into_inner()).file = file.clone();
        }
    }
    /// The path this lock was opened from.
    pub fn path(&self) -> &std::path::Path {
        &self.holder.path
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
        self.holder
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owner = None;
        self.holder.released.notify_all();
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
    /// A private directory (mode 0700) for lock files: /tmp itself is writable by others and
    /// is refused as a lock directory.
    fn private_directory(name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    /// Run `work` on its own thread and fail if it does not finish within `limit` (a
    /// self-deadlock then fails the test instead of hanging it).
    fn within<T: Send + 'static>(
        limit: std::time::Duration,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(work());
        });
        finished
            .recv_timeout(limit)
            .expect("did not finish in time: lifecycle lock deadlock")
    }
    fn config(path: &std::path::Path) -> LifecycleLock {
        LifecycleLock {
            path: path.to_owned(),
            serialization: Serialization::Teardown,
        }
    }
    #[test]
    fn openers_in_one_process_share_one_holder() {
        let directory = private_directory("nitro-lifecycle");
        let path = directory.join("lock");
        File::create(&path).unwrap();
        let first = config(&path).open().unwrap();
        let second = std::sync::Arc::new(config(&path).open().unwrap());
        let guard = first.acquire().unwrap();
        // Another process (another open file description) cannot take it meanwhile.
        let foreign = File::open(&path).unwrap();
        let try_foreign =
            || unsafe { libc::flock(foreign.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(try_foreign(), -1);
        // Another thread of this process waits for the guard, then gets it.
        let waiter = second.clone();
        let (entered, got) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let guard = waiter.acquire().unwrap();
            entered.send(()).unwrap();
            drop(guard);
        });
        assert!(got
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_err());
        drop(guard);
        got.recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        thread.join().unwrap();
        assert_eq!(try_foreign(), 0);
        unsafe { libc::flock(foreign.as_raw_fd(), libc::LOCK_UN) };
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn a_thread_that_holds_the_lock_is_refused_at_once() {
        let directory = private_directory("nitro-reentry");
        let path = directory.join("lock");
        File::create(&path).unwrap();
        let (first, second) = (config(&path).open().unwrap(), config(&path).open().unwrap());
        let error = within(std::time::Duration::from_secs(10), move || {
            let _guard = first.acquire().unwrap();
            second.acquire().err().map(|error| error.to_string())
        });
        assert!(error.unwrap().contains("already held by this thread"));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn overlapping_lifecycles_in_one_process_never_overlap_and_never_deadlock() {
        // Two lifecycles' worth of openers (as two sandboxes or a daemon's guests would open
        // them), used from 8 threads: create windows and final closes interleave, at most one
        // holds the lock at a time, and all finish within the limit.
        let directory = private_directory("nitro-overlap");
        let path = directory.join("lock");
        File::create(&path).unwrap();
        let locks: Vec<_> = (0..2)
            .map(|_| std::sync::Arc::new(config(&path).open().unwrap()))
            .collect();
        let inside = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let most = within(std::time::Duration::from_secs(60), move || {
            let threads: Vec<_> = (0..8)
                .map(|index| {
                    let (lock, inside) = (locks[index % 2].clone(), inside.clone());
                    std::thread::spawn(move || {
                        let mut most = 0;
                        for round in 0..200 {
                            let mut descriptor = Some(File::open("/dev/null").unwrap().into());
                            let guard = if round % 2 == 0 {
                                Some(lock.acquire().unwrap())
                            } else {
                                None
                            };
                            if guard.is_some() {
                                let now =
                                    inside.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
                                most = most.max(now);
                                std::thread::yield_now();
                                inside.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                            }
                            drop(guard);
                            if round % 2 == 1 {
                                // The close path takes the lock itself.
                                lock.close_vm_descriptor(&mut descriptor).unwrap();
                            }
                        }
                        most
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .max()
                .unwrap()
        });
        assert_eq!(most, 1, "two holders inside the lock at once");
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn daemon_close_consumes_descriptor_under_lock() {
        let directory = private_directory("nitro-daemon-close");
        let path = directory.join("lock");
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
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn missing_daemon_lock_is_not_created() {
        let directory = private_directory("nitro-missing");
        let path = directory.join("lock");
        let config = LifecycleLock {
            path: path.clone(),
            serialization: Serialization::Teardown,
        };
        assert!(config.open().is_err());
        assert!(!path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn replaceable_or_odd_lock_files_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let open = |path: &std::path::Path| {
            LifecycleLock {
                path: path.to_owned(),
                serialization: Serialization::Teardown,
            }
            .open()
        };
        // A directory others can write: anybody could replace the file.
        let shared = private_directory("nitro-shared-dir");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        File::create(shared.join("lock")).unwrap();
        let error = open(&shared.join("lock")).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("writable by group or others"),
            "{error}"
        );
        // Execute or special bits on the file.
        let directory = private_directory("nitro-modes");
        let path = directory.join("lock");
        File::create(&path).unwrap();
        for mode in [0o755, 0o4600, 0o1600] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(open(&path).is_err(), "mode {mode:o}");
        }
        // Read and write bits are not a reason to refuse (shared hosts keep it 0666).
        for mode in [0o600, 0o644, 0o666] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            open(&path).unwrap();
        }
        // A symlink is never followed.
        std::os::unix::fs::symlink(&path, directory.join("link")).unwrap();
        assert!(open(&directory.join("link")).is_err());
        // A file owned by another user (root can create one to check; others skip this).
        if unsafe { libc::geteuid() } == 0 {
            let foreign = directory.join("foreign");
            File::create(&foreign).unwrap();
            std::os::unix::fs::chown(&foreign, Some(65534), Some(65534)).unwrap();
            assert!(open(&foreign).is_err());
        }
        std::fs::remove_dir_all(shared).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn a_replaced_lock_file_is_reopened_after_the_flock() {
        let directory = private_directory("nitro-replaced");
        let path = directory.join("lock");
        File::create(&path).unwrap();
        let lock = LifecycleLock {
            path: path.clone(),
            serialization: Serialization::Teardown,
        }
        .open()
        .unwrap();
        // The file is replaced under the running program (a cleanup recreated it).
        std::fs::remove_file(&path).unwrap();
        File::create(&path).unwrap();
        let guard = lock.acquire().unwrap();
        // The guard now holds the new file: another program locking the path must wait.
        let other = File::open(&path).unwrap();
        assert_eq!(
            unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );
        drop(guard);
        assert_eq!(
            unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_UN) };
        // A removed file with nothing in its place is an error (the caller retries).
        std::fs::remove_file(&path).unwrap();
        assert!(lock.acquire().is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
