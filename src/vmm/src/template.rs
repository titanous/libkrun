//! Persistent RAM extent format for cooperatively frozen guests.

#[cfg(all(test, target_arch = "aarch64"))]
#[path = "template_arm64_tests.rs"]
mod arm64_tests;

#[cfg(all(test, target_arch = "x86_64"))]
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
        let template =
            Template::capture_with_manifest(&memory, 4096, 1, 1, 123, topology, b"cpu-xstate-tsc")
                .unwrap();
        use std::os::unix::fs::MetadataExt;
        assert!(
            template.file.metadata().unwrap().blocks() * 512
                < template.file.metadata().unwrap().len(),
            "capture must leave zero guest pages as holes"
        );
        let reopened = Template::open(template.file.try_clone().unwrap()).unwrap();
        assert_eq!(reopened.topology, topology);
        assert_eq!(reopened.compatibility, b"cpu-xstate-tsc");
        assert_eq!(reopened.clock_ns, 123);
        let path = std::env::temp_dir().join(format!("krun-template-path-{}", std::process::id()));
        let staged = reopened.prepare_path(&path).unwrap();
        assert!(!path.exists());
        drop(staged);
        assert!(
            !path.exists(),
            "cancelled capture must not publish a template"
        );
        reopened.write_to_path(&path).unwrap();
        let exported = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let exported_len = exported.metadata().unwrap().len();
        exported
            .set_len(exported_len - EXPORT_COMPLETE.len() as u64)
            .unwrap();
        assert!(
            Template::from_path(&path).is_err(),
            "partial export must be refused"
        );
        exported
            .write_all_at(EXPORT_COMPLETE, exported_len - EXPORT_COMPLETE.len() as u64)
            .unwrap();
        let imported = Template::from_path(&path).unwrap();
        let imported_inode = imported.file.metadata().unwrap().ino();
        let original_inode = std::fs::metadata(&path).unwrap().ino();
        std::fs::remove_file(&path).unwrap();
        assert_ne!(
            imported_inode, original_inode,
            "path import must use independent sealed backing"
        );
        Template::open(imported.file.try_clone().unwrap()).unwrap();
        let a = reopened.map_private().unwrap();
        let b = reopened.map_private().unwrap();
        a.write_slice(&[42], GuestAddress(0)).unwrap();
        let mut value = [0];
        b.read_slice(&mut value, GuestAddress(0)).unwrap();
        assert_eq!(value, [0]);
        assert_eq!(reopened.regions, [(0, 8192, HEADER as u64)]);
        assert_eq!(reopened.control, 4096);
    }

    #[test]
    fn exports_owner_only_template() {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("krun-template-mode-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("guest.ram");
        let template = Template {
            file: crate::template_memory::create(4096).unwrap(),
            control: 0,
            vcpus: 1,
            ram_mib: 1,
            clock_ns: 0,
            regions: Vec::new(),
            topology: Vec::new(),
            compatibility: Vec::new(),
        };
        template.write_to_path(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        use std::os::unix::fs::MetadataExt;
        let sparse = metadata.blocks() * 512 < metadata.len();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
        assert_eq!(mode, 0o600);
        assert!(sparse, "export must preserve zero pages as holes");
    }

    #[test]
    fn failed_export_removes_staged_and_final_paths() {
        let directory =
            std::env::temp_dir().join(format!("krun-export-failure-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("guest.ram");
        let file = crate::template_memory::create(4096).unwrap();
        let template = Template {
            file,
            control: 0,
            vcpus: 1,
            ram_mib: 1,
            clock_ns: 0,
            regions: Vec::new(),
            topology: Vec::new(),
            compatibility: Vec::new(),
        };
        // Inject failure after one chunk, before publication.
        let source = directory.join("source");
        let mut template = template;
        template.file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&source)
            .unwrap();
        template.file.set_len(2 * 1024 * 1024).unwrap();
        FAIL_EXPORT.with(|fail| fail.set(true));
        assert!(template.write_to_path(&path).is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_file(source).unwrap();
        std::fs::remove_dir(directory).unwrap();
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
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion,
    GuestRegionMmap,
};

#[cfg(test)]
thread_local! {
    static FAIL_EXPORT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const MAGIC: &[u8; 8] = b"KRUNFRZ1";
const EXPORT_COMPLETE: &[u8; 16] = b"KRUNEXPORTDONE01";
const HEADER: usize = 24576;
const COMPATIBILITY_START: usize = 8192;
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
    pub compatibility: Vec<u8>,
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid cooperative template")
}

pub struct PreparedTemplatePath {
    temporary: std::path::PathBuf,
    destination: std::path::PathBuf,
}

impl Drop for PreparedTemplatePath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temporary);
    }
}

impl PreparedTemplatePath {
    pub fn publish(self) -> io::Result<()> {
        std::fs::hard_link(&self.temporary, &self.destination)?;
        let result = File::open(
            self.destination
                .parent()
                .unwrap_or(std::path::Path::new(".")),
        )
        .and_then(|directory| directory.sync_all());
        if result.is_err() {
            let _ = std::fs::remove_file(&self.destination);
        }
        result
    }
}

impl Template {
    pub fn write_to_path(&self, path: impl AsRef<std::path::Path>) -> io::Result<()> {
        self.prepare_path(path)?.publish()
    }

    pub fn prepare_path(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> io::Result<PreparedTemplatePath> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = path.as_ref();
        match std::fs::symlink_metadata(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "template destination exists",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let parent = path.parent().unwrap_or(std::path::Path::new("."));
        let temporary = parent.join(format!(
            ".krun-template-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let destination = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        let prepared = PreparedTemplatePath {
            temporary,
            destination: path.to_path_buf(),
        };
        let mut offset = 0;
        let mut buffer = vec![0; 1024 * 1024];
        let len = self.file.metadata()?.len();
        destination.set_len(len)?;
        while offset < len {
            let count = (len - offset).min(buffer.len() as u64) as usize;
            self.file.read_exact_at(&mut buffer[..count], offset)?;
            for (index, page) in buffer[..count].chunks(4096).enumerate() {
                if page.iter().any(|byte| *byte != 0) {
                    destination.write_all_at(page, offset + (index * 4096) as u64)?;
                }
            }
            offset += count as u64;
            #[cfg(test)]
            if FAIL_EXPORT.with(|fail| fail.replace(false)) {
                return Err(io::Error::other("injected mid-export failure"));
            }
        }
        destination.write_all_at(EXPORT_COMPLETE, len)?;
        destination.sync_all()?;
        Ok(prepared)
    }

    pub fn from_path(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let source = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = source.metadata()?;
        // Validate ownership before importing into independent sealed backing.
        // SAFETY: geteuid has no preconditions.
        if !metadata.is_file()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "template path must be an owner-only regular file owned by this user",
            ));
        }
        let data_len = metadata
            .len()
            .checked_sub(EXPORT_COMPLETE.len() as u64)
            .ok_or_else(invalid)?;
        let mut completion = [0; EXPORT_COMPLETE.len()];
        source.read_exact_at(&mut completion, data_len)?;
        if &completion != EXPORT_COMPLETE {
            return Err(invalid());
        }
        let backing =
            crate::template_memory::create(usize::try_from(data_len).map_err(|_| invalid())?)?;
        let mut offset = 0;
        let mut buffer = vec![0; 1024 * 1024];
        while offset < data_len {
            let count = (data_len - offset).min(buffer.len() as u64) as usize;
            source.read_exact_at(&mut buffer[..count], offset)?;
            for (index, page) in buffer[..count].chunks(4096).enumerate() {
                if page.iter().any(|byte| *byte != 0) {
                    backing.write_all_at(page, offset + (index * 4096) as u64)?;
                }
            }
            offset += count as u64;
        }
        crate::template_memory::seal(&backing)?;
        Self::open(backing)
    }

    pub fn open(file: File) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        // SAFETY: fcntl observes seals without taking descriptor ownership.
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK;
        if seals < 0 || seals & required != required {
            return Err(invalid());
        }
        Self::parse_file(file)
    }

    fn parse_file(file: File) -> io::Result<Self> {
        let mut header = [0; HEADER];
        file.read_exact_at(&mut header, 0)?;
        let get = |n: usize| u64::from_le_bytes(header[n * 8..n * 8 + 8].try_into().unwrap());
        if &header[..8] != MAGIC
            || get(1) != 3
            || get(3) == 0
            || get(3) > 255
            || get(5) == 0
            || get(5) > MAX_REGIONS as u64
            || get(7) > EXTENT_HEADER as u64
            || get(392) > (HEADER - COMPATIBILITY_START) as u64
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
            compatibility: header[COMPATIBILITY_START..COMPATIBILITY_START + get(392) as usize]
                .to_vec(),
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
        Self::capture_with_manifest(memory, control, vcpus, ram_mib, clock_ns, topology, &[])
    }

    pub fn capture_with_manifest(
        memory: &GuestMemoryMmap,
        control: u64,
        vcpus: u8,
        ram_mib: u64,
        clock_ns: u64,
        topology: &[u8],
        compatibility: &[u8],
    ) -> io::Result<Self> {
        if compatibility.len() > HEADER - COMPATIBILITY_START {
            return Err(invalid());
        }
        if topology.len() > EXTENT_HEADER {
            return Err(invalid());
        }
        let mut page = [0; 4096];
        memory
            .read_slice(&mut page, GuestAddress(control))
            .map_err(io::Error::other)?;
        #[cfg(target_arch = "x86_64")]
        crate::cooperative::ResumeEntry::parse(&page)?;
        #[cfg(target_arch = "aarch64")]
        nitro_sandbox::cooperative::ResumeEntry::parse(&page, |address, size| {
            memory.iter().any(|region| {
                address >= region.start_addr().raw_value()
                    && address
                        .checked_add(size)
                        .is_some_and(|end| end <= region.start_addr().raw_value() + region.len())
            })
        })?;
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cooperative capture is unsupported on this architecture",
        ));
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
        put(1, 3);
        put(392, compatibility.len() as u64);
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
                // The pre-sized descriptor represents absent pages as zeroes.
                // Do not allocate shmem for untouched or all-zero guest pages.
                for (index, page) in buffer[..len].chunks(4096).enumerate() {
                    if page.iter().any(|byte| *byte != 0) {
                        file.write_all_at(page, offset + position + (index * 4096) as u64)?;
                    }
                }
                position += len as u64;
            }
            offset += region.len();
        }
        header[EXTENT_HEADER..EXTENT_HEADER + topology.len()].copy_from_slice(topology);
        header[COMPATIBILITY_START..COMPATIBILITY_START + compatibility.len()]
            .copy_from_slice(compatibility);
        file.write_all_at(&header, 0)?;
        crate::template_memory::seal(&file)?;
        Self::open(file)
    }
}
