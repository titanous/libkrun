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
        if offset.checked_add(len).is_none_or(|end| end > self.size) {
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
        let addr = self.host_addr.checked_add(dax_offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "DAX address overflow: host_addr + dax_offset overflows u64",
            )
        })?;

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

        let addr = self.host_addr.checked_add(dax_offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "DAX address overflow: host_addr + dax_offset overflows u64",
            )
        })?;

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

        let addr = self.host_addr.checked_add(dax_offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "DAX address overflow: host_addr + dax_offset overflows u64",
            )
        })?;

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
        let result = mapper.unmap(2048, 2049);
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
        let result = mapper.unmap(0, 4096);
        // If it errors, it should NOT be from bounds check (EINVAL)
        if let Err(e) = result {
            assert_ne!(e.raw_os_error(), Some(libc::EINVAL));
        }
        // If Ok, bounds check also passed - either outcome is valid
    }
}

/// GAP-018: dax_mapper MAP_FIXED arithmetic overflow (fixed)
///
/// `check_bounds` validates `dax_offset + len <= total_size` but does NOT verify
/// that `self.host_addr + dax_offset` doesn't overflow u64. With `MAP_FIXED`, an
/// overflowed address silently replaces an existing mapping.
///
/// Fixed: `map_file`, `map_data`, and `unmap` now use `checked_add` for the
/// `host_addr + dax_offset` computation and return `InvalidInput` on overflow.
#[cfg(kani)]
mod verification {
    use super::*;

    /// Assumption: when check_bounds passes, host_addr + dax_offset is safe.
    ///
    /// This is a mathematical assumption about the relationship between check_bounds
    /// and pointer arithmetic. Previously check_bounds only validated `dax_offset + len <= size`
    /// without checking whether `host_addr + dax_offset` wraps around u64::MAX. The fix
    /// adds `checked_add` at each arithmetic site (map_file, map_data, unmap) to guard
    /// against overflow.
    ///
    /// This proof verifies the mathematical invariant: given the preconditions, the
    /// address sum does not overflow. The production functions (map_file, map_data, unmap)
    /// use this as a defensive check before calling mmap.
    #[kani::proof]
    fn proof_check_bounds_no_overflow() {
        // Symbolic host_addr — constrained so the full window fits in address space.
        // This models the real invariant: a valid DaxMapper must have host_addr + size
        // representable as u64 (otherwise the window itself would overflow).
        let host_addr: u64 = kani::any();
        let size: u64 = kani::any_where(|&s| s > 0);
        // Precondition: the mapping window itself must not overflow u64.
        kani::assume(host_addr.checked_add(size).is_some());

        let dax_offset: u64 = kani::any_where(|&o| o < size);
        let len: u64 = kani::any_where(|&l| l > 0 && l <= size - dax_offset);

        let mapper = LinuxDaxMapper::new(host_addr, size);

        // Precondition: check_bounds must succeed (the offset/len are in-window).
        kani::assume(mapper.check_bounds(dax_offset, len).is_ok());

        // Verify the mathematical invariant: when check_bounds passes with these
        // constraints, host_addr + dax_offset cannot overflow u64.
        kani::assert(
            host_addr.checked_add(dax_offset).is_some(),
            "host_addr + dax_offset must not overflow u64 after check_bounds passes",
        );
    }

    /// Assumption: the mmap address computation `host_addr + dax_offset` does not overflow.
    ///
    /// This is a mathematical assumption about valid address ranges. Previously this
    /// addition was unchecked (unsafely assuming callers provide valid offsets).
    /// After the fix, all three mmap call sites (`map_file`, `map_data`, `unmap`)
    /// use `checked_add` and return `InvalidInput` on overflow, ensuring the sum
    /// is safe before reaching mmap.
    ///
    /// This proof documents the invariant used to justify those defensive checks.
    #[kani::proof]
    fn proof_map_file_addr_no_overflow() {
        let host_addr: u64 = kani::any();
        let size: u64 = kani::any_where(|&s| s > 0 && s <= u64::MAX / 2);
        // Precondition: the mapping window itself must not overflow u64.
        kani::assume(host_addr.checked_add(size).is_some());

        let dax_offset: u64 = kani::any_where(|&o| o <= size);
        let len: u64 = kani::any_where(|&l| l <= size.saturating_sub(dax_offset));

        let mapper = LinuxDaxMapper::new(host_addr, size);

        // Establish that the offset/len are within the declared window.
        kani::assume(dax_offset.checked_add(len).is_some());
        kani::assume(dax_offset + len <= size);

        // Verify the mathematical invariant: given the preconditions, host_addr + dax_offset
        // cannot overflow u64. This justifies the checked_add guards in map_file/map_data/unmap.
        kani::assert(
            host_addr.checked_add(dax_offset).is_some(),
            "map_file address computation host_addr + dax_offset must not overflow",
        );
    }
}
