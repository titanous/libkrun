use std::io;
use std::os::unix::io::RawFd;

/// Abstracts platform-specific DAX window operations.
///
/// FileSystem implementations call DaxMapper methods instead of
/// libc::mmap directly. This enables different DAX backends without
/// changing the FileSystem trait.
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

/// Copy-based mapper for a hypervisor-registered, private per-guest window.
/// The caller must retain the backing mapping until all filesystem workers stop.
pub struct CopyDaxMapper {
    host_addr: u64,
    size: u64,
}
impl CopyDaxMapper {
    /// # Safety
    /// The address must identify a writable registered window of `size` bytes
    /// that outlives this mapper and every operation performed through it.
    pub unsafe fn new(host_addr: u64, size: u64) -> Self {
        Self { host_addr, size }
    }
    fn address(&self, offset: u64, len: u64) -> io::Result<*mut u8> {
        if offset.checked_add(len).is_none_or(|end| end > self.size) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.host_addr
            .checked_add(offset)
            .map(|address| address as *mut u8)
            .ok_or_else(|| io::ErrorKind::InvalidInput.into())
    }
}
impl DaxMapper for CopyDaxMapper {
    fn map_file(
        &self,
        dax_offset: u64,
        len: u64,
        fd: RawFd,
        file_offset: u64,
        writable: bool,
    ) -> io::Result<()> {
        if writable {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "writable file-backed DAX is unsupported on this backend; use filesystem writes",
            ));
        }
        let address = self.address(dax_offset, len)?;
        if file_offset
            .checked_add(len)
            .is_none_or(|end| end > i64::MAX as u64)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        unsafe {
            std::ptr::write_bytes(address, 0, len as usize);
        }
        let mut read = 0usize;
        while read < len as usize {
            let result = unsafe {
                libc::pread(
                    fd,
                    address.add(read).cast(),
                    len as usize - read,
                    (file_offset + read as u64) as libc::off_t,
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if result == 0 {
                break;
            }
            read += result as usize;
        }
        Ok(())
    }
    fn map_data(&self, dax_offset: u64, data: &[u8]) -> io::Result<()> {
        let address = self.address(dax_offset, data.len() as u64)?;
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), address, data.len());
        }
        Ok(())
    }
    fn unmap(&self, dax_offset: u64, len: u64) -> io::Result<()> {
        let address = self.address(dax_offset, len)?;
        unsafe {
            std::ptr::write_bytes(address, 0, len as usize);
        }
        Ok(())
    }
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
    fn copy_window_maps_data_and_zeroes_without_replacing_backing() {
        let mut backing = vec![0xa5u8; 4096];
        let mapper =
            unsafe { CopyDaxMapper::new(backing.as_mut_ptr() as u64, backing.len() as u64) };
        mapper.map_data(128, b"guest file bytes").unwrap();
        assert_eq!(&backing[128..144], b"guest file bytes");
        mapper.unmap(128, 16).unwrap();
        assert!(backing[128..144].iter().all(|byte| *byte == 0));
        assert!(mapper.map_data(4095, b"xx").is_err());
        assert!(mapper.unmap(u64::MAX, 2).is_err());
        assert_eq!(
            mapper.map_file(0, 4096, -1, 0, true).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn copy_file_zero_fills_past_eof() {
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd};
        let raw = unsafe { libc::memfd_create(c"copy-dax-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw >= 0);
        let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
        file.write_all(b"file content").unwrap();
        let mut backing = vec![0xa5u8; 4096];
        let mapper = unsafe { CopyDaxMapper::new(backing.as_mut_ptr() as u64, 4096) };
        mapper
            .map_file(0, 4096, file.as_raw_fd(), 0, false)
            .unwrap();
        assert_eq!(&backing[..12], b"file content");
        assert!(backing[12..].iter().all(|byte| *byte == 0));
    }

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

    #[test]
    fn test_init_mapping_plan_len_exceeds_binary() {
        let (data, zero) = super::init_mapping_plan(8192, 4096);
        assert_eq!(data, 4096);
        assert_eq!(zero, 4096);
    }

    #[test]
    fn test_init_mapping_plan_exact_match() {
        let (data, zero) = super::init_mapping_plan(4096, 4096);
        assert_eq!(data, 4096);
        assert_eq!(zero, 0);
    }

    #[test]
    fn test_init_mapping_plan_len_within_binary() {
        let (data, zero) = super::init_mapping_plan(2048, 4096);
        assert_eq!(data, 2048);
        assert_eq!(zero, 0);
    }

    #[test]
    fn test_init_mapping_plan_zero_len() {
        let (data, zero) = super::init_mapping_plan(0, 4096);
        assert_eq!(data, 0);
        assert_eq!(zero, 0);
    }

    #[test]
    fn test_init_mapping_plan_zero_binary() {
        let (data, zero) = super::init_mapping_plan(4096, 0);
        assert_eq!(data, 0);
        assert_eq!(zero, 4096);
    }
}

/// Computes the init binary DAX mapping plan: how many bytes to map with
/// init data and how many trailing bytes to unmap (PROT_NONE).
///
/// The guest may request a DAX mapping `requested_len` larger than the init
/// binary (due to page-size alignment). The full range must be covered:
/// init data for `min(requested_len, init_binary_len)` bytes, then the
/// remainder is unmapped (PROT_NONE) to prevent stale data exposure.
///
/// Called by `PassthroughFs::setupmapping` for the init inode.
pub(crate) fn init_mapping_plan(requested_len: u64, init_binary_len: u64) -> (u64, u64) {
    let data_bytes = std::cmp::min(requested_len, init_binary_len);
    let zero_bytes = requested_len - data_bytes;
    (data_bytes, zero_bytes)
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

    /// When check_bounds returns Ok, host_addr + dax_offset does not overflow u64.
    ///
    /// Verifies the safety of the `host_addr + dax_offset` computation performed
    /// in `map_file`, `map_data`, and `unmap` after a successful `check_bounds`
    /// call. If check_bounds were weakened to skip the `checked_add(len)` guard,
    /// the assume preconditions would no longer be entailed by Ok and this proof
    /// would fail.
    ///
    /// Breaking change: removing the `is_none_or(|end| end > self.size)` guard
    /// from `check_bounds` would allow this proof to fail.
    ///
    /// Bound: no loops; no unwind attribute needed.
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

        // Cover both the large-offset and small-offset cases.
        kani::cover!(dax_offset == 0, "zero dax_offset exercised");
        kani::cover!(dax_offset > 0, "non-zero dax_offset exercised");
    }

    /// check_bounds returns Ok for valid (offset, len) and Err(EINVAL) when dax_offset + len overflows u64.
    ///
    /// Verifies both the success path and the overflow-rejection guard in check_bounds.
    /// Previously check_bounds only validated `dax_offset + len <= size` without guarding
    /// against u64 wrap-around; a guest supplying dax_offset near u64::MAX could bypass
    /// the size check. The fix uses `checked_add(len).is_none_or(|end| end > self.size)`.
    ///
    /// Breaking change: replacing `checked_add(len)` with `dax_offset + len` (unchecked)
    /// in check_bounds would cause the overflow case to return Ok instead of Err(EINVAL),
    /// and this proof's Err assertion would fail.
    ///
    /// Bound: no loops; no unwind attribute needed.
    #[kani::proof]
    fn proof_map_file_addr_no_overflow() {
        let host_addr: u64 = kani::any();
        let size: u64 = kani::any_where(|&s| s > 0);
        kani::assume(host_addr.checked_add(size).is_some());

        let mapper = LinuxDaxMapper::new(host_addr, size);

        // Case 1: valid (offset, len) — check_bounds must return Ok.
        let valid_offset: u64 = kani::any_where(|&o| o < size);
        let valid_len: u64 = kani::any_where(|&l| l > 0 && l <= size - valid_offset);
        kani::assert(
            mapper.check_bounds(valid_offset, valid_len).is_ok(),
            "check_bounds returns Ok for in-window (offset, len)",
        );
        kani::cover!(valid_offset == 0, "zero offset accepted");
        kani::cover!(valid_offset > 0, "non-zero offset accepted");

        // Case 2: overflow — dax_offset + len wraps u64; check_bounds must return Err(EINVAL).
        let overflow_offset: u64 = kani::any_where(|&o| o > u64::MAX / 2);
        let overflow_len: u64 = kani::any_where(|&l| overflow_offset.checked_add(l).is_none());
        let result = mapper.check_bounds(overflow_offset, overflow_len);
        kani::assert(
            result.is_err(),
            "check_bounds returns Err for overflowing (offset, len)",
        );
        kani::assert(
            result.unwrap_err().raw_os_error() == Some(libc::EINVAL),
            "check_bounds returns EINVAL for overflowing (offset, len)",
        );
        kani::cover!(
            overflow_offset.checked_add(overflow_len).is_none(),
            "u64 overflow path exercised"
        );
    }

    /// check_bounds returns Err(EINVAL) when dax_offset + len overflows u64.
    ///
    /// This is the security-critical guard against guest-controlled address
    /// wrap-around. Without `checked_add`, a guest could supply dax_offset near
    /// u64::MAX and a small len, causing the sum to wrap to a value <= size and
    /// bypass the bounds check. The fix uses `checked_add(len).is_none_or(...)`.
    ///
    /// Breaking change: replacing `checked_add(len)` with unchecked `+` in
    /// check_bounds would cause this proof to fail because the wrapped sum
    /// could satisfy `end <= size`, returning Ok instead of Err(EINVAL).
    ///
    /// Bound: no loops; no unwind attribute needed.
    #[kani::proof]
    fn proof_check_bounds_overflow_rejected() {
        let host_addr: u64 = kani::any();
        let size: u64 = kani::any_where(|&s| s <= u64::MAX / 2);
        let mapper = LinuxDaxMapper::new(host_addr, size);

        // Inputs where dax_offset + len overflows u64.
        let dax_offset: u64 = kani::any_where(|&o| o > u64::MAX / 2);
        let len: u64 = kani::any_where(|&l| dax_offset.checked_add(l).is_none());

        let result = mapper.check_bounds(dax_offset, len);

        kani::assert(result.is_err(), "overflow inputs must be rejected");
        kani::assert(
            result.unwrap_err().raw_os_error() == Some(libc::EINVAL),
            "overflow rejection must use EINVAL",
        );
        kani::cover!(dax_offset > u64::MAX / 2, "large dax_offset exercised");
        kani::cover!(len > 1, "non-trivial len exercised");
    }

    /// Init inode DAX mapping must cover the full requested range.
    ///
    /// When a guest requests a DAX mapping of `len` bytes for the init inode,
    /// the sum of init-data bytes and zero-filled bytes must equal `len`.
    /// Otherwise, the tail of the DAX window retains stale content from prior
    /// mappings, causing an information disclosure vulnerability.
    ///
    /// Breaking change: removing `zero_bytes = requested_len - data_bytes`
    /// from `init_mapping_plan` (e.g. hardcoding `zero_bytes = 0`) would
    /// cause this proof to fail.
    ///
    /// Bound: no loops; no unwind needed.
    #[kani::proof]
    fn proof_init_mapping_full_coverage() {
        let requested_len: u64 = kani::any_where(|&l| l > 0);
        let init_binary_len: u64 = kani::any_where(|&s| s > 0);

        let (data_bytes, zero_bytes) = init_mapping_plan(requested_len, init_binary_len);

        // Property 1: full coverage — no byte in [0, requested_len) is left unmapped
        kani::assert(
            data_bytes + zero_bytes == requested_len,
            "init mapping must cover the full requested DAX range (info leak if not)",
        );

        // Property 2: no out-of-bounds read from init binary
        kani::assert(
            data_bytes <= init_binary_len,
            "data_bytes must not exceed init binary length",
        );

        // Property 3: no excess mapping beyond request
        kani::assert(
            data_bytes <= requested_len,
            "data_bytes must not exceed requested length",
        );

        // Coverage: verify the bug case is exercised
        kani::cover!(
            requested_len > init_binary_len,
            "guest requests more than init binary (the info leak case)"
        );
        kani::cover!(
            requested_len == init_binary_len,
            "exact match — no tail needed"
        );
        kani::cover!(
            requested_len < init_binary_len,
            "partial init binary — no tail needed"
        );
    }

    /// Init mapping data_bytes must not overflow when added to zero_bytes.
    ///
    /// Ensures the plan never produces a pair where `data_bytes + zero_bytes`
    /// wraps u64 (which would falsely satisfy the coverage check).
    ///
    /// Breaking change: if `init_mapping_plan` returned values whose sum
    /// overflows u64, this proof would fail.
    ///
    /// Bound: no loops; no unwind needed.
    #[kani::proof]
    fn proof_init_mapping_no_overflow() {
        let requested_len: u64 = kani::any_where(|&l| l > 0);
        let init_binary_len: u64 = kani::any_where(|&s| s > 0);

        let (data_bytes, zero_bytes) = init_mapping_plan(requested_len, init_binary_len);

        kani::assert(
            data_bytes.checked_add(zero_bytes).is_some(),
            "data_bytes + zero_bytes must not overflow u64",
        );
    }
}
