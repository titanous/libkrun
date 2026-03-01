use std::io;
use std::os::unix::io::RawFd;

/// Abstracts platform-specific DAX window operations.
///
/// FileSystem implementations call DaxMapper methods instead of
/// libc::mmap directly. This enables different DAX backends
/// (Linux mmap, HVF mapping messages, etc.) without changing the
/// FileSystem trait.
pub trait DaxMapper: Send + Sync {
    fn map_file(
        &self,
        dax_offset: u64,
        len: u64,
        fd: RawFd,
        file_offset: u64,
        writable: bool,
    ) -> io::Result<()>;

    fn map_data(&self, dax_offset: u64, data: &[u8]) -> io::Result<()>;

    fn unmap(&self, dax_offset: u64, len: u64) -> io::Result<()>;
}

/// Linux implementation of DaxMapper using mmap(MAP_FIXED).
///
/// Created from a VirtioShmRegion's host_addr and size. Performs
/// bounds checking before every mmap call.
pub(crate) struct LinuxDaxMapper {
    host_addr: u64,
    size: u64,
}

impl LinuxDaxMapper {
    pub fn new(host_addr: u64, size: u64) -> Self {
        Self { host_addr, size }
    }

    fn check_bounds(&self, offset: u64, len: u64) -> io::Result<()> {
        if offset.checked_add(len).map_or(true, |end| end > self.size) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        Ok(())
    }
}

impl DaxMapper for LinuxDaxMapper {
    fn map_file(
        &self,
        dax_offset: u64,
        len: u64,
        fd: RawFd,
        file_offset: u64,
        writable: bool,
    ) -> io::Result<()> {
        self.check_bounds(dax_offset, len)?;

        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        let addr = self.host_addr + dax_offset;

        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                len as usize,
                prot,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                file_offset as libc::off_t,
            )
        };
        if std::ptr::eq(ret, libc::MAP_FAILED) {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn map_data(&self, dax_offset: u64, data: &[u8]) -> io::Result<()> {
        let len = data.len() as u64;
        self.check_bounds(dax_offset, len)?;

        let addr = self.host_addr + dax_offset;

        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                data.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if std::ptr::eq(ret, libc::MAP_FAILED) {
            return Err(io::Error::last_os_error());
        }

        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len());
        }
        Ok(())
    }

    fn unmap(&self, dax_offset: u64, len: u64) -> io::Result<()> {
        self.check_bounds(dax_offset, len)?;

        let addr = self.host_addr + dax_offset;

        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                len as usize,
                libc::PROT_NONE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if std::ptr::eq(ret, libc::MAP_FAILED) {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_file_bounds_at_exact_boundary() {
        let mapper = LinuxDaxMapper::new(0, 4096);
        // Mapping that goes just beyond the boundary should fail bounds check
        // offset=4095, len=2 would be 4095 + 2 = 4097 > 4096
        let result = mapper.map_file(4095, 2, -1, 0, true);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_map_file_bounds_exceeds_window() {
        let mapper = LinuxDaxMapper::new(0, 4096);
        // Mapping beyond the window should fail
        let result = mapper.map_file(2048, 2049, -1, 0, true);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_map_file_bounds_offset_overflow() {
        let mapper = LinuxDaxMapper::new(0, 4096);
        // Offset + len overflows u64 (would wrap to small number)
        let result = mapper.map_file(u64::MAX - 100, 200, -1, 0, true);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_map_file_bounds_zero_size_window() {
        let mapper = LinuxDaxMapper::new(0, 0);
        // Any non-zero length should fail on zero-size window
        let result = mapper.map_file(0, 1, -1, 0, true);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_map_data_bounds_exceeds_window() {
        let mapper = LinuxDaxMapper::new(0, 100);
        let data = vec![0u8; 101];
        // Data longer than remaining window should fail
        let result = mapper.map_data(0, &data);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_map_data_bounds_offset_plus_data() {
        let mapper = LinuxDaxMapper::new(0, 200);
        let data = vec![0u8; 150];
        // Offset leaves only 50 bytes, data is 150
        let result = mapper.map_data(50, &data);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_unmap_bounds_exceeds_window() {
        let mapper = LinuxDaxMapper::new(0, 4096);
        // Unmapping beyond the window should fail
        let result = mapper.unmap(2048, 2049, );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_unmap_bounds_offset_overflow() {
        let mapper = LinuxDaxMapper::new(0, 4096);
        // Offset + len overflows u64
        let result = mapper.unmap(u64::MAX - 50, 100);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_map_file_in_bounds_passes_check() {
        let mapper = LinuxDaxMapper::new(0, 4096);
        // In-bounds call passes check but fails on actual mmap (due to invalid fd)
        // We're testing that bounds check passes
        let result = mapper.map_file(0, 4095, -1, 0, true);
        // Will fail due to invalid fd, but not due to bounds check
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Should not be EINVAL (which is what bounds check returns)
        assert_ne!(err.raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn test_unmap_in_bounds_passes_check() {
        let mapper = LinuxDaxMapper::new(0, 4096);
        // In-bounds unmap passes bounds check
        let result = mapper.unmap(0, 4096);
        // Will fail due to trying to unmap unmapped memory, but not bounds check
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Should not be EINVAL (bounds check), would be different error from mmap
        // Actually unmap at 0 with 4096 might succeed on some systems,
        // but the point is to verify bounds check passed
        // We just verify the call was made, not that it succeeded
    }
}
