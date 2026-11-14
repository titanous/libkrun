//! In-memory filesystem fixture with stable node IDs/handles across fresh servers.
use devices::virtio::fs::{
    dax_mapper::DaxMapper,
    fuse::{FsOptions, RemovemappingOne},
};
use krun::{
    Entry, FileSystem, FilesystemContext, Handle, Inode, OpenOptions, ZeroCopyReader,
    ZeroCopyWriter,
};
use std::{ffi::CStr, io, sync::Mutex, time::Duration};

pub struct Tree {
    data: Mutex<Vec<u8>>,
    readonly: bool,
}
impl Tree {
    pub fn new(readonly: bool) -> Self {
        Self {
            data: Mutex::new(vec![b'A'; 4096]),
            readonly,
        }
    }
    fn attr(&self, inode: u64) -> libc::stat64 {
        let mut attr: libc::stat64 = unsafe { std::mem::zeroed() };
        attr.st_ino = inode;
        attr.st_nlink = 1;
        attr.st_blksize = 4096;
        attr.st_mode = if inode == 1 {
            libc::S_IFDIR | 0o755
        } else {
            libc::S_IFREG | if self.readonly { 0o444 } else { 0o644 }
        };
        attr.st_size = if inode == 1 { 0 } else { 4096 };
        attr
    }
}
impl FileSystem for Tree {
    fn init(&self, _: FsOptions) -> io::Result<FsOptions> {
        eprintln!("CLONE FS INIT readonly={}", self.readonly);
        Ok(FsOptions::empty())
    }
    fn lookup(&self, _: FilesystemContext, parent: Inode, name: &CStr) -> io::Result<Entry> {
        if parent.0 != 1 || name.to_bytes() != b"file" {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        Ok(Entry {
            inode: 2,
            generation: 1,
            attr: self.attr(2),
            attr_flags: 0,
            attr_timeout: Duration::from_secs(3600),
            entry_timeout: Duration::from_secs(3600),
        })
    }
    fn getattr(
        &self,
        _: FilesystemContext,
        inode: Inode,
        _: Option<Handle>,
    ) -> io::Result<(libc::stat64, Duration)> {
        Ok((self.attr(inode.0), Duration::from_secs(3600)))
    }
    fn open(
        &self,
        _: FilesystemContext,
        inode: Inode,
        _: bool,
        _: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        Ok((Some(Handle(inode.0)), OpenOptions::empty()))
    }
    fn read(
        &self,
        _: FilesystemContext,
        inode: Inode,
        handle: Handle,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        _: Option<u64>,
        _: u32,
    ) -> io::Result<usize> {
        if inode.0 != 2 || handle.0 != 2 {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let bytes = self.data.lock().unwrap();
        let start = (offset as usize).min(bytes.len());
        let end = (start + size as usize).min(bytes.len());
        w.write(&bytes[start..end])
    }
    fn write(
        &self,
        _: FilesystemContext,
        _: Inode,
        _: Handle,
        r: &mut dyn ZeroCopyReader,
        size: u32,
        offset: u64,
        _: Option<u64>,
        _: bool,
        _: bool,
        _: u32,
    ) -> io::Result<usize> {
        if self.readonly {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        let mut bytes = vec![0; size as usize];
        r.read_exact(&mut bytes)?;
        let mut data = self.data.lock().unwrap();
        let start = offset as usize;
        if start + bytes.len() > data.len() {
            return Err(io::Error::from_raw_os_error(libc::EFBIG));
        }
        data[start..start + bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
    fn setupmapping(
        &self,
        _: FilesystemContext,
        _: Inode,
        _: Handle,
        offset: u64,
        len: u64,
        _: u64,
        moffset: u64,
        mapper: &dyn DaxMapper,
    ) -> io::Result<()> {
        let data = self.data.lock().unwrap();
        let mut bytes = vec![0; len as usize];
        let start = (offset as usize).min(data.len());
        let count = (data.len() - start).min(bytes.len());
        bytes[..count].copy_from_slice(&data[start..start + count]);
        mapper.map_data(moffset, &bytes)
    }
    fn removemapping(
        &self,
        _: FilesystemContext,
        requests: Vec<RemovemappingOne>,
        mapper: &dyn DaxMapper,
    ) -> io::Result<()> {
        for r in requests {
            mapper.unmap(r.moffset, r.len)?;
        }
        Ok(())
    }
}
