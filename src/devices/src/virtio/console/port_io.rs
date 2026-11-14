use libc::{O_NONBLOCK, STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO, TIOCGWINSZ};
use log::Level;
use nix::errno::Errno;
use nix::ioctl_read_bad;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::{dup, isatty};
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use utils::eventfd::EventFd;
use utils::eventfd::EFD_NONBLOCK;
use vm_memory::bitmap::Bitmap;
use vm_memory::{VolatileSlice, WriteVolatile};

#[cfg(test)]
#[path = "port_io_tests.rs"]
mod descriptor_tests;

pub trait PortInput {
    fn read_volatile(&mut self, buf: &mut VolatileSlice) -> Result<usize, io::Error>;

    fn wait_until_readable(&self, stopfd: Option<&EventFd>);
}

pub trait PortOutput {
    fn write_volatile(&mut self, buf: &VolatileSlice) -> Result<usize, io::Error>;

    fn wait_until_writable(&self);
}

/// Terminal properties associated with this port
pub trait PortTerminalProperties: Send + Sync {
    fn get_win_size(&self) -> (u16, u16);
}

pub fn stdin() -> Result<Box<dyn PortInput + Send>, nix::Error> {
    input_to_raw_fd_dup(STDIN_FILENO)
}

pub fn input_to_raw_fd_dup(fd: RawFd) -> Result<Box<dyn PortInput + Send>, nix::Error> {
    Ok(Box::new(PortInputFd(prepare_private_fd(fd, false)?)))
}

pub fn stdout() -> Result<Box<dyn PortOutput + Send>, nix::Error> {
    output_to_raw_fd_dup_blocking(STDOUT_FILENO)
}

pub fn stderr() -> Result<Box<dyn PortOutput + Send>, nix::Error> {
    output_to_raw_fd_dup_blocking(STDERR_FILENO)
}

pub fn term_fd(
    term_fd: RawFd,
) -> Result<Box<dyn PortTerminalProperties + Send + Sync>, nix::Error> {
    let fd = dup_raw_fd_into_owned(term_fd)?;
    assert!(
        isatty(&fd).is_ok_and(|v| v),
        "Expected fd {fd:?}, to be a tty, to query the window size!"
    );
    Ok(Box::new(PortTerminalPropertiesFd(fd)))
}

pub fn term_fixed_size(width: u16, height: u16) -> Box<dyn PortTerminalProperties + Send + Sync> {
    Box::new(PortTerminalPropertiesFixed((width, height)))
}

pub fn input_empty() -> Result<Box<dyn PortInput + Send>, nix::Error> {
    Ok(Box::new(PortInputEmpty {}))
}

pub fn output_file(file: File) -> Result<Box<dyn PortOutput + Send>, nix::Error> {
    output_to_raw_fd_dup(file.as_raw_fd())
}

pub fn output_to_raw_fd_dup(fd: RawFd) -> Result<Box<dyn PortOutput + Send>, nix::Error> {
    output_to_raw_fd_dup_blocking(fd)
}

/// Preserve the supplied open description, including offset and append semantics.
pub fn output_to_raw_fd_dup_blocking(fd: RawFd) -> Result<Box<dyn PortOutput + Send>, nix::Error> {
    Ok(Box::new(PortOutputFd(prepare_output_fd(fd)?)))
}

fn prepare_output_fd(fd: RawFd) -> Result<OwnedFd, nix::Error> {
    prepare_private_fd(fd, true)
}

fn prepare_private_fd(fd: RawFd, output: bool) -> Result<OwnedFd, nix::Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat initializes stat on success; fd remains owned by caller.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
        return Err(Errno::last());
    }
    let mode = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
    if mode == libc::S_IFIFO || mode == libc::S_IFCHR {
        // No regular-file offsets or socket identity are reopened. Pipe/tty flags
        // belong to this private description, never the caller's shared one.
        let file = std::fs::OpenOptions::new()
            .write(output)
            .read(!output)
            .custom_flags(O_NONBLOCK | libc::O_NOCTTY)
            .open(format!("/proc/self/fd/{fd}"))
            .map_err(|error| {
                log::error!("console fd {fd}: cannot obtain a private nonblocking description; refusing unsafe shared-flag or blocking fallback: {error}");
                Errno::from_raw(error.raw_os_error().unwrap_or(libc::EIO))
            })?;
        Ok(file.into())
    } else {
        dup_raw_fd_into_owned(fd)
    }
}

