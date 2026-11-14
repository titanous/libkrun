//! Linux resource ownership and direct-mode boot sequence.
use crate::INFO_PAGE_SIZE;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn memfd_create(name: *const u8, flags: u32) -> i32;
    fn eventfd(initial: u32, flags: i32) -> i32;
    fn poll(fds: *mut PollFd, count: usize, timeout: i32) -> i32;
}
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
fn request(direction: u64, nr: u64, size: u64) -> u64 {
    direction << 30 | size << 16 | 0xae00 | nr
}
fn call<T>(fd: &File, direction: u64, nr: u64, arg: &mut T) -> io::Result<i32> {
    let result = unsafe {
        ioctl(
            fd.as_raw_fd(),
            request(direction, nr, std::mem::size_of::<T>() as u64),
            arg as *mut T,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
#[repr(C)]
struct Create {
    flags: u32,
    padding: u32,
    uid: u64,
}
#[repr(C)]
struct Pair {
    first: u64,
    second: u64,
}
#[repr(C)]
struct Region {
    flags: u64,
    size: u64,
    address: u64,
}

#[repr(C)]
struct AddDevice {
    kind: u32,
    queues: u32,
    config_size: u32,
    info_vector: u16,
    reserved: u16,
    features: [u8; 64],
    config: [u8; 256],
    shm_count: u32,
    reserved2: u32,
    shm_pointer: u64,
    uid: u64,
    wiring: [[u16; 4]; 64],
}
#[repr(C)]
struct Vring {
    uid: u64,
    fd: i32,
    value: u16,
    reserved: u16,
}
#[repr(C)]
struct InfoEvent {
    fd: i32,
    vector: u16,
    reserved: u16,
}
#[repr(C)]
struct ShmRegion {
    id: u8,
    reserved: [u8; 7],
    address: u64,
    size: u64,
}
/// A device region owned by the caller, never concurrently shared with guests.
/// Keep it alive until the Sandbox is closed. Registration clears all bytes.
pub struct SharedRegion {
    pub id: u8,
    pub mapping: Mapping,
}
pub struct DeviceEvents {
    pub uid: u64,
    pub kicks: Vec<File>,
    pub calls: Vec<File>,
    pub info: File,
}
fn new_event() -> io::Result<File> {
    let fd = unsafe { eventfd(0, 0x800 | 0x80000) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub struct Mapping {
    address: NonNull<u8>,
    pub len: usize,
}
impl Mapping {
    fn new(fd: &File, len: usize) -> io::Result<Self> {
        let address = unsafe { mmap(std::ptr::null_mut(), len, 3, 1, fd.as_raw_fd(), 0) };
        if address as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            address: NonNull::new(address).ok_or_else(|| io::Error::other("null mmap"))?,
            len,
        })
    }
    pub fn address(&self) -> u64 {
        self.address.as_ptr() as u64
    }
    pub fn zero(&mut self) {
        unsafe { std::ptr::write_bytes(self.address.as_ptr(), 0, self.len) }
    }
    /// Copy hypervisor-written bytes without creating shared Rust references to them.
    pub fn copy(&self) -> Vec<u8> {
        self.copy_range(0, self.len).expect("entire mapping range")
    }
    fn copy_range(&self, offset: usize, length: usize) -> io::Result<Vec<u8>> {
        if offset.checked_add(length).is_none_or(|end| end > self.len) {
            return Err(io::Error::other("mapping range overflow"));
        }
        Ok((offset..offset + length)
            .map(|i| unsafe { self.address.as_ptr().add(i).read_volatile() })
            .collect())
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        if self.len != 0 {
            unsafe {
                munmap(self.address.as_ptr(), self.len);
            }
        }
    }
}

/// Field order is not used for teardown: the VM is explicitly closed under the
/// host-wide lock before any backing mapping can be released.
pub struct Sandbox {
    vm: Option<File>,
    pub ram: Mapping,
    pub info: Mapping,
    _control: File,
    _info_file: File,
    lifecycle_lock: Option<crate::lifecycle::OpenLock>,
    console_at: std::collections::HashMap<usize, u32>,
}
impl Sandbox {
    pub fn new(memory: usize, vcpus: u32) -> io::Result<Self> {
        Self::with_mapping(memory, vcpus, None)
    }
    pub fn with_mapping(memory: usize, vcpus: u32, reused: Option<Mapping>) -> io::Result<Self> {
        Self::with_lifecycle_lock(memory, vcpus, reused, None)
    }
    pub fn with_lifecycle_lock(
        memory: usize,
        vcpus: u32,
        reused: Option<Mapping>,
        lifecycle_lock: Option<crate::lifecycle::LifecycleLock>,
    ) -> io::Result<Self> {
        if memory < 64 * 1024 * 1024 || !memory.is_multiple_of(INFO_PAGE_SIZE) || vcpus == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "memory/vCPU geometry",
            ));
        }
        let lifecycle_lock = lifecycle_lock
            .as_ref()
            .map(|lock| lock.open())
            .transpose()?;
        let mut creation_guard = lifecycle_lock
            .as_ref()
            .map(|lock| lock.acquire())
            .transpose()?;
        if lifecycle_lock
            .as_ref()
            .is_some_and(|lock| lock.serialization == crate::lifecycle::Serialization::Teardown)
        {
            creation_guard.take();
        }
        let control = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/nitro_enclaves")?;
        // Allocate backing before creating a slot: allocation failures cannot
        // drop a live VM fd outside the host teardown lock.
        let ram = match reused {
            Some(mut mapping) if mapping.len == memory => {
                mapping.zero();
                mapping
            }
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "reused mapping size mismatch",
                ))
            }
            None => Mapping::new(&control, memory)?,
        };
        // Fresh pool mmap is cleared by the driver before it is returned;
        // retained mappings are explicitly cleared in the reuse branch above.
        let raw =
            unsafe { memfd_create(c"libkrun-nitro-info".as_ptr().cast(), 1 | 4 | (21 << 26)) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let info_file = unsafe { File::from_raw_fd(raw) };
        info_file.set_len(INFO_PAGE_SIZE as u64)?;
        let mut info = Mapping::new(&info_file, INFO_PAGE_SIZE)?;
        info.zero();
        let mut c = Create {
            flags: 0xa42,
            padding: 0,
            uid: 0,
        };
        let raw = call(&control, 3, 0x30, &mut c)?;
        let vm = unsafe { File::from_raw_fd(raw) };
        drop(creation_guard);
        let sb = Self {
            vm: Some(vm),
            ram,
            info,
            _control: control,
            _info_file: info_file,
            lifecycle_lock,
            console_at: Default::default(),
        };
        for _ in 0..vcpus {
            call(sb.vm.as_ref().unwrap(), 1, 0x2d, &mut -1i32)?;
        }
        call(
            sb.vm.as_ref().unwrap(),
            1,
            0x25,
            &mut Pair {
                first: sb.info.address(),
                second: INFO_PAGE_SIZE as u64,
            },
        )?;
        Ok(sb)
    }
    /// Expose base guest RAM to existing libkrun virtio devices.
    ///
    /// # Safety
    /// All returned memory clones and activated devices must be dropped before
    /// this Sandbox. Nitro places the first GiB at 0x40000000 and the
    /// remaining contiguous host backing at guest address 0x400000000.
    #[cfg(feature = "devices")]
    pub unsafe fn guest_memory(&self) -> io::Result<vm_memory::GuestMemoryMmap> {
        use vm_memory::{GuestAddress, GuestMemoryMmap, GuestRegionMmap, MmapRegion};
        let low = self.ram.len.min(1024 * 1024 * 1024);
        let mut regions = Vec::new();
        for (offset, length, gpa) in [(0, low, 0x40000000), (low, self.ram.len - low, 0x400000000)]
        {
            if length == 0 {
                continue;
            }
            let pointer = unsafe { self.ram.address.as_ptr().add(offset) };
            let region = unsafe { MmapRegion::build_raw(pointer, length, 3, 1) }
                .map_err(|e| io::Error::other(format!("RAM mapping: {e:?}")))?;
            let region = GuestRegionMmap::new(region, GuestAddress(gpa))
                .ok_or_else(|| io::Error::other("RAM region address overflow"))?;
            regions.push(region);
        }
        GuestMemoryMmap::from_regions(regions)
            .map_err(|e| io::Error::other(format!("RAM table: {e:?}")))
    }
    pub fn load(&mut self, path: &str) -> io::Result<()> {
        let vm = self.vm.as_ref().unwrap();
        let mut load = Pair {
            first: 1,
            second: 0,
        };
        call(vm, 3, 0x22, &mut load)?;
        let mut image = File::open(path)?;
        let size = usize::try_from(image.metadata()?.len())
            .map_err(|_| io::Error::other("image size overflow"))?;
        let offset =
            usize::try_from(load.second).map_err(|_| io::Error::other("image offset overflow"))?;
        if offset
            .checked_add(size)
            .is_none_or(|end| end > self.ram.len)
        {
            return Err(io::Error::other("image does not fit"));
        }
        let destination =
            unsafe { std::slice::from_raw_parts_mut(self.ram.address.as_ptr().add(offset), size) };
        image.read_exact(destination)?;
        call(
            vm,
            1,
            0x23,
            &mut Region {
                flags: 0,
                size: self.ram.len as u64,
                address: self.ram.address(),
            },
        )?;
        Ok(())
    }
    pub fn allocate_region(&self, id: u8, size: usize) -> io::Result<SharedRegion> {
        if size == 0 || size > 256 * 1024 * 1024 || !size.is_multiple_of(INFO_PAGE_SIZE) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared region geometry",
            ));
        }
        let mut mapping = Mapping::new(&self._control, size)?;
        mapping.zero();
        Ok(SharedRegion { id, mapping })
    }
    pub fn add_device(
        &self,
        kind: u32,
        queues: usize,
        features: u64,
        config: &[u8],
    ) -> io::Result<DeviceEvents> {
        unsafe { self.add_device_with_regions(kind, queues, features, config, &mut []) }
    }
    /// # Safety
    /// Regions must remain mapped and exclusively assigned to this guest until
    /// the VM fd has closed. Do not call while any previous guest uses them.
    pub unsafe fn add_device_with_regions(
        &self,
        kind: u32,
        queues: usize,
        features: u64,
        config: &[u8],
        regions: &mut [SharedRegion],
    ) -> io::Result<DeviceEvents> {
        if regions.len() > 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many device backing ranges",
            ));
        }
        if queues == 0 || queues > 64 || config.len() > 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "device geometry",
            ));
        }
        let mut req: AddDevice = unsafe { std::mem::zeroed() };
        req.kind = kind;
        req.queues = queues as u32;
        req.config_size = config.len() as u32;
        req.features[..8].copy_from_slice(&features.to_le_bytes());
        req.config[..config.len()].copy_from_slice(config);
        let mut descriptors = Vec::with_capacity(regions.len());
        for region in regions {
            if region.mapping.len == 0
                || region.mapping.len > 256 * 1024 * 1024
                || region.mapping.len % INFO_PAGE_SIZE != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "shared region geometry",
                ));
            }
            region.mapping.zero();
            descriptors.push(ShmRegion {
                id: region.id,
                reserved: [0; 7],
                address: region.mapping.address(),
                size: region.mapping.len as u64,
            });
        }
        req.shm_count = descriptors.len() as u32;
        req.shm_pointer = if descriptors.is_empty() {
            0
        } else {
            descriptors.as_ptr() as u64
        };
        let vm = self.vm.as_ref().unwrap();
        call(vm, 3, 0x26, &mut req)?;
        let mut kicks = Vec::new();
        let mut calls = Vec::new();
        for q in &req.wiring[..queues] {
            let kick = new_event()?;
            let notify = new_event()?;
            call(
                vm,
                1,
                0x28,
                &mut Vring {
                    uid: req.uid,
                    fd: kick.as_raw_fd(),
                    value: q[2],
                    reserved: 0,
                },
            )?;
            call(
                vm,
                1,
                0x27,
                &mut Vring {
                    uid: req.uid,
                    fd: notify.as_raw_fd(),
                    value: q[3],
                    reserved: 0,
                },
            )?;
            kicks.push(kick);
            calls.push(notify);
        }
        let info = new_event()?;
        call(
            vm,
            1,
            0x2b,
            &mut InfoEvent {
                fd: info.as_raw_fd(),
                vector: req.info_vector,
                reserved: 0,
            },
        )?;
        Ok(DeviceEvents {
            uid: req.uid,
            kicks,
            calls,
            info,
        })
    }
    pub fn snapshot(&self, uid: u64) -> io::Result<Option<crate::DeviceSnapshot>> {
        let header = self.info.copy_range(0, 16)?;
        if crate::u32_at(&header, 0).ok() != Some(0x5343494e) {
            return Ok(None);
        }
        let count = crate::u32_at(&header, 8).unwrap() as usize;
        if count > (INFO_PAGE_SIZE - 16) / 48 {
            return Err(io::Error::other("invalid TOC count"));
        }
        let toc = self.info.copy_range(16, count * 48)?;
        for record in toc.chunks_exact(48) {
            if crate::u16_at(record, 0).unwrap() != 2 || crate::u64_at(record, 16).unwrap() != uid {
                continue;
            }
            let offset = crate::u32_at(record, 4).unwrap() as usize;
            let size = crate::u64_at(record, 8).unwrap();
            if size < 2344
                || offset
                    .checked_add(2344)
                    .is_none_or(|end| end > INFO_PAGE_SIZE)
            {
                return Err(io::Error::other("invalid device entry range"));
            }
            for _ in 0..16 {
                let before = self.info.copy_range(offset, 12)?;
                std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                let entry = self.info.copy_range(offset, 2344)?;
                std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                let after = self.info.copy_range(offset, 12)?;
                if before != after {
                    continue;
                }
                // Reuse the checked wire parser with a compact synthetic TOC.
                let mut copy = vec![0u8; 64 + 2344];
                copy[..16].copy_from_slice(&header);
                copy[8..12].copy_from_slice(&1u32.to_le_bytes());
                copy[16..64].copy_from_slice(record);
                copy[20..24].copy_from_slice(&64u32.to_le_bytes());
                copy[24..32].copy_from_slice(&2344u64.to_le_bytes());
                copy[64..].copy_from_slice(&entry);
                if entry[128 + 20] & 15 == 15 {
                    if let Ok(directory) = std::env::var("NITRO_INFO_FIXTURES") {
                        std::fs::create_dir_all(&directory)?;
                        let path =
                            std::path::Path::new(&directory).join(format!("device-{uid}.bin"));
                        if !path.exists() {
                            std::fs::write(path, &copy)?;
                        }
                    }
                }
                return crate::device_snapshot(&copy, uid)
                    .map_err(|e| io::Error::other(format!("info page: {e:?}")));
            }
            return Ok(None);
        }
        Ok(None)
    }
    pub fn start(&self, cid: u64) -> io::Result<()> {
        let _guard = self
            .lifecycle_lock
            .as_ref()
            .filter(|lock| lock.serialization == crate::lifecycle::Serialization::StartAndTeardown)
            .map(|lock| lock.acquire())
            .transpose()?;
        call(
            self.vm.as_ref().unwrap(),
            3,
            0x24,
            &mut Pair {
                first: 1,
                second: cid,
            },
        )?;
        Ok(())
    }
    pub fn drain_console(&mut self, output: &mut impl Write) -> io::Result<()> {
        let header = self.info.copy_range(0, 16)?;
        if crate::u32_at(&header, 0).ok() != Some(0x5343494e) {
            return Ok(());
        }
        let count = crate::u32_at(&header, 8).unwrap_or(0) as usize;
        if count > (INFO_PAGE_SIZE - 16) / 48 {
            return Err(io::Error::other("bad console TOC"));
        }
        let toc = self.info.copy_range(16, count * 48)?;
        for record in toc.chunks_exact(48) {
            if crate::u16_at(record, 0).unwrap_or(0) != 1 {
                continue;
            }
            let off = crate::u32_at(record, 4).unwrap_or(u32::MAX) as usize;
            let size = crate::u64_at(record, 8).unwrap();
            if size < 8 || off > INFO_PAGE_SIZE - 8 {
                return Err(io::Error::other("bad console entry"));
            }
            let ring = self.info.copy_range(off, 8)?;
            let end = crate::u32_at(&ring, 0).unwrap();
            let cap = crate::u32_at(&ring, 4).unwrap();
            if cap == 0 || cap as usize > INFO_PAGE_SIZE - off - 8 || u64::from(cap) > size - 8 {
                return Err(io::Error::other("bad console capacity"));
            }
            std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
            let cursor = self.console_at.entry(off).or_insert(0);
            if end.wrapping_sub(*cursor) > cap {
                *cursor = end.wrapping_sub(cap);
            }
            while *cursor != end {
                let index = *cursor % cap;
                let length = end.wrapping_sub(*cursor).min(cap - index);
                let bytes = self
                    .info
                    .copy_range(off + 8 + index as usize, length as usize)?;
                output.write_all(&bytes)?;
                *cursor = cursor.wrapping_add(length);
            }
        }
        output.flush()
    }
    pub fn powered_off(&self, milliseconds: i32) -> io::Result<bool> {
        let mut p = PollFd {
            fd: self.vm.as_ref().unwrap().as_raw_fd(),
            events: 0x10,
            revents: 0,
        };
        if unsafe { poll(&mut p, 1, milliseconds) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(p.revents & 0x10 != 0)
    }
    /// Poll the VM fd with a deadline. Console text is diagnostic, never the
    /// power-off signal. A reboot/no-start is reclaimed on deadline expiry.
    pub fn wait(&mut self, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain_console(&mut io::stdout())?;
            let mut p = PollFd {
                fd: self.vm.as_ref().unwrap().as_raw_fd(),
                events: 0x10,
                revents: 0,
            };
            let rc = unsafe { poll(&mut p, 1, 10) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            if p.revents & 0x10 != 0 {
                self.drain_console(&mut io::stdout())?;
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }
}
impl Sandbox {
    /// Return RAM for a later guest after the VM has been synchronously closed.
    /// All device transports and guest-memory views must already be dropped.
    pub fn into_mapping(mut self) -> Mapping {
        self.close_vm();
        let empty = Mapping {
            address: NonNull::dangling(),
            len: 0,
        };
        std::mem::replace(&mut self.ram, empty)
    }
    fn close_vm(&mut self) {
        if self.vm.is_none() {
            return;
        }
        let _guard = self.lifecycle_lock.as_ref().map(|lock| {
            loop {
                match lock.acquire() {
                    Ok(guard) => break guard,
                    Err(_) => {
                        // Keep VM and backing memory alive. The daemon must
                        // repair its lock path or reap us under the same lock.
                        std::thread::park_timeout(Duration::from_secs(1));
                    }
                }
            }
        });
        drop(self.vm.take());
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        self.close_vm();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uapi_structure_sizes() {
        assert_eq!(std::mem::size_of::<Create>(), 16);
        assert_eq!(std::mem::size_of::<Region>(), 24);
        assert_eq!(std::mem::size_of::<AddDevice>(), 872);
        assert_eq!(std::mem::size_of::<Vring>(), 16);
        assert_eq!(std::mem::size_of::<InfoEvent>(), 8);
        assert_eq!(std::mem::size_of::<ShmRegion>(), 24);
    }
    #[test]
    fn mapping_zero_erases_previous_bytes() {
        let fd = unsafe { memfd_create(c"zero-test".as_ptr().cast(), 1) };
        assert!(fd >= 0);
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(4096).unwrap();
        let mut mapping = Mapping::new(&file, 4096).unwrap();
        unsafe {
            std::ptr::write_bytes(mapping.address.as_ptr(), 0xa5, mapping.len);
        }
        assert!(mapping.copy().iter().all(|b| *b == 0xa5));
        mapping.zero();
        assert!(mapping.copy().iter().all(|b| *b == 0));
    }
}
