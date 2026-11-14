#![cfg(feature = "blk")]
use krun::{
    BlockBackend, CacheType, Entry, FileSystem, FilesystemContext, Handle, Inode, VolatileSlice,
    ZeroCopyWriter,
};
use std::io;
use std::sync::Mutex;
use std::time::Duration;

pub struct MemoryDisk(pub Mutex<Vec<u8>>);
impl BlockBackend for MemoryDisk {
    fn cache_type(&self) -> CacheType {
        CacheType::Writeback
    }
    fn nsectors(&self) -> u64 {
        (self.0.lock().unwrap().len() / 512) as u64
    }
    fn image_id(&self) -> &[u8] {
        b"embed-memory-disk"
    }
    fn read_vectored_at(&self, bufs: &[VolatileSlice], offset: u64) -> io::Result<usize> {
        let data = self.0.lock().unwrap();
        let mut pos = offset as usize;
        for buf in bufs {
            let end = pos + buf.len();
            if end > data.len() {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            buf.copy_from(&data[pos..end]);
            pos = end;
        }
        Ok(pos - offset as usize)
    }
    fn write_vectored_at(&self, bufs: &[VolatileSlice], offset: u64) -> io::Result<usize> {
        let mut data = self.0.lock().unwrap();
        let mut pos = offset as usize;
        for buf in bufs {
            let end = pos + buf.len();
            if end > data.len() {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            buf.copy_to(&mut data[pos..end]);
            pos = end;
        }
        Ok(pos - offset as usize)
    }
}

pub struct FailingTree;
impl FileSystem for FailingTree {
    fn init(
        &self,
        _: devices::virtio::fs::filesystem::FsOptions,
    ) -> io::Result<devices::virtio::fs::filesystem::FsOptions> {
        Err(io::Error::from_raw_os_error(libc::EIO))
    }
}

pub struct ReadOnlyTree;
impl ReadOnlyTree {
    fn attr(inode: Inode) -> libc::stat64 {
        // SAFETY: all-zero is a valid initial stat structure.
        let mut attr: libc::stat64 = unsafe { std::mem::zeroed() };
        attr.st_ino = inode.into();
        attr.st_nlink = 1;
        attr.st_blksize = 4096;
        attr.st_mode = if inode == Inode(1) {
            libc::S_IFDIR | 0o555
        } else {
            libc::S_IFREG | 0o444
        };
        attr.st_size = if inode == Inode(1) { 0 } else { 13 };
        attr
    }
}
impl FileSystem for ReadOnlyTree {
    fn setupmapping(
        &self,
        _: FilesystemContext,
        _: Inode,
        _: Handle,
        offset: u64,
        len: u64,
        _: u64,
        mapped: u64,
        mapper: &dyn krun::dax_mapper::DaxMapper,
    ) -> io::Result<()> {
        let mut bytes = vec![0; usize::try_from(len).map_err(io::Error::other)?];
        let data = b"embed-fs-data";
        let offset = usize::try_from(offset).map_err(io::Error::other)?;
        if offset < data.len() {
            let count = bytes.len().min(data.len() - offset);
            bytes[..count].copy_from_slice(&data[offset..offset + count]);
        }
        mapper.map_data(mapped, &bytes)
    }
    fn removemapping(
        &self,
        _: FilesystemContext,
        requests: Vec<devices::virtio::fs::filesystem::RemovemappingOne>,
        mapper: &dyn krun::dax_mapper::DaxMapper,
    ) -> io::Result<()> {
        for request in requests {
            mapper.unmap(request.moffset, request.len)?;
        }
        Ok(())
    }
    fn lookup(
        &self,
        _: FilesystemContext,
        parent: Inode,
        name: &std::ffi::CStr,
    ) -> io::Result<Entry> {
        if parent != Inode(1) || name.to_bytes() != b"hello" {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        Ok(Entry {
            inode: 2,
            generation: 1,
            attr: Self::attr(Inode(2)),
            attr_flags: 0,
            attr_timeout: Duration::ZERO,
            entry_timeout: Duration::ZERO,
        })
    }
    fn getattr(
        &self,
        _: FilesystemContext,
        inode: Inode,
        _: Option<Handle>,
    ) -> io::Result<(libc::stat64, Duration)> {
        Ok((Self::attr(inode), Duration::ZERO))
    }
    fn read(
        &self,
        _: FilesystemContext,
        _: Inode,
        _: Handle,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        _: Option<u64>,
        _: u32,
    ) -> io::Result<usize> {
        let data = b"embed-fs-data";
        let start = (offset as usize).min(data.len());
        let end = (start + size as usize).min(data.len());
        w.write(&data[start..end])
    }
}