pub fn output_to_log_as_err() -> Box<dyn PortOutput + Send> {
    Box::new(PortOutputLog::new())
}

struct PortInputFd(OwnedFd);

impl AsRawFd for PortInputFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl PortInput for PortInputFd {
    fn read_volatile(&mut self, buf: &mut VolatileSlice) -> io::Result<usize> {
        // This source code is copied from vm-memory, except it fixes an issue, where
        // the original code would does not handle handle EWOULDBLOCK

        let fd = self.as_raw_fd();
        let guard = buf.ptr_guard_mut();

        let dst = guard.as_ptr().cast::<libc::c_void>();

        // SAFETY: We got a valid file descriptor from `AsRawFd`. The memory pointed to by `dst` is
        // valid for writes of length `buf.len() by the invariants upheld by the constructor
        // of `VolatileSlice`.
        let received = unsafe { libc::recv(fd, dst, buf.len(), libc::MSG_DONTWAIT) };
        let bytes_read =
            if received < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTSOCK) {
                unsafe { libc::read(fd, dst, buf.len()) }
            } else {
                received
            };

        if bytes_read < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != ErrorKind::WouldBlock {
                // We don't know if a partial read might have happened, so mark everything as dirty
                buf.bitmap().mark_dirty(0, buf.len());
            }

            Err(err)
        } else {
            let bytes_read = bytes_read.try_into().unwrap();
            buf.bitmap().mark_dirty(0, bytes_read);
            Ok(bytes_read)
        }
    }

    fn wait_until_readable(&self, stopfd: Option<&EventFd>) {
        let mut poll_fds = Vec::new();
        poll_fds.push(PollFd::new(self.0.as_fd(), PollFlags::POLLIN));
        if let Some(stopfd) = stopfd {
            // SAFETY: we trust stopfd won't go away to avoid a dup call here.
            let borrowed_fd = unsafe { BorrowedFd::borrow_raw(stopfd.as_raw_fd()) };
            poll_fds.push(PollFd::new(borrowed_fd, PollFlags::POLLIN));
        }
        poll(&mut poll_fds, PollTimeout::NONE).expect("Failed to poll");
    }
}

struct PortOutputFd(OwnedFd);

impl AsRawFd for PortOutputFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl PortOutput for PortOutputFd {
    fn write_volatile(&mut self, buf: &VolatileSlice) -> Result<usize, io::Error> {
        let fd = self.as_raw_fd();
        let guard = buf.ptr_guard();
        // Socket nonblocking behavior is per call and does not alter caller flags.
        let sent = unsafe {
            libc::send(
                fd,
                guard.as_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if sent >= 0 {
            return Ok(sent as usize);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOTSOCK) {
            return Err(error);
        }
        // Regular files retain offset/O_APPEND; pipes/terminals already own a
        // private nonblocking description. No caller flags are ever changed.
        let written = unsafe { libc::write(fd, guard.as_ptr().cast(), buf.len()) };
        if written < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(written as usize)
        }
    }

    fn wait_until_writable(&self) {
        let mut poll_fds = [PollFd::new(self.0.as_fd(), PollFlags::POLLOUT)];
        // Return periodically so the worker observes final stop even if a reader stalls.
        poll(&mut poll_fds, PollTimeout::from(10u16)).expect("Failed to poll");
    }
}

fn dup_raw_fd_into_owned(raw_fd: RawFd) -> Result<OwnedFd, nix::Error> {
    // SAFETY: if raw_fd is invalid the `dup` call below will fail
    let borrowed_fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    let fd = dup(borrowed_fd)?;
    Ok(fd)
}

// Utility to relay log from the VM (the kernel boot log and messages from init)
// to the rust log
#[derive(Default)]
pub struct PortOutputLog {
    buf: Vec<u8>,
}

impl PortOutputLog {
    const FORCE_FLUSH_TRESHOLD: usize = 512;
    const LOG_TARGET: &'static str = "init_or_kernel";

    fn new() -> Self {
        Self::default()
    }

