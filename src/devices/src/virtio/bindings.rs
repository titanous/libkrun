use libc;

pub const LINUX_EACCES: libc::c_int = 13;
pub const LINUX_ENODATA: libc::c_int = 61;
pub const LINUX_ENOSYS: libc::c_int = 38;
pub const LINUX_ENOTEMPTY: libc::c_int = 39;

pub const LINUX_O_APPEND: libc::c_int = 1024;
pub const LINUX_O_CLOEXEC: libc::c_int = 0x80000;
pub const LINUX_O_DIRECT: libc::c_int = 0x4000;
pub const LINUX_O_DIRECTORY: libc::c_int = 0x10000;
pub const LINUX_O_LARGEFILE: libc::c_int = 0;
pub const LINUX_O_NOFOLLOW: libc::c_int = 0x20000;
pub const LINUX_O_CREAT: libc::c_int = 64;
pub const LINUX_O_EXCL: libc::c_int = 128;
pub const LINUX_O_NOCTTY: libc::c_int = 256;
pub const LINUX_O_NONBLOCK: libc::c_int = 2048;
pub const LINUX_O_SYNC: libc::c_int = 1052672;
pub const LINUX_O_TRUNC: libc::c_int = 512;
pub const LINUX_O_RSYNC: libc::c_int = 1052672;
pub const LINUX_O_DSYNC: libc::c_int = 4096;
pub const LINUX_O_ASYNC: libc::c_int = 0x2000;

pub const LINUX_RENAME_NOREPLACE: libc::c_int = 1 << 0;
pub const LINUX_RENAME_EXCHANGE: libc::c_int = 1 << 1;
pub const LINUX_RENAME_WHITEOUT: libc::c_int = 1 << 2;

pub const LINUX_XATTR_CREATE: libc::c_int = 1;
pub const LINUX_XATTR_REPLACE: libc::c_int = 2;

#[cfg(target_os = "macos")]
pub type stat64 = libc::stat;
#[cfg(target_os = "linux")]
pub use libc::stat64;

#[cfg(target_os = "macos")]
pub type off64_t = libc::off_t;
#[cfg(target_os = "linux")]
pub use libc::off64_t;

#[cfg(target_os = "macos")]
pub type statvfs64 = libc::statvfs;
#[cfg(target_os = "linux")]
pub use libc::statvfs64;

#[cfg(target_os = "macos")]
pub type ino64_t = libc::ino_t;
#[cfg(target_os = "linux")]
pub use libc::ino64_t;

/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `buf` must be a valid pointer to at least `count` bytes of writable memory.
#[cfg(target_os = "linux")]
pub unsafe fn pread64(
    fd: libc::c_int,
    buf: *mut libc::c_void,
    count: libc::size_t,
    offset: off64_t,
) -> libc::ssize_t {
    libc::pread64(fd, buf, count, offset)
}
/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `buf` must be a valid pointer to at least `count` bytes of writable memory.
#[cfg(target_os = "macos")]
pub unsafe fn pread64(
    fd: libc::c_int,
    buf: *mut libc::c_void,
    count: libc::size_t,
    offset: off64_t,
) -> libc::ssize_t {
    libc::pread(fd, buf, count, offset)
}

/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `iov` must be a valid pointer to `iovcnt` initialized `iovec` structures, each with valid buffer pointers.
#[cfg(target_os = "linux")]
pub unsafe fn preadv64(
    fd: libc::c_int,
    iov: *const libc::iovec,
    iovcnt: libc::c_int,
    offset: off64_t,
) -> libc::ssize_t {
    libc::preadv64(fd, iov, iovcnt, offset)
}
/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `iov` must be a valid pointer to `iovcnt` initialized `iovec` structures, each with valid buffer pointers.
#[cfg(target_os = "macos")]
pub unsafe fn preadv64(
    fd: libc::c_int,
    iov: *const libc::iovec,
    iovcnt: libc::c_int,
    offset: off64_t,
) -> libc::ssize_t {
    libc::preadv(fd, iov, iovcnt, offset)
}

/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `buf` must be a valid pointer to at least `count` bytes of readable memory.
#[cfg(target_os = "linux")]
pub unsafe fn pwrite64(
    fd: libc::c_int,
    buf: *const libc::c_void,
    count: libc::size_t,
    offset: off64_t,
) -> libc::ssize_t {
    libc::pwrite64(fd, buf, count, offset)
}
/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `buf` must be a valid pointer to at least `count` bytes of readable memory.
#[cfg(target_os = "macos")]
pub unsafe fn pwrite64(
    fd: libc::c_int,
    buf: *const libc::c_void,
    count: libc::size_t,
    offset: off64_t,
) -> libc::ssize_t {
    libc::pwrite(fd, buf, count, offset)
}

/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `iov` must be a valid pointer to `iovcnt` initialized `iovec` structures, each with valid buffer pointers.
#[cfg(target_os = "linux")]
pub unsafe fn pwritev64(
    fd: libc::c_int,
    iov: *const libc::iovec,
    iovcnt: libc::c_int,
    offset: off64_t,
) -> libc::ssize_t {
    libc::pwritev64(fd, iov, iovcnt, offset)
}
/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `iov` must be a valid pointer to `iovcnt` initialized `iovec` structures, each with valid buffer pointers.
#[cfg(target_os = "macos")]
pub unsafe fn pwritev64(
    fd: libc::c_int,
    iov: *const libc::iovec,
    iovcnt: libc::c_int,
    offset: off64_t,
) -> libc::ssize_t {
    libc::pwritev(fd, iov, iovcnt, offset)
}

