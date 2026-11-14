//! Persistent RAM extent format for cooperatively frozen guests.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_requires_freeze_and_maps_independent_clones() {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 8192)]).unwrap();
        assert!(Template::capture(&memory, 4096, 1, 1).is_err());
        let mut page = [0u8; 4096];
        for (n, value) in [
            (0, crate::cooperative::FROZEN_MAGIC),
            (2, 0xffff_8880_0010_0000),
            (3, 0x2000),
            (4, 0x20),
            (5, 0xffff_ffff_8100_0000),
            (19, 1),
            (20, 0x8000_0011),
            (21, 0x500),
        ] {
            page[n * 8..n * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        memory.write_slice(&page, GuestAddress(4096)).unwrap();
        assert!(Template::capture_with_topology(&memory, 4096, 1, 1, 123, &[0; 4097]).is_err());
        let topology = b"ordered device topology";
        let template = Template::capture_with_topology(&memory, 4096, 1, 1, 123, topology).unwrap();
        let reopened = Template::open(template.file.try_clone().unwrap()).unwrap();
        assert_eq!(reopened.topology, topology);
        assert_eq!(reopened.clock_ns, 123);
        let a = reopened.map_private().unwrap();
        let b = reopened.map_private().unwrap();
        a.write_slice(&[42], GuestAddress(0)).unwrap();
        let mut value = [0];
        b.read_slice(&mut value, GuestAddress(0)).unwrap();
        assert_eq!(value, [0]);
        assert_eq!(reopened.regions, [(0, 8192, 8192)]);
        assert_eq!(reopened.control, 4096);
    }

    #[test]
    fn rejects_bad_header_and_truncated_file() {
        let file = crate::template_memory::create(4096).unwrap();
        assert!(Template::open(file.try_clone().unwrap()).is_err());
        file.write_all_at(b"KRUNFRZ1", 0).unwrap();
        file.write_all_at(&1u64.to_le_bytes(), 8).unwrap();
        file.write_all_at(&1u64.to_le_bytes(), 24).unwrap();
        file.write_all_at(&1u64.to_le_bytes(), 40).unwrap();
        file.write_all_at(&4096u64.to_le_bytes(), 72).unwrap();
        file.write_all_at(&4096u64.to_le_bytes(), 80).unwrap();
        crate::template_memory::seal(&file).unwrap();
        assert!(Template::open(file).is_err());
    }
}

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion,
    GuestRegionMmap,
};

const MAGIC: &[u8; 8] = b"KRUNFRZ1";
const HEADER: usize = 8192;
const EXTENT_HEADER: usize = 4096;
const MAX_REGIONS: usize = 128;

pub struct Template {
    pub file: File,
    pub control: u64,
    pub vcpus: u8,
    pub ram_mib: u64,
    pub clock_ns: u64,
    pub regions: Vec<(u64, usize, u64)>,
    pub topology: Vec<u8>,
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid cooperative template")
}

impl Template {
    pub fn write_to_path(&self, path: impl AsRef<std::path::Path>) -> io::Result<()> {
        let mut destination = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut offset = 0;
        let mut buffer = vec![0; 1024 * 1024];
        let len = self.file.metadata()?.len();
        while offset < len {
            let count = (len - offset).min(buffer.len() as u64) as usize;
            self.file.read_exact_at(&mut buffer[..count], offset)?;
            std::io::Write::write_all(&mut destination, &buffer[..count])?;
            offset += count as u64;
        }
        destination.sync_all()
    }

    pub fn from_path(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let mut source = File::open(path)?;
        let size = usize::try_from(source.metadata()?.len()).map_err(|_| invalid())?;
        let mut stable = crate::template_memory::create(size)?;
        std::io::copy(&mut source, &mut stable)?;
        crate::template_memory::seal(&stable)?;
        Self::open(stable)
    }