    fn force_flush(&mut self) {
        log::log!(target: PortOutputLog::LOG_TARGET, Level::Error, "[missing newline]{}", String::from_utf8_lossy(&self.buf));
        self.buf.clear();
    }
}

impl PortOutput for PortOutputLog {
    fn write_volatile(&mut self, buf: &VolatileSlice) -> Result<usize, io::Error> {
        self.buf.write_volatile(buf).map_err(io::Error::other)?;

        let mut start = 0;
        for (i, ch) in self.buf.iter().cloned().enumerate() {
            if ch == b'\n' {
                log::log!(target: PortOutputLog::LOG_TARGET, Level::Error, "{}", String::from_utf8_lossy(&self.buf[start..i]));
                start = i + 1;
            }
        }
        self.buf.drain(0..start);
        // Make sure to not grow the internal buffer forever!
        if self.buf.len() > PortOutputLog::FORCE_FLUSH_TRESHOLD {
            self.force_flush()
        }
        Ok(buf.len())
    }

    fn wait_until_writable(&self) {}
}

pub struct PortInputSigInt {
    sigint_evt: EventFd,
}

impl PortInputSigInt {
    pub fn new() -> Self {
        PortInputSigInt {
            sigint_evt: EventFd::new(EFD_NONBLOCK)
                .expect("Failed to create EventFd for SIGINT signaling"),
        }
    }

    pub fn sigint_evt(&self) -> &EventFd {
        &self.sigint_evt
    }
}

impl Default for PortInputSigInt {
    fn default() -> Self {
        Self::new()
    }
}

impl PortInput for PortInputSigInt {
    fn read_volatile(&mut self, buf: &mut VolatileSlice) -> Result<usize, io::Error> {
        self.sigint_evt.read()?;
        log::trace!("SIGINT received");
        buf.copy_from(&[3u8]); //ASCII 'ETX' -> generates SIGINIT in a terminal
        Ok(1)
    }

    fn wait_until_readable(&self, stopfd: Option<&EventFd>) {
        let mut poll_fds = Vec::with_capacity(2);
        // SAFETY: we trust sigint_evt won't go away to avoid a dup call here.
        let sigint_bfd = unsafe { BorrowedFd::borrow_raw(self.sigint_evt.as_raw_fd()) };
        poll_fds.push(PollFd::new(sigint_bfd, PollFlags::POLLIN));
        if let Some(stopfd) = stopfd {
            // SAFETY: we trust stopfd won't go away to avoid a dup call here.
            let stop_bfd = unsafe { BorrowedFd::borrow_raw(stopfd.as_raw_fd()) };
            poll_fds.push(PollFd::new(stop_bfd, PollFlags::POLLIN));
        }

        poll(&mut poll_fds, PollTimeout::NONE).expect("Failed to poll");
    }
}

pub struct PortInputEmpty {}

impl PortInputEmpty {
    pub fn new() -> Self {
        PortInputEmpty {}
    }
}

impl Default for PortInputEmpty {
    fn default() -> Self {
        Self::new()
    }
}

impl PortInput for PortInputEmpty {
    fn read_volatile(&mut self, _buf: &mut VolatileSlice) -> Result<usize, io::Error> {
        Ok(0)
    }

    fn wait_until_readable(&self, stopfd: Option<&EventFd>) {
        if let Some(stopfd) = stopfd {
            // SAFETY: we trust stopfd won't go away to avoid a dup call here.
            let borrowed_fd = unsafe { BorrowedFd::borrow_raw(stopfd.as_raw_fd()) };
            let mut poll_fds = [PollFd::new(borrowed_fd, PollFlags::POLLIN)];
            poll(&mut poll_fds, PollTimeout::NONE).expect("Failed to poll");
        } else {
            std::thread::sleep(std::time::Duration::MAX);
        }
    }
}

struct PortTerminalPropertiesFixed((u16, u16));

impl PortTerminalProperties for PortTerminalPropertiesFixed {
    fn get_win_size(&self) -> (u16, u16) {
        self.0
    }
}

struct PortTerminalPropertiesFd(OwnedFd);

impl PortTerminalProperties for PortTerminalPropertiesFd {
    fn get_win_size(&self) -> (u16, u16) {
        let mut ws: WS = WS::default();

        if let Err(err) = unsafe { tiocgwinsz(self.0.as_raw_fd(), &mut ws) } {
            error!("Couldn't get terminal dimensions: {err}");
            return (0, 0);
        }
        (ws.cols, ws.rows)
    }
}

#[repr(C)]
#[derive(Default)]
struct WS {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}
ioctl_read_bad!(tiocgwinsz, TIOCGWINSZ, WS);
