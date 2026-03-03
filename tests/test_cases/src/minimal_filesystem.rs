//! Minimal FileSystem implementation for testing.
//!
//! Implements only lookup and read; all other methods return ENOSYS (the
//! default for the FileSystem trait). Used by test_virtiofs_minimal.rs to
//! verify that a FileSystem impl with minimum viable surface mounts correctly
//! and serves files from an in-memory directory.
//!
//! This is host-only: the FileSystem trait is only available under feature = "host"
//! (because it lives in the libkrun crate dependency).

use std::collections::HashMap;
use std::ffi::CStr;
use std::io;
use std::sync::RwLock;

use krun::{
    FilesystemContext as Context, DirEntry, Entry, FileSystem, Handle, Inode, OpenOptions,
    ZeroCopyReader, ZeroCopyWriter,
};

/// A minimal read-only in-memory filesystem.
///
/// Files are registered at construction time as a flat directory (root inode = 1).
/// Lookup by name resolves to an inode; read returns the stored bytes.
/// All other operations return ENOSYS.
pub struct MinimalFileSystem {
    /// Map from filename to (inode, content)
    files: HashMap<String, (Inode, Vec<u8>)>,
    /// Map from inode to content (for reads)
    inodes: RwLock<HashMap<Inode, Vec<u8>>>,
    /// Next inode to allocate
    next_inode: std::sync::atomic::AtomicU64,
}

impl MinimalFileSystem {
    /// Create a filesystem with the given named files.
    ///
    /// # Example
    /// ```ignore
    /// let fs = MinimalFileSystem::new(vec![
    ///     ("hello.txt", b"hello world".to_vec()),
    /// ]);
    /// ```
    pub fn new(files: Vec<(&str, Vec<u8>)>) -> Self {
        let mut file_map = HashMap::new();
        let mut inode_map = HashMap::new();
        let mut next_ino = 2u64; // inode 1 is root

        for (name, data) in files {
            let ino = next_ino;
            next_ino += 1;
            file_map.insert(name.to_string(), (ino, data.clone()));
            inode_map.insert(ino, data);
        }

        MinimalFileSystem {
            files: file_map,
            inodes: RwLock::new(inode_map),
            next_inode: std::sync::atomic::AtomicU64::new(next_ino),
        }
    }
}

impl FileSystem for MinimalFileSystem {
    fn lookup(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        // Only support lookups in root (inode 1)
        if parent != 1 {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        let name_str = name.to_str().map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        if let Some(&(ino, ref data)) = self.files.get(name_str) {
            Ok(Entry {
                inode: ino,
                generation: 0,
                attr: file_attr(ino, data.len() as u64),
                attr_flags: 0,
                attr_timeout: std::time::Duration::from_secs(3600),
                entry_timeout: std::time::Duration::from_secs(3600),
            })
        } else {
            Err(io::Error::from_raw_os_error(libc::ENOENT))
        }
    }

    fn read(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Handle,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let inodes = self.inodes.read().unwrap();
        let data = inodes
            .get(&inode)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;

        let start = offset as usize;
        if start >= data.len() {
            return Ok(0);
        }
        let end = (start + size as usize).min(data.len());
        let slice = &data[start..end];
        w.write_all(slice)?;
        Ok(slice.len())
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Option<Handle>,
    ) -> io::Result<(libc::stat64, std::time::Duration)> {
        // Root inode
        if inode == 1 {
            let mut attr: libc::stat64 = unsafe { std::mem::zeroed() };
            attr.st_ino = 1;
            attr.st_mode = libc::S_IFDIR | 0o755;
            attr.st_nlink = 2;
            return Ok((attr, std::time::Duration::from_secs(3600)));
        }
        let inodes = self.inodes.read().unwrap();
        let data = inodes
            .get(&inode)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok((file_attr(inode, data.len() as u64), std::time::Duration::from_secs(3600)))
    }

    fn open(
        &self,
        _ctx: Context,
        _inode: Inode,
        _flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        // No handle needed for a simple in-memory read
        Ok((None, OpenOptions::empty()))
    }

    fn opendir(
        &self,
        _ctx: Context,
        _inode: Inode,
        _flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        Ok((None, OpenOptions::empty()))
    }

    fn readdir(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Handle,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
    ) -> io::Result<()> {
        if inode != 1 {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let mut cur_offset = 0u64;
        for (name, &(ino, ref data)) in &self.files {
            cur_offset += 1;
            if cur_offset <= offset {
                continue;
            }
            let entry = DirEntry {
                ino,
                offset: cur_offset,
                type_: libc::DT_REG as u32,
                name: name.as_bytes(),
            };
            let attr = Entry {
                inode: ino,
                generation: 0,
                attr: file_attr(ino, data.len() as u64),
                attr_flags: 0,
                attr_timeout: std::time::Duration::from_secs(3600),
                entry_timeout: std::time::Duration::from_secs(3600),
            };
            match add_entry(entry, attr) {
                Ok(0) => break, // buffer full
                Ok(_) => {}
                Err(e) => return Err(e),
            }
            let _ = size; // size check handled by fuse layer
        }
        Ok(())
    }
}

fn file_attr(inode: Inode, size: u64) -> libc::stat64 {
    let mut attr: libc::stat64 = unsafe { std::mem::zeroed() };
    attr.st_ino = inode;
    attr.st_mode = libc::S_IFREG | 0o644;
    attr.st_nlink = 1;
    attr.st_size = size as libc::off64_t;
    attr
}