    pub fn open(file: File) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        // SAFETY: fcntl observes seals without taking descriptor ownership.
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK;
        if seals < 0 || seals & required != required {
            return Err(invalid());
        }
        let mut header = [0; HEADER];
        file.read_exact_at(&mut header, 0)?;
        let get = |n: usize| u64::from_le_bytes(header[n * 8..n * 8 + 8].try_into().unwrap());
        if &header[..8] != MAGIC
            || get(1) != 2
            || get(3) == 0
            || get(3) > 255
            || get(5) == 0
            || get(5) > MAX_REGIONS as u64
            || get(7) > EXTENT_HEADER as u64
        {
            return Err(invalid());
        }
        let mut regions = Vec::new();
        let mut last_gpa = 0;
        let mut last_file = HEADER as u64;
        let file_len = file.metadata()?.len();
        for i in 0..get(5) as usize {
            let gpa = get(8 + i * 3);
            let size = get(9 + i * 3);
            let offset = get(10 + i * 3);
            let gpa_end = gpa.checked_add(size).ok_or_else(invalid)?;
            let file_end = offset.checked_add(size).ok_or_else(invalid)?;
            if gpa < last_gpa
                || offset < last_file
                || size == 0
                || (gpa | size | offset) & 4095 != 0
                || file_end > file_len
                || size > usize::MAX as u64
            {
                return Err(invalid());
            }
            regions.push((gpa, size as usize, offset));
            last_gpa = gpa_end;
            last_file = file_end;
        }
        let control = get(2);
        if control & 4095 != 0
            || !regions.iter().any(|(gpa, size, _)| {
                control >= *gpa
                    && control
                        .checked_add(4096)
                        .is_some_and(|end| end <= *gpa + *size as u64)
            })
        {
            return Err(invalid());
        }
        Ok(Self {
            file,
            control,
            vcpus: get(3) as u8,
            ram_mib: get(4),
            clock_ns: get(6),
            regions,
            topology: header[EXTENT_HEADER..EXTENT_HEADER + get(7) as usize].to_vec(),
        })
    }

    pub fn map_private(&self) -> io::Result<GuestMemoryMmap> {
        let regions = self
            .regions
            .iter()
            .map(|(gpa, size, offset)| {
                let mapping = crate::template_memory::map(&self.file, *offset, *size, true)?;
                GuestRegionMmap::new(mapping, GuestAddress(*gpa)).ok_or_else(invalid)
            })
            .collect::<io::Result<Vec<_>>>()?;
        GuestMemoryMmap::from_regions(regions).map_err(io::Error::other)
    }

    /// Copy only after the guest has frozen and all backend workers are stopped.
    /// The destination is sealed before it can be opened as a template.
    pub fn capture(
        memory: &GuestMemoryMmap,
        control: u64,
        vcpus: u8,
        ram_mib: u64,
    ) -> io::Result<Self> {
        Self::capture_with_clock(memory, control, vcpus, ram_mib, 0)
    }

    pub fn capture_with_clock(
        memory: &GuestMemoryMmap,
        control: u64,
        vcpus: u8,
        ram_mib: u64,
        clock_ns: u64,
    ) -> io::Result<Self> {
        Self::capture_with_topology(memory, control, vcpus, ram_mib, clock_ns, &[])
    }

    pub fn capture_with_topology(
        memory: &GuestMemoryMmap,
        control: u64,
        vcpus: u8,
        ram_mib: u64,
        clock_ns: u64,
        topology: &[u8],
    ) -> io::Result<Self> {
        if topology.len() > EXTENT_HEADER {
            return Err(invalid());
        }
        let mut page = [0; 4096];
        memory
            .read_slice(&mut page, GuestAddress(control))
            .map_err(io::Error::other)?;
        crate::cooperative::ResumeEntry::parse(&page)?;
        if memory.num_regions() > MAX_REGIONS || vcpus == 0 {
            return Err(invalid());
        }
        let total = memory.iter().try_fold(HEADER as u64, |sum, r| {
            sum.checked_add(r.len()).ok_or_else(invalid)
        })?;
        let file = crate::template_memory::create(usize::try_from(total).map_err(|_| invalid())?)?;
        let mut header = [0u8; HEADER];
        header[..8].copy_from_slice(MAGIC);
        let mut put =
            |n: usize, value: u64| header[n * 8..n * 8 + 8].copy_from_slice(&value.to_le_bytes());
        put(1, 2);
        put(2, control);
        put(3, vcpus as u64);
        put(4, ram_mib);
        put(5, memory.num_regions() as u64);
        put(6, clock_ns);
        put(7, topology.len() as u64);
        let mut offset = HEADER as u64;
        let mut buffer = vec![0; 1024 * 1024];
        for (i, region) in memory.iter().enumerate() {
            put(8 + i * 3, region.start_addr().raw_value());
            put(9 + i * 3, region.len());
            put(10 + i * 3, offset);
            let mut position = 0;
            while position < region.len() {
                let len = (region.len() - position).min(buffer.len() as u64) as usize;
                region
                    .read_slice(&mut buffer[..len], vm_memory::MemoryRegionAddress(position))
                    .map_err(io::Error::other)?;
                file.write_all_at(&buffer[..len], offset + position)?;
                position += len as u64;
            }
            offset += region.len();
        }
        header[EXTENT_HEADER..EXTENT_HEADER + topology.len()].copy_from_slice(topology);
        file.write_all_at(&header, 0)?;
        crate::template_memory::seal(&file)?;
        Self::open(file)
    }
}
