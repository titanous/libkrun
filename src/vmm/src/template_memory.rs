//! File-backed RAM for cooperative guest templates.
//!
//! A caller may seal a template only after the guest has cooperatively frozen
//! and every writable shared mapping (including KVM slots) has been removed.
//! Linux enforces the latter condition when F_SEAL_WRITE is installed.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use vm_memory::mmap::MmapRegionBuilder;
use vm_memory::{FileOffset, MmapRegion};

/// Allocate RAM whose contents survive the VMM's mappings.
pub fn create(size: usize) -> io::Result<File> {
    if size == 0 || size > i64::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid RAM size",
        ));
    }
    // SAFETY: the name is terminated and ownership is transferred exactly once.
    let fd = unsafe {
        libc::memfd_create(
            c"guest-template".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: memfd_create returned an owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(size as u64)?;
    Ok(file)
}

/// Map a file extent as shared RAM, or as a private copy-on-write clone.
pub fn map(file: &File, offset: u64, size: usize, private: bool) -> io::Result<MmapRegion> {
    let end = offset
        .checked_add(size as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "RAM extent overflow"))?;
    if size == 0 || offset % 4096 != 0 || end > file.metadata()?.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid RAM extent",
        ));
    }
    MmapRegionBuilder::new(size)
        .with_file_offset(FileOffset::new(file.try_clone()?, offset))
        .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
        .with_mmap_flags(if private {
            libc::MAP_PRIVATE
        } else {
            libc::MAP_SHARED
        })
        .build()
        .map_err(io::Error::other)
}

/// Irreversibly prevent modifications and resizing of a memfd template.
///
/// Failure (including EBUSY from a remaining shared mapping) is fatal to
/// publication; callers must not expose an unsealed file as a template.
pub fn seal(file: &File) -> io::Result<()> {
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: fcntl does not take ownership of the valid descriptor.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    #[test]
    fn clones_are_private_and_source_is_immutable() {
        let file = create(8192).unwrap();
        {
            let shared = map(&file, 0, 8192, false).unwrap();
            // SAFETY: shared owns a writable 8192-byte mapping.
            unsafe { shared.as_ptr().write(37) };
            assert!(seal(&file).is_err(), "must reject a mutable template alias");
        }
        seal(&file).unwrap();
        assert!(file.write_at(&[99], 0).is_err());
        assert!(file.set_len(4096).is_err());
        assert!(map(&file, 0, 8192, false).is_err());
        let a = map(&file, 0, 8192, true).unwrap();
        let b = map(&file, 0, 8192, true).unwrap();
        // SAFETY: both mappings remain live and cover these bytes.
        unsafe {
            assert_eq!(a.as_ptr().read(), 37);
            assert_eq!(b.as_ptr().read(), 37);
            a.as_ptr().write(42);
            assert_eq!(b.as_ptr().read(), 37);
        }
        let mut source = [0];
        file.read_exact_at(&mut source, 0).unwrap();
        assert_eq!(source, [37]);
    }

    #[test]
    fn rejects_invalid_extents() {
        let file = create(4096).unwrap();
        assert!(create(0).is_err());
        assert!(map(&file, 1, 1, true).is_err());
        assert!(map(&file, 0, 0, true).is_err());
        assert!(map(&file, 4096, 4096, true).is_err());
        assert!(map(&file, u64::MAX - 4095, 8192, true).is_err());
    }
}
