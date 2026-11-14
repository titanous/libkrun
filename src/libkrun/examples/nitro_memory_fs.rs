//! Small deterministic filesystem for Nitro acceptance examples, not production storage.
use devices::virtio::fs::filesystem::{Extensions, RemovemappingOne};
use krun::{
    Entry, FileSystem, FilesystemContext as Context, Handle, Inode, OpenOptions, ZeroCopyReader,
    ZeroCopyWriter,
};
use std::collections::HashMap;
use std::ffi::CStr;
use std::io;
use std::sync::Mutex;
use std::time::Duration;
#[derive(Default)]
pub struct Tree(Mutex<State>);
#[derive(Default)]
struct State {
    names: HashMap<Vec<u8>, u64>,
    files: HashMap<u64, Vec<u8>>,
    next: u64,
}
impl Tree {
    fn attr(&self, inode: u64) -> libc::stat64 {
        let mut attr: libc::stat64 = unsafe { std::mem::zeroed() };
        attr.st_ino = inode;
        attr.st_nlink = 1;
        attr.st_blksize = 4096;
        attr.st_mode = if inode == 1 {
            libc::S_IFDIR | 0o755
        } else {
            libc::S_IFREG | 0o644
        };
        attr.st_size = if inode == 2 {
            64 * 1024 * 1024
        } else if (10..1010).contains(&inode) {
            16
        } else {
            self.0
                .lock()
                .unwrap()
                .files
                .get(&inode)
                .map_or(0, |data| data.len() as i64)
        };
        attr
    }
    fn entry(&self, inode: u64) -> Entry {
        Entry {
            inode,
            generation: 1,
            attr: self.attr(inode),
            attr_flags: 0,
            attr_timeout: Duration::ZERO,
            entry_timeout: Duration::ZERO,
        }
    }
    fn bytes(&self, inode: u64, offset: usize, size: usize) -> Vec<u8> {
        if inode == 2 {
            return (offset..(offset + size).min(64 * 1024 * 1024))
                .map(|at| (at % 251) as u8)
                .collect();
        }
        if (10..1010).contains(&inode) {
            let data = format!("file-{:04}-bytes!!", inode - 10);
            return data.as_bytes()[offset.min(data.len())..(offset + size).min(data.len())]
                .to_vec();
        }
        let state = self.0.lock().unwrap();
        let data = state.files.get(&inode).map(Vec::as_slice).unwrap_or(&[]);
        data[offset.min(data.len())..(offset + size).min(data.len())].to_vec()
    }
}
impl FileSystem for Tree {
    fn lookup(&self, _: Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        if parent != Inode(1) {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        let name = name.to_bytes();
        let inode = if name == b"large" {
            Some(2)
        } else if name.len() == 5 && name[0] == b'f' {
            std::str::from_utf8(&name[1..])
                .ok()
                .and_then(|n| n.parse::<u64>().ok())
                .filter(|n| *n < 1000)
                .map(|n| n + 10)
        } else {
            self.0.lock().unwrap().names.get(name).copied()
        };
        inode
            .map(|inode| self.entry(inode))
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }
    fn getattr(
        &self,
        _: Context,
        inode: Inode,
        _: Option<Handle>,
    ) -> io::Result<(libc::stat64, Duration)> {
        Ok((self.attr(inode.into()), Duration::ZERO))
    }
    fn read(
        &self,
        _: Context,
        inode: Inode,
        _: Handle,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        _: Option<u64>,
        _: u32,
    ) -> io::Result<usize> {
        w.write(&self.bytes(inode.into(), offset as usize, size as usize))
    }
    fn create(
        &self,
        _: Context,
        parent: Inode,
        name: &CStr,
        _: u32,
        _: bool,
        _: u32,
        _: u32,
        _: Extensions,
    ) -> io::Result<(Entry, Option<Handle>, OpenOptions)> {
        if parent != Inode(1) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let mut state = self.0.lock().unwrap();
        state.next = state.next.max(2000) + 1;
        let inode = state.next;
        state.names.insert(name.to_bytes().to_vec(), inode);
        state.files.insert(inode, Vec::new());
        drop(state);
        Ok((self.entry(inode), None, OpenOptions::empty()))
    }
    fn write(
        &self,
        _: Context,
        inode: Inode,
        _: Handle,
        r: &mut dyn ZeroCopyReader,
        size: u32,
        offset: u64,
        _: Option<u64>,
        _: bool,
        _: bool,
        _: u32,
    ) -> io::Result<usize> {
        let mut bytes = vec![0; size as usize];
        r.read_exact(&mut bytes)?;
        let mut state = self.0.lock().unwrap();
        let data = state
            .files
            .get_mut(&u64::from(inode))
            .ok_or(io::ErrorKind::PermissionDenied)?;
        let end = (offset as usize)
            .checked_add(bytes.len())
            .ok_or(io::ErrorKind::InvalidInput)?;
        if end > 1024 * 1024 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        data.resize(end.max(data.len()), 0);
        data[offset as usize..end].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
    fn rename(
        &self,
        _: Context,
        _: Inode,
        old: &CStr,
        _: Inode,
        new: &CStr,
        _: u32,
    ) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        let inode = state
            .names
            .remove(old.to_bytes())
            .ok_or(io::ErrorKind::NotFound)?;
        state.names.insert(new.to_bytes().to_vec(), inode);
        Ok(())
    }
    fn unlink(&self, _: Context, _: Inode, name: &CStr) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        let inode = state
            .names
            .remove(name.to_bytes())
            .ok_or(io::ErrorKind::NotFound)?;
        state.files.remove(&inode);
        Ok(())
    }
    fn setupmapping(
        &self,
        _: Context,
        inode: Inode,
        _: Handle,
        offset: u64,
        len: u64,
        _: u64,
        mapped: u64,
        mapper: &dyn krun::dax_mapper::DaxMapper,
    ) -> io::Result<()> {
        let mut data = self.bytes(inode.into(), offset as usize, len as usize);
        data.resize(len as usize, 0);
        mapper.map_data(mapped, &data)
    }
    fn removemapping(
        &self,
        _: Context,
        requests: Vec<RemovemappingOne>,
        mapper: &dyn krun::dax_mapper::DaxMapper,
    ) -> io::Result<()> {
        for r in requests {
            mapper.unmap(r.moffset, r.len)?;
        }
        Ok(())
    }
}
fn main() {}
