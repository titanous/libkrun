use std::io;

pub fn ebadf() -> io::Error {
    io::Error::from_raw_os_error(libc::EBADF)
}

pub fn einval() -> io::Error {
    io::Error::from_raw_os_error(libc::EINVAL)
}

/// Get the system page size in bytes using `sysconf(_SC_PAGESIZE)`.
///
/// Panics if `sysconf` returns a non-positive value, which would indicate a broken
/// system configuration.
pub fn system_page_size() -> u64 {
    // SAFETY: sysconf is a pure query with no side-effects. The return value is
    // checked before use; -1 (error) or 0 (impossible but defensive) both cause a panic.
    let result = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if result <= 0 {
        panic!("sysconf(_SC_PAGESIZE) returned non-positive value: {result}");
    }
    result as u64
}