/// # Safety
/// - `dirfd` must be a valid open directory file descriptor, or `AT_FDCWD`.
/// - `pathname` must be a valid null-terminated C string.
/// - `buf` must be a valid pointer to a writable `stat64` structure.
#[cfg(target_os = "linux")]
pub unsafe fn fstatat64(
    dirfd: libc::c_int,
    pathname: *const libc::c_char,
    buf: *mut stat64,
    flags: libc::c_int,
) -> libc::c_int {
    libc::fstatat64(dirfd, pathname, buf, flags)
}
/// # Safety
/// - `dirfd` must be a valid open directory file descriptor, or `AT_FDCWD`.
/// - `pathname` must be a valid null-terminated C string.
/// - `buf` must be a valid pointer to a writable `stat64` structure.
#[cfg(target_os = "macos")]
pub unsafe fn fstatat64(
    dirfd: libc::c_int,
    pathname: *const libc::c_char,
    buf: *mut stat64,
    flags: libc::c_int,
) -> libc::c_int {
    libc::fstatat(dirfd, pathname, buf, flags)
}

/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `offset` and `len` must describe a valid byte range within the file.
#[cfg(target_os = "linux")]
pub unsafe fn fallocate64(
    fd: libc::c_int,
    mode: libc::c_int,
    offset: off64_t,
    len: off64_t,
) -> libc::c_int {
    libc::fallocate64(fd, mode, offset, len)
}
/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `offset` and `len` must describe a valid byte range within the file.
#[cfg(target_os = "macos")]
pub unsafe fn fallocate64(
    _fd: libc::c_int,
    _mode: libc::c_int,
    _offset: off64_t,
    _len: off64_t,
) -> libc::c_int {
    -LINUX_ENOSYS
}

/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `length` must be a valid non-negative file size.
#[cfg(target_os = "linux")]
pub unsafe fn ftruncate64(fd: libc::c_int, length: off64_t) -> libc::c_int {
    libc::ftruncate64(fd, length)
}
/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `length` must be a valid non-negative file size.
#[cfg(target_os = "macos")]
pub unsafe fn ftruncate64(fd: libc::c_int, length: off64_t) -> libc::c_int {
    libc::ftruncate(fd, length)
}

/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `whence` must be a valid seek directive (`SEEK_SET`, `SEEK_CUR`, or `SEEK_END`).
#[cfg(target_os = "linux")]
pub unsafe fn lseek64(fd: libc::c_int, offset: off64_t, whence: libc::c_int) -> off64_t {
    libc::lseek64(fd, offset, whence)
}
/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `whence` must be a valid seek directive (`SEEK_SET`, `SEEK_CUR`, or `SEEK_END`).
#[cfg(target_os = "macos")]
pub unsafe fn lseek64(fd: libc::c_int, offset: off64_t, whence: libc::c_int) -> off64_t {
    libc::lseek(fd, offset, whence)
}

/// # Safety
/// - `path` must be a valid null-terminated C string pointing to an accessible filesystem path.
/// - `buf` must be a valid pointer to a writable `statvfs64` struct.
#[cfg(target_os = "macos")]
pub unsafe fn statvfs64(path: *const libc::c_char, buf: *mut statvfs64) -> libc::c_int {
    libc::statvfs(path, buf)
}

/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `buf` must be a valid pointer to a writable `statvfs64` struct.
#[cfg(target_os = "linux")]
pub unsafe fn fstatvfs64(fd: libc::c_int, buf: *mut statvfs64) -> libc::c_int {
    libc::fstatvfs64(fd, buf)
}
/// # Safety
/// - `fd` must be a valid open file descriptor.
/// - `buf` must be a valid pointer to a writable `statvfs64` struct.
#[cfg(target_os = "macos")]
pub unsafe fn fstatvfs64(fd: libc::c_int, buf: *mut statvfs64) -> libc::c_int {
    libc::fstatvfs(fd, buf)
}

/// # Safety
/// - `dirfd` must be a valid open directory file descriptor, or `AT_FDCWD`.
/// - `pathname` must be a valid null-terminated C string.
#[cfg(target_os = "linux")]
pub unsafe fn mknodat(
    dirfd: libc::c_int,
    pathname: *const libc::c_char,
    mode: libc::mode_t,
    dev: libc::dev_t,
) -> libc::c_int {
    libc::mknodat(dirfd, pathname, mode, dev)
}
/// # Safety
/// - `dirfd` must be a valid open directory file descriptor, or `AT_FDCWD`.
/// - `pathname` must be a valid null-terminated C string.
#[cfg(target_os = "macos")]
pub unsafe fn mknodat(
    _dirfd: libc::c_int,
    _pathname: *const libc::c_char,
    _mode: libc::mode_t,
    _dev: u64,
) -> libc::c_int {
    -LINUX_ENOSYS
}
