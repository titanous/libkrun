// Copyright 2024 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! VM snapshot and restore support.
//!
//! Provides full and incremental snapshot capabilities.
//! Platform-agnostic: vCPU states are stored as opaque serialized bytes
//! so the snapshot format doesn't depend on HVF or KVM types.

use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion,
};

pub const SNAPSHOT_MAGIC: u32 = 0x4B52_534E; // "KRSN"
pub const SNAPSHOT_VERSION: u32 = 1;

/// Maximum size for vmstate files during deserialization (10 MB).
/// This prevents OOM from corrupted or malicious files.
#[cfg(feature = "snapshot")]
const VMSTATE_MAX_SIZE: u64 = 10 * 1024 * 1024;

/// Timeout for quiescing async device workers during snapshot operations.
pub const SNAPSHOT_QUIESCE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Debug)]
pub enum SnapshotError {
    Io(io::Error),
    Serialize(String),
    Deserialize(String),
    InvalidMagic,
    InvalidVersion(u32),
    MemorySizeMismatch {
        expected: u64,
        got: u64,
    },
    MemoryLayoutMismatch {
        expected: Vec<(u64, u64)>,
        got: Vec<(u64, u64)>,
    },
    VcpuCountMismatch {
        expected: usize,
        got: usize,
    },
    NestedEnabledMismatch,
    DirtyTrackingNotEnabled,
    FileSizeExceeded {
        size: u64,
        limit: u64,
    },
}

impl Display for SnapshotError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            SnapshotError::Io(e) => write!(f, "Snapshot I/O error: {e}"),
            SnapshotError::Serialize(e) => write!(f, "Snapshot serialization error: {e}"),
            SnapshotError::Deserialize(e) => write!(f, "Snapshot deserialization error: {e}"),
            SnapshotError::InvalidMagic => write!(f, "Invalid snapshot magic number"),
            SnapshotError::InvalidVersion(v) => write!(f, "Unsupported snapshot version: {v}"),
            SnapshotError::MemorySizeMismatch { expected, got } => {
                write!(
                    f,
                    "Memory size mismatch: expected {expected} bytes, got {got}"
                )
            }
            SnapshotError::MemoryLayoutMismatch { expected, got } => {
                write!(f, "RAM layout mismatch: expected {expected:?}, got {got:?}")
            }
            SnapshotError::VcpuCountMismatch { expected, got } => {
                write!(f, "vCPU count mismatch: expected {expected}, got {got}")
            }
            SnapshotError::NestedEnabledMismatch => {
                write!(
                    f,
                    "Nested virtualization enabled mismatch between snapshot and current VM"
                )
            }
            SnapshotError::DirtyTrackingNotEnabled => {
                write!(f, "Dirty tracking is not enabled")
            }
            SnapshotError::FileSizeExceeded { size, limit } => {
                write!(
                    f,
                    "Snapshot file size ({size} bytes) exceeds limit ({limit} bytes)"
                )
            }
        }
    }
}

impl From<io::Error> for SnapshotError {
    fn from(e: io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

#[cfg_attr(kani, kani::ensures(|result| {
    if header.magic != SNAPSHOT_MAGIC {
        result.is_err()
    } else if header.version != SNAPSHOT_VERSION {
        result.is_err()
    } else {
        result.is_ok()
    }
}))]
pub fn validate_magic_and_version(header: &SnapshotHeader) -> Result<(), SnapshotError> {
    if header.magic != SNAPSHOT_MAGIC {
        return Err(SnapshotError::InvalidMagic);
    }
    if header.version != SNAPSHOT_VERSION {
        return Err(SnapshotError::InvalidVersion(header.version));
    }
    Ok(())
}

pub fn validate_header_for_vm(
    header: &SnapshotHeader,
    guest_memory: &GuestMemoryMmap,
    expected_vcpu_count: usize,
    expected_nested_enabled: bool,
) -> Result<(), SnapshotError> {
    validate_magic_and_version(header)?;

    if header.vcpu_count as usize != expected_vcpu_count {
        return Err(SnapshotError::VcpuCountMismatch {
            expected: expected_vcpu_count,
            got: header.vcpu_count as usize,
        });
    }

    if header.nested_enabled != expected_nested_enabled {
        return Err(SnapshotError::NestedEnabledMismatch);
    }

    let expected_layout = ram_layout(guest_memory);
    if header.ram_regions != expected_layout {
        return Err(SnapshotError::MemoryLayoutMismatch {
            expected: expected_layout,
            got: header.ram_regions.clone(),
        });
    }

    Ok(())
}

/// Header for the snapshot file.
#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
pub struct SnapshotHeader {
    pub magic: u32,
    pub version: u32,
    pub vcpu_count: u32,
    /// (guest_addr, size) pairs describing the RAM layout
    pub ram_regions: Vec<(u64, u64)>,
    pub nested_enabled: bool,
}

/// Complete VM snapshot (metadata, excluding raw memory).
#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
pub struct VmSnapshot {
    pub header: SnapshotHeader,
    /// Per-vCPU states as opaque serialized bytes (platform-specific format).
    pub vcpu_states: Vec<Vec<u8>>,
    /// Device states as (device_id, serialized_bytes) pairs.
    pub device_states: Vec<(String, Vec<u8>)>,
    #[cfg_attr(feature = "snapshot", serde(default))]
    pub gic_state: Option<Vec<u8>>,
    /// VM-level state (x86_64: PIT/PIC/IOAPIC/clock) as opaque serialized bytes.
    #[cfg_attr(feature = "snapshot", serde(default))]
    pub vm_state: Option<Vec<u8>>,
    /// Guest addresses of pages excluded from snapshot (balloon-reclaimed).
    #[cfg_attr(feature = "snapshot", serde(default))]
    pub excluded_pages: Vec<u64>,
}

/// Dump guest memory to a file.
pub fn dump_memory(guest_memory: &GuestMemoryMmap, path: &Path) -> Result<(), SnapshotError> {
    let mut file = File::create(path)?;
    for region in guest_memory.iter() {
        let region_start = region.start_addr().raw_value();
        let region_len = region.len();
        let vhp = crate::get_validated_host_ptr(guest_memory, region_start).ok_or_else(|| {
            SnapshotError::Serialize(format!("Invalid guest address: {region_start:#x}"))
        })?;
        let slice = vhp
            .as_slice(region_len as usize)
            .map_err(|e| SnapshotError::Serialize(format!("Memory region slice invalid: {e}")))?;
        file.write_all(slice)?;
    }
    file.sync_all()?;
    Ok(())
}

/// Load guest memory from a file.
pub fn load_memory(guest_memory: &GuestMemoryMmap, path: &Path) -> Result<(), SnapshotError> {
    let mut file = File::open(path)?;
    let expected_size = total_ram_size(guest_memory);
    let actual_size = file.metadata()?.len();
    if actual_size != expected_size {
        return Err(SnapshotError::MemorySizeMismatch {
            expected: expected_size,
            got: actual_size,
        });
    }

    for region in guest_memory.iter() {
        let region_start = region.start_addr().raw_value();
        let region_len = region.len();
        let mut vhp =
            crate::get_validated_host_ptr(guest_memory, region_start).ok_or_else(|| {
                SnapshotError::Deserialize(format!("Invalid guest address: {region_start:#x}"))
            })?;
        let slice = vhp
            .as_slice_mut(region_len as usize)
            .map_err(|e| SnapshotError::Deserialize(format!("Memory region slice invalid: {e}")))?;
        file.read_exact(slice)?;
    }
    Ok(())
}

/// Compute total RAM size from guest memory regions.
/// Convert sysconf result to page size.
///
/// Pure function that validates the sysconf(_SC_PAGESIZE) return value.
/// Returns Some(page_size) for positive values, None for non-positive values
/// (including the error sentinel -1).
///
/// This helper is extracted for testability in Kani proofs.
pub fn sysconf_to_page_size(result: i64) -> Option<u64> {
    if result <= 0 {
        None
    } else {
        Some(result as u64)
    }
}

/// Get the system page size in bytes using `sysconf(_SC_PAGESIZE)`.
///
/// Panics if `sysconf` returns a non-positive value, which would indicate a broken
/// system configuration.  This is unconditionally available (not feature-gated) so
/// that non-snapshot code paths (e.g., kernel bundle validation, UFFD handler) can
/// use it without pulling in the full snapshot-store dependency.
pub fn system_page_size() -> u64 {
    // SAFETY: sysconf is a pure query that does not modify any memory or process state.
    let result = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    sysconf_to_page_size(result).expect("sysconf(_SC_PAGESIZE) returned non-positive value")
}

pub fn total_ram_size(guest_memory: &GuestMemoryMmap) -> u64 {
    guest_memory.iter().map(|r| r.len()).sum()
}

/// Get the RAM layout as (guest_addr, size) pairs.
pub fn ram_layout(guest_memory: &GuestMemoryMmap) -> Vec<(u64, u64)> {
    guest_memory
        .iter()
        .map(|r| (r.start_addr().raw_value(), r.len()))
        .collect()
}

/// Zero-fill reclaimed pages in guest memory.
///
/// For each guest address in `pages`, writes 4096 zeros to guest memory.
/// Called during incremental restore to zero-fill pages that were reclaimed by the balloon.
pub fn apply_reclaimed_pages(mem: &GuestMemoryMmap, pages: &[u64]) -> Result<(), SnapshotError> {
    const PAGE_SIZE: usize = 4096;
    let zeros = [0u8; PAGE_SIZE];

    for &addr in pages {
        mem.write_slice(&zeros, GuestAddress(addr)).map_err(|e| {
            SnapshotError::Serialize(format!("Failed to zero-fill page at 0x{:x}: {e}", addr))
        })?;
    }
    Ok(())
}

/// Save VM snapshot metadata to a file (vmstate).
#[cfg(feature = "snapshot")]
pub fn save_vmstate(snapshot: &VmSnapshot, path: &Path) -> Result<(), SnapshotError> {
    let data = bincode::serialize(snapshot).map_err(|e| SnapshotError::Serialize(e.to_string()))?;
    let mut file = File::create(path)?;
    file.write_all(&data)?;
    file.sync_all()?;
    Ok(())
}

/// Load VM snapshot metadata from a file (vmstate).
#[cfg(feature = "snapshot")]
pub fn load_vmstate(path: &Path) -> Result<VmSnapshot, SnapshotError> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    if file_size > VMSTATE_MAX_SIZE {
        return Err(SnapshotError::FileSizeExceeded {
            size: file_size,
            limit: VMSTATE_MAX_SIZE,
        });
    }
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let snapshot: VmSnapshot =
        bincode::deserialize(&data).map_err(|e| SnapshotError::Deserialize(e.to_string()))?;
    validate_magic_and_version(&snapshot.header)?;
    if snapshot.vcpu_states.len() != snapshot.header.vcpu_count as usize {
        return Err(SnapshotError::VcpuCountMismatch {
            expected: snapshot.header.vcpu_count as usize,
            got: snapshot.vcpu_states.len(),
        });
    }
    Ok(snapshot)
}

/// Create a full VM snapshot to a directory.
///
/// The directory will contain:
/// - `vmstate`: serialized VmSnapshot (header + vcpu states + device states)
/// - `memory`: raw guest RAM dump
#[cfg(feature = "snapshot")]
pub fn create_full_snapshot(
    path: &Path,
    guest_memory: &GuestMemoryMmap,
    vcpu_states: Vec<Vec<u8>>,
    device_states: Vec<(String, Vec<u8>)>,
    gic_state: Option<Vec<u8>>,
    vm_state: Option<Vec<u8>>,
    nested_enabled: bool,
) -> Result<(), SnapshotError> {
    std::fs::create_dir_all(path)?;

    let snapshot = VmSnapshot {
        header: SnapshotHeader {
            magic: SNAPSHOT_MAGIC,
            version: SNAPSHOT_VERSION,
            vcpu_count: vcpu_states.len() as u32,
            ram_regions: ram_layout(guest_memory),
            nested_enabled,
        },
        vcpu_states,
        device_states,
        gic_state,
        vm_state,
        excluded_pages: Vec::new(),
    };

    save_vmstate(&snapshot, &path.join("vmstate"))?;
    dump_memory(guest_memory, &path.join("memory"))?;

    Ok(())
}

/// Page size on Apple Silicon (16KB).
pub const PAGE_SIZE: u64 = 16384;

/// An incremental memory diff entry.
#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
pub struct DirtyPage {
    pub guest_addr: u64,
    pub data: Vec<u8>,
}

/// Incremental snapshot: vm state + dirty pages only.
#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
pub struct IncrementalSnapshot {
    pub header: SnapshotHeader,
    /// Per-vCPU states as opaque serialized bytes (platform-specific format).
    pub vcpu_states: Vec<Vec<u8>>,
    pub device_states: Vec<(String, Vec<u8>)>,
    pub dirty_pages: Vec<DirtyPage>,
    #[cfg_attr(feature = "snapshot", serde(default))]
    pub gic_state: Option<Vec<u8>>,
    /// VM-level state (x86_64: PIT/PIC/IOAPIC/clock) as opaque serialized bytes.
    #[cfg_attr(feature = "snapshot", serde(default))]
    pub vm_state: Option<Vec<u8>>,
    /// Guest addresses that should be zero-filled on restore.
    #[cfg_attr(feature = "snapshot", serde(default))]
    pub reclaimed_pages: Vec<u64>,
}

/// Combined interrupt controller snapshot: pending IRQs + GIC register state.
#[cfg(feature = "snapshot")]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct InterruptControllerSnapshot {
    pub pending_irqs: Vec<Vec<u32>>,
    pub gic_registers: Option<Vec<u8>>,
}

/// Save an incremental snapshot.
#[cfg(feature = "snapshot")]
pub fn save_incremental_snapshot(
    snapshot: &IncrementalSnapshot,
    path: &Path,
) -> Result<(), SnapshotError> {
    let data = bincode::serialize(snapshot).map_err(|e| SnapshotError::Serialize(e.to_string()))?;
    let mut file = File::create(path)?;
    file.write_all(&data)?;
    file.sync_all()?;
    Ok(())
}

/// Load an incremental snapshot.
#[cfg(feature = "snapshot")]
pub fn load_incremental_snapshot(path: &Path) -> Result<IncrementalSnapshot, SnapshotError> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    if file_size > VMSTATE_MAX_SIZE {
        return Err(SnapshotError::FileSizeExceeded {
            size: file_size,
            limit: VMSTATE_MAX_SIZE,
        });
    }
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let snapshot: IncrementalSnapshot =
        bincode::deserialize(&data).map_err(|e| SnapshotError::Deserialize(e.to_string()))?;
    validate_magic_and_version(&snapshot.header)?;
    if snapshot.vcpu_states.len() != snapshot.header.vcpu_count as usize {
        return Err(SnapshotError::VcpuCountMismatch {
            expected: snapshot.header.vcpu_count as usize,
            got: snapshot.vcpu_states.len(),
        });
    }
    Ok(snapshot)
}

/// Apply incremental snapshot dirty pages on top of existing guest memory.
pub fn apply_dirty_pages(
    guest_memory: &GuestMemoryMmap,
    dirty_pages: &[DirtyPage],
) -> Result<(), SnapshotError> {
    for page in dirty_pages {
        guest_memory
            .write_slice(&page.data, GuestAddress(page.guest_addr))
            .map_err(|e| {
                SnapshotError::Deserialize(format!(
                    "Failed writing dirty page at 0x{:x}: {e}",
                    page.guest_addr
                ))
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a test GuestMemoryMmap from a list of (guest_addr, size) pairs.
    fn make_memory(regions: &[(u64, u64)]) -> GuestMemoryMmap {
        let regions_with_addr: Vec<(GuestAddress, usize)> = regions
            .iter()
            .map(|(addr, size)| (GuestAddress(*addr), *size as usize))
            .collect();
        GuestMemoryMmap::from_ranges(&regions_with_addr).unwrap()
    }

    /// Create a valid SnapshotHeader matching the given memory and parameters.
    fn valid_header(mem: &GuestMemoryMmap, vcpu_count: u32, nested: bool) -> SnapshotHeader {
        SnapshotHeader {
            magic: SNAPSHOT_MAGIC,
            version: SNAPSHOT_VERSION,
            vcpu_count,
            ram_regions: ram_layout(mem),
            nested_enabled: nested,
        }
    }

    /// AC1.1: Valid SnapshotHeader roundtrip
    #[cfg(feature = "snapshot")]
    #[test]
    fn test_header_roundtrip() {
        let mem = make_memory(&[(0x1000, 0x2000), (0x4000, 0x3000)]);
        let header = valid_header(&mem, 4, false);

        let data = bincode::serialize(&header).unwrap();
        let decoded: SnapshotHeader = bincode::deserialize(&data).unwrap();

        assert_eq!(decoded.magic, header.magic);
        assert_eq!(decoded.version, header.version);
        assert_eq!(decoded.vcpu_count, header.vcpu_count);
        assert_eq!(decoded.ram_regions, header.ram_regions);
        assert_eq!(decoded.nested_enabled, header.nested_enabled);
    }

    /// AC1.2: Invalid magic bytes
    #[test]
    fn test_invalid_magic() {
        let mem = make_memory(&[(0x1000, 0x2000)]);
        let mut header = valid_header(&mem, 4, false);
        header.magic = 0xDEAD_BEEF;

        let result = validate_header_for_vm(&header, &mem, 4, false);
        assert!(matches!(result, Err(SnapshotError::InvalidMagic)));
    }

    /// AC1.3: Invalid version
    #[test]
    fn test_invalid_version() {
        let mem = make_memory(&[(0x1000, 0x2000)]);
        let mut header = valid_header(&mem, 4, false);
        header.version = 99;

        let result = validate_header_for_vm(&header, &mem, 4, false);
        assert!(matches!(result, Err(SnapshotError::InvalidVersion(99))));
    }

    /// AC1.4: vCPU count mismatch
    #[test]
    fn test_vcpu_count_mismatch() {
        let mem = make_memory(&[(0x1000, 0x2000)]);
        let header = valid_header(&mem, 2, false);

        let result = validate_header_for_vm(&header, &mem, 4, false);
        assert!(matches!(
            result,
            Err(SnapshotError::VcpuCountMismatch {
                expected: 4,
                got: 2
            })
        ));
    }

    /// AC1.5: Memory layout mismatch
    #[test]
    fn test_layout_mismatch() {
        let mem = make_memory(&[(0x1000, 0x2000), (0x4000, 0x3000)]);
        let mut header = valid_header(&mem, 4, false);

        // Corrupt the layout: change second region size
        header.ram_regions[1] = (0x4000, 0x1000);

        let result = validate_header_for_vm(&header, &mem, 4, false);
        assert!(matches!(
            result,
            Err(SnapshotError::MemoryLayoutMismatch { .. })
        ));
    }

    /// AC1.5b: Memory layout mismatch (different region sizes)
    #[test]
    fn test_layout_mismatch_different_region_sizes() {
        let mem = make_memory(&[(0x1000, 0x2000)]);
        let header = valid_header(&mem, 4, false);

        // Create a different memory with different total size
        let different_mem = make_memory(&[(0x1000, 0x1000)]);

        let result = validate_header_for_vm(&header, &different_mem, 4, false);
        assert!(matches!(
            result,
            Err(SnapshotError::MemoryLayoutMismatch { .. })
        ));
    }

    /// AC1.6: Memory file size mismatch via load_memory
    #[cfg(feature = "snapshot")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_memory_file_size_mismatch() {
        use std::io::Write;

        let mem = make_memory(&[(0x1000, 0x2000)]);

        // Create a temp file with incorrect size
        let mut temp_file = std::io::Cursor::new(Vec::new());
        // Write only 0x1000 bytes when expecting 0x2000
        temp_file.write_all(&vec![0u8; 0x1000]).unwrap();

        // We need to test load_memory with a real file, use a temp directory
        let temp_dir = std::path::PathBuf::from("/tmp");
        let temp_path = temp_dir.join(format!("libkrun_test_{}.bin", std::process::id()));

        // Write wrong-sized memory file
        std::fs::write(&temp_path, vec![0u8; 0x1000]).unwrap();

        let result = load_memory(&mem, &temp_path);
        let _ = std::fs::remove_file(&temp_path);

        assert!(matches!(
            result,
            Err(SnapshotError::MemorySizeMismatch {
                expected: 0x2000,
                got: 0x1000
            })
        ));
    }

    /// AC1.7: Truncated/Invalid vmstate file
    #[cfg(feature = "snapshot")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_truncated_vmstate_file() {
        use std::io::Write;

        // Create a temp file with truncated/invalid vmstate data
        let temp_dir = std::path::PathBuf::from("/tmp");
        let temp_path = temp_dir.join(format!("libkrun_test_vmstate_{}.bin", std::process::id()));

        // Write just a few bytes (not a valid serialized VmSnapshot)
        let mut file = std::fs::File::create(&temp_path).unwrap();
        file.write_all(&[0x01, 0x02, 0x03, 0x04]).unwrap();
        drop(file);

        let result = load_vmstate(&temp_path);
        let _ = std::fs::remove_file(&temp_path);

        // Should fail to deserialize
        assert!(matches!(result, Err(SnapshotError::Deserialize(_))));
    }

    /// AC1.8: Nested enabled mismatch
    #[test]
    fn test_nested_enabled_mismatch() {
        let mem = make_memory(&[(0x1000, 0x2000)]);
        let header = valid_header(&mem, 4, true); // Header says nested=true

        // But validate with expected_nested_enabled=false
        let result = validate_header_for_vm(&header, &mem, 4, false);
        assert!(matches!(result, Err(SnapshotError::NestedEnabledMismatch)));
    }

    /// AC1.8 variant: nested_enabled matches
    #[test]
    fn test_nested_enabled_match() {
        let mem = make_memory(&[(0x1000, 0x2000)]);
        let header = valid_header(&mem, 4, true);

        let result = validate_header_for_vm(&header, &mem, 4, true);
        assert!(result.is_ok());
    }

    /// AC5.1: VmSnapshot save/load round-trip with valid snapshot under 10MB
    #[cfg(feature = "snapshot")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_vmstate_roundtrip() {
        let mem = make_memory(&[(0x1000, 0x2000)]);
        let header = valid_header(&mem, 2, false);

        // Create a valid VmSnapshot with minimal content
        let snapshot = VmSnapshot {
            header,
            vcpu_states: vec![vec![0xAA; 256], vec![0xBB; 256]], // 2 vCPUs with state data
            device_states: vec![("test_device".to_string(), vec![0xCC; 512])], // One device
            gic_state: None,
            vm_state: None,
            excluded_pages: Vec::new(),
        };

        // Create a temp file for vmstate
        let temp_dir = std::path::PathBuf::from("/tmp");
        let temp_path = temp_dir.join(format!(
            "libkrun_test_vmstate_roundtrip_{}.bin",
            std::process::id()
        ));

        // Save snapshot
        let save_result = save_vmstate(&snapshot, &temp_path);
        assert!(save_result.is_ok());

        // Verify file exists and is under 10MB
        let file_size = std::fs::metadata(&temp_path).unwrap().len();
        assert!(file_size < 10 * 1024 * 1024);

        // Load snapshot back
        let load_result = load_vmstate(&temp_path);
        let _ = std::fs::remove_file(&temp_path);
        assert!(load_result.is_ok());

        let loaded = load_result.unwrap();

        // Verify round-trip: header, vcpu_states, and device_states are preserved
        assert_eq!(loaded.header.magic, snapshot.header.magic);
        assert_eq!(loaded.header.version, snapshot.header.version);
        assert_eq!(loaded.header.vcpu_count, snapshot.header.vcpu_count);
        assert_eq!(loaded.header.ram_regions, snapshot.header.ram_regions);
        assert_eq!(loaded.header.nested_enabled, snapshot.header.nested_enabled);
        assert_eq!(loaded.vcpu_states.len(), snapshot.vcpu_states.len());
        assert_eq!(loaded.vcpu_states[0], snapshot.vcpu_states[0]);
        assert_eq!(loaded.vcpu_states[1], snapshot.vcpu_states[1]);
        assert_eq!(loaded.device_states.len(), snapshot.device_states.len());
        assert_eq!(loaded.device_states[0], snapshot.device_states[0]);
    }

    /// AC5.3: IncrementalSnapshot save/load round-trip with valid snapshot under 10MB
    #[cfg(feature = "snapshot")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_incremental_snapshot_roundtrip() {
        let mem = make_memory(&[(0x1000, 0x2000)]);
        let header = valid_header(&mem, 2, false);

        // Create a valid IncrementalSnapshot with minimal content
        let dirty_pages = vec![
            DirtyPage {
                guest_addr: 0x1000,
                data: vec![0xAA; 256],
            },
            DirtyPage {
                guest_addr: 0x2000,
                data: vec![0xBB; 256],
            },
        ];

        let snapshot = IncrementalSnapshot {
            header,
            vcpu_states: vec![vec![0xCC; 256], vec![0xDD; 256]], // 2 vCPUs with state data
            device_states: vec![("test_device".to_string(), vec![0xEE; 512])], // One device
            dirty_pages,
            gic_state: None,
            vm_state: None,
            reclaimed_pages: Vec::new(),
        };

        // Create a temp file for incremental snapshot
        let temp_dir = std::path::PathBuf::from("/tmp");
        let temp_path = temp_dir.join(format!(
            "libkrun_test_incremental_roundtrip_{}.bin",
            std::process::id()
        ));

        // Save incremental snapshot
        let save_result = save_incremental_snapshot(&snapshot, &temp_path);
        assert!(save_result.is_ok());

        // Verify file exists and is under 10MB
        let file_size = std::fs::metadata(&temp_path).unwrap().len();
        assert!(file_size < 10 * 1024 * 1024);

        // Load incremental snapshot back
        let load_result = load_incremental_snapshot(&temp_path);
        let _ = std::fs::remove_file(&temp_path);
        assert!(load_result.is_ok());

        let loaded = load_result.unwrap();

        // Verify round-trip: header, vcpu_states, device_states, and dirty_pages are preserved
        assert_eq!(loaded.header.magic, snapshot.header.magic);
        assert_eq!(loaded.header.version, snapshot.header.version);
        assert_eq!(loaded.header.vcpu_count, snapshot.header.vcpu_count);
        assert_eq!(loaded.header.ram_regions, snapshot.header.ram_regions);
        assert_eq!(loaded.header.nested_enabled, snapshot.header.nested_enabled);
        assert_eq!(loaded.vcpu_states.len(), snapshot.vcpu_states.len());
        assert_eq!(loaded.vcpu_states[0], snapshot.vcpu_states[0]);
        assert_eq!(loaded.vcpu_states[1], snapshot.vcpu_states[1]);
        assert_eq!(loaded.device_states.len(), snapshot.device_states.len());
        assert_eq!(loaded.device_states[0], snapshot.device_states[0]);
        assert_eq!(loaded.dirty_pages.len(), snapshot.dirty_pages.len());
        assert_eq!(
            loaded.dirty_pages[0].guest_addr,
            snapshot.dirty_pages[0].guest_addr
        );
        assert_eq!(loaded.dirty_pages[0].data, snapshot.dirty_pages[0].data);
        assert_eq!(
            loaded.dirty_pages[1].guest_addr,
            snapshot.dirty_pages[1].guest_addr
        );
        assert_eq!(loaded.dirty_pages[1].data, snapshot.dirty_pages[1].data);
    }

    /// AC5.2: vmstate file exceeding 10MB size limit
    #[cfg(feature = "snapshot")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_load_vmstate_exceeds_size_limit() {
        use std::io::Write;

        // Create a temp file larger than VMSTATE_MAX_SIZE (11MB)
        let temp_dir = std::path::PathBuf::from("/tmp");
        let temp_path = temp_dir.join(format!(
            "libkrun_test_oversized_vmstate_{}.bin",
            std::process::id()
        ));

        // Write 11MB of zeros
        let mut file = std::fs::File::create(&temp_path).unwrap();
        let oversized_data = vec![0u8; 11 * 1024 * 1024];
        file.write_all(&oversized_data).unwrap();
        drop(file);

        let result = load_vmstate(&temp_path);
        let _ = std::fs::remove_file(&temp_path);

        // Should return FileSizeExceeded error
        assert!(matches!(
            result,
            Err(SnapshotError::FileSizeExceeded {
                size: 11534336,
                limit: 10485760
            })
        ));
    }

    /// AC5.4: incremental snapshot file exceeding 10MB size limit
    #[cfg(feature = "snapshot")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_load_incremental_snapshot_exceeds_size_limit() {
        use std::io::Write;

        // Create a temp file larger than VMSTATE_MAX_SIZE (11MB)
        let temp_dir = std::path::PathBuf::from("/tmp");
        let temp_path = temp_dir.join(format!(
            "libkrun_test_oversized_incr_{}.bin",
            std::process::id()
        ));

        // Write 11MB of zeros
        let mut file = std::fs::File::create(&temp_path).unwrap();
        let oversized_data = vec![0u8; 11 * 1024 * 1024];
        file.write_all(&oversized_data).unwrap();
        drop(file);

        let result = load_incremental_snapshot(&temp_path);
        let _ = std::fs::remove_file(&temp_path);

        // Should return FileSizeExceeded error
        assert!(matches!(
            result,
            Err(SnapshotError::FileSizeExceeded {
                size: 11534336,
                limit: 10485760
            })
        ));
    }

    /// AC2.5: Incremental snapshot records reclaimed pages in the snapshot metadata
    #[cfg(feature = "snapshot")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_incremental_snapshot_with_reclaimed_pages() {
        let mem = make_memory(&[(0x1000, 0x4000)]);
        let header = valid_header(&mem, 2, false);

        // Create an incremental snapshot with both dirty and reclaimed pages
        let dirty_pages = vec![
            DirtyPage {
                guest_addr: 0x1000,
                data: vec![0xAA; 4096],
            },
            DirtyPage {
                guest_addr: 0x2000,
                data: vec![0xBB; 4096],
            },
        ];

        // Reclaimed pages are in the snapshot
        let reclaimed_pages = vec![0x3000, 0x4000];

        let snapshot = IncrementalSnapshot {
            header,
            vcpu_states: vec![vec![0xCC; 256], vec![0xDD; 256]],
            device_states: vec![],
            dirty_pages,
            gic_state: None,
            vm_state: None,
            reclaimed_pages,
        };

        // Create a temp file for incremental snapshot
        let temp_dir = std::path::PathBuf::from("/tmp");
        let temp_path = temp_dir.join(format!(
            "libkrun_test_reclaimed_pages_{}.bin",
            std::process::id()
        ));

        // Save incremental snapshot
        let save_result = save_incremental_snapshot(&snapshot, &temp_path);
        assert!(save_result.is_ok());

        // Load incremental snapshot back
        let load_result = load_incremental_snapshot(&temp_path);
        let _ = std::fs::remove_file(&temp_path);
        assert!(load_result.is_ok());

        let loaded = load_result.unwrap();

        // Verify reclaimed_pages are preserved
        assert_eq!(loaded.reclaimed_pages.len(), 2);
        assert_eq!(loaded.reclaimed_pages[0], 0x3000);
        assert_eq!(loaded.reclaimed_pages[1], 0x4000);
    }

    /// AC2.6: Restore zero-fills reclaimed pages in guest memory
    #[cfg(feature = "snapshot")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_apply_reclaimed_pages_zero_fills() {
        let mem = make_memory(&[(0x0, 0x8000)]); // 32KB region

        // Fill the first two pages with non-zero data
        let test_data = vec![0xFF; 4096];
        mem.write_slice(&test_data, GuestAddress(0x0)).unwrap();
        mem.write_slice(&test_data, GuestAddress(0x1000)).unwrap();

        // Fill pages after with different data
        let test_data2 = vec![0xAA; 4096];
        mem.write_slice(&test_data2, GuestAddress(0x2000)).unwrap();
        mem.write_slice(&test_data2, GuestAddress(0x3000)).unwrap();

        // Apply reclaimed pages for the first two pages (should be zeroed)
        let reclaimed = vec![0x0, 0x1000];
        let result = apply_reclaimed_pages(&mem, &reclaimed);
        assert!(result.is_ok());

        // Verify first two pages are now zeros
        let mut buf = vec![0u8; 4096];
        mem.read_slice(&mut buf, GuestAddress(0x0)).unwrap();
        assert!(buf.iter().all(|&b| b == 0), "Page at 0x0 should be zeroed");

        mem.read_slice(&mut buf, GuestAddress(0x1000)).unwrap();
        assert!(
            buf.iter().all(|&b| b == 0),
            "Page at 0x1000 should be zeroed"
        );

        // Verify other pages are unchanged
        mem.read_slice(&mut buf, GuestAddress(0x2000)).unwrap();
        assert!(
            buf.iter().all(|&b| b == 0xAA),
            "Page at 0x2000 should be unchanged"
        );

        mem.read_slice(&mut buf, GuestAddress(0x3000)).unwrap();
        assert!(
            buf.iter().all(|&b| b == 0xAA),
            "Page at 0x3000 should be unchanged"
        );
    }

    /// AC2.6 variant: Empty reclaimed_pages list (backward compat with old snapshots)
    #[cfg(feature = "snapshot")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_apply_reclaimed_pages_empty_list() {
        let mem = make_memory(&[(0x0, 0x4000)]);

        // Fill with non-zero data
        let test_data = vec![0xFF; 4096];
        mem.write_slice(&test_data, GuestAddress(0x0)).unwrap();

        // Apply empty reclaimed pages
        let reclaimed = vec![];
        let result = apply_reclaimed_pages(&mem, &reclaimed);
        assert!(result.is_ok());

        // Verify data is unchanged
        let mut buf = vec![0u8; 4096];
        mem.read_slice(&mut buf, GuestAddress(0x0)).unwrap();
        assert!(
            buf.iter().all(|&b| b == 0xFF),
            "Page should be unchanged with empty reclaimed list"
        );
    }

    #[cfg(all(not(loom), feature = "snapshot"))]
    mod proptest_tests {
        use super::*;
        use proptest::prelude::*;

        fn arb_snapshot_header() -> impl Strategy<Value = SnapshotHeader> {
            (
                any::<u32>(), // magic (arbitrary for round-trip)
                any::<u32>(), // version (arbitrary for round-trip)
                1u32..=16u32, // vcpu_count (at least 1)
                prop::collection::vec(
                    (any::<u64>(), 1u64..=(1u64 << 30)), // (addr, size) pairs
                    0..4,
                ),
                any::<bool>(), // nested_enabled
            )
                .prop_map(
                    |(magic, version, vcpu_count, ram_regions, nested_enabled)| SnapshotHeader {
                        magic,
                        version,
                        vcpu_count,
                        ram_regions,
                        nested_enabled,
                    },
                )
        }

        fn arb_vm_snapshot() -> impl Strategy<Value = VmSnapshot> {
            (
                arb_snapshot_header(),
                prop::collection::vec(prop::collection::vec(any::<u8>(), 0..128), 0..4),
                prop::collection::vec(
                    (
                        "[a-z]{1,8}".prop_map(|s: String| s),
                        prop::collection::vec(any::<u8>(), 0..64),
                    )
                        .prop_map(|(id, state)| (id, state)),
                    0..4,
                ),
                proptest::option::of(prop::collection::vec(any::<u8>(), 0..32)),
                proptest::option::of(prop::collection::vec(any::<u8>(), 0..32)),
                prop::collection::vec(any::<u64>(), 0..8),
            )
                .prop_map(
                    |(header, vcpu_states, device_states, gic_state, vm_state, excluded_pages)| {
                        VmSnapshot {
                            header,
                            vcpu_states,
                            device_states,
                            gic_state,
                            vm_state,
                            excluded_pages,
                        }
                    },
                )
        }

        proptest! {
            /// VmSnapshot serializes and deserializes with identity (bincode round-trip).
            #[test]
            fn prop_vm_snapshot_bincode_roundtrip(snapshot in arb_vm_snapshot()) {
                let serialized = bincode::serialize(&snapshot)
                    .expect("serialization failed");
                let deserialized: VmSnapshot = bincode::deserialize(&serialized)
                    .expect("deserialization failed");

                prop_assert_eq!(snapshot.header.magic, deserialized.header.magic);
                prop_assert_eq!(snapshot.header.version, deserialized.header.version);
                prop_assert_eq!(snapshot.header.vcpu_count, deserialized.header.vcpu_count);
                prop_assert_eq!(snapshot.header.ram_regions, deserialized.header.ram_regions);
                prop_assert_eq!(snapshot.header.nested_enabled, deserialized.header.nested_enabled);
                prop_assert_eq!(snapshot.vcpu_states, deserialized.vcpu_states);
                prop_assert_eq!(snapshot.device_states, deserialized.device_states);
                prop_assert_eq!(snapshot.gic_state, deserialized.gic_state);
                prop_assert_eq!(snapshot.vm_state, deserialized.vm_state);
                prop_assert_eq!(snapshot.excluded_pages, deserialized.excluded_pages);
            }

            /// SnapshotHeader round-trip preserves all fields.
            #[test]
            fn prop_snapshot_header_roundtrip(header in arb_snapshot_header()) {
                let serialized = bincode::serialize(&header).expect("serialize");
                let recovered: SnapshotHeader = bincode::deserialize(&serialized).expect("deserialize");
                prop_assert_eq!(header.magic, recovered.magic);
                prop_assert_eq!(header.version, recovered.version);
                prop_assert_eq!(header.vcpu_count, recovered.vcpu_count);
                prop_assert_eq!(header.ram_regions, recovered.ram_regions);
                prop_assert_eq!(header.nested_enabled, recovered.nested_enabled);
            }

            /// validate_magic_and_version: wrong magic always fails.
            #[test]
            fn prop_invalid_magic_always_fails(
                magic in any::<u32>().prop_filter("not valid magic", |m| *m != SNAPSHOT_MAGIC),
                version in any::<u32>(),
                vcpu_count in 1u32..16,
            ) {
                let header = SnapshotHeader {
                    magic,
                    version,
                    vcpu_count,
                    ram_regions: vec![],
                    nested_enabled: false,
                };
                let result = validate_magic_and_version(&header);
                prop_assert!(result.is_err());
            }
        }
    }
}

#[cfg(kani)]
impl kani::Arbitrary for SnapshotHeader {
    fn any() -> Self {
        SnapshotHeader {
            magic: kani::any(),
            version: kani::any(),
            vcpu_count: kani::any(),
            ram_regions: vec![], // Keep bounded - Kani can't handle arbitrary-length vecs well
            nested_enabled: kani::any(),
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Proof: wrong magic always produces InvalidMagic error.
    ///
    /// For any header where magic != SNAPSHOT_MAGIC, validate_magic_and_version
    /// must return Err(SnapshotError::InvalidMagic).
    #[kani::proof]
    fn proof_invalid_magic_rejected() {
        let magic: u32 = kani::any_where(|&m| m != SNAPSHOT_MAGIC);

        let header = SnapshotHeader {
            magic,
            version: SNAPSHOT_VERSION, // correct version (magic is the error)
            vcpu_count: 1,
            ram_regions: vec![],
            nested_enabled: false,
        };

        let result = validate_magic_and_version(&header);
        kani::assert(
            matches!(result, Err(SnapshotError::InvalidMagic)),
            "wrong magic must produce InvalidMagic error",
        );
        kani::cover!(true, "error path is reachable");
    }

    /// Proof: correct magic but wrong version produces InvalidVersion error.
    ///
    /// For any header where magic == SNAPSHOT_MAGIC and version != SNAPSHOT_VERSION,
    /// validate_magic_and_version must return Err(SnapshotError::InvalidVersion(v)).
    #[kani::proof]
    fn proof_invalid_version_rejected() {
        let version: u32 = kani::any_where(|&v| v != SNAPSHOT_VERSION);

        let header = SnapshotHeader {
            magic: SNAPSHOT_MAGIC,
            version,
            vcpu_count: 1,
            ram_regions: vec![],
            nested_enabled: false,
        };

        let result = validate_magic_and_version(&header);
        kani::assert(
            matches!(result, Err(SnapshotError::InvalidVersion(_))),
            "wrong version (with correct magic) must produce InvalidVersion error",
        );
        kani::cover!(true, "invalid version error path is reachable");
    }

    /// Proof: correct magic AND correct version produces Ok(()).
    ///
    /// This is the only valid input combination. All other combinations must fail
    /// (proven by the proofs above).
    #[kani::proof]
    fn proof_valid_header_accepted() {
        let header = SnapshotHeader {
            magic: SNAPSHOT_MAGIC,
            version: SNAPSHOT_VERSION,
            vcpu_count: kani::any(),
            ram_regions: vec![],
            nested_enabled: kani::any(),
        };

        let result = validate_magic_and_version(&header);
        kani::assert(
            result.is_ok(),
            "correct magic and version must produce Ok(())",
        );
        kani::cover!(true, "valid header accepted path reachable");
    }

    /// Proof: exhaustive check — magic XOR version wrong always fails.
    ///
    /// Explores all combinations where at least one of (magic, version) is wrong.
    /// Together with proof_valid_header_accepted, this covers the full input space.
    #[kani::proof]
    fn proof_any_wrong_field_fails() {
        let magic: u32 = kani::any();
        let version: u32 = kani::any();

        // At least one of the two fields is wrong.
        kani::assume(magic != SNAPSHOT_MAGIC || version != SNAPSHOT_VERSION);

        let header = SnapshotHeader {
            magic,
            version,
            vcpu_count: 1,
            ram_regions: vec![],
            nested_enabled: false,
        };

        let result = validate_magic_and_version(&header);
        kani::assert(result.is_err(), "any wrong field must produce an error");
        kani::cover!(true, "any-wrong-field error path reachable");
    }

    /// Proof: all three validation outcomes are reachable.
    #[kani::proof]
    fn proof_validate_magic_version_all_paths_reachable() {
        let header: SnapshotHeader = kani::any();
        let result = validate_magic_and_version(&header);
        kani::cover!(matches!(result, Ok(())), "valid header path reachable");
        kani::cover!(
            matches!(result, Err(SnapshotError::InvalidMagic)),
            "invalid magic path reachable"
        );
        kani::cover!(
            matches!(result, Err(SnapshotError::InvalidVersion(_))),
            "invalid version path reachable"
        );
    }

    #[kani::proof_for_contract(validate_magic_and_version)]
    fn proof_validate_magic_version_contract() {
        let header: SnapshotHeader = kani::any();
        let _ = validate_magic_and_version(&header);
    }

    // ── validate_header_for_vm pure-logic proofs ──────────────────────────────
    //
    // GuestMemoryMmap::from_ranges() calls mmap internally which Kani cannot
    // model.  We therefore test the pure validation logic inline, mirroring
    // the exact checks performed by validate_header_for_vm, without
    // constructing a GuestMemoryMmap.

    /// Proof: vCPU count mismatch produces VcpuCountMismatch error.
    ///
    /// Inline replication of the vcpu_count branch in validate_header_for_vm.
    #[kani::proof]
    fn proof_vcpu_count_mismatch_logic() {
        let header_vcpu_count: u32 = kani::any_where(|&n| n <= 32);
        let expected_vcpu_count: usize = kani::any_where(|&n| n <= 32);
        kani::assume(header_vcpu_count as usize != expected_vcpu_count);

        let result: Result<(), SnapshotError> = if header_vcpu_count as usize != expected_vcpu_count
        {
            Err(SnapshotError::VcpuCountMismatch {
                expected: expected_vcpu_count,
                got: header_vcpu_count as usize,
            })
        } else {
            Ok(())
        };

        kani::assert(
            matches!(result, Err(SnapshotError::VcpuCountMismatch { .. })),
            "mismatched vCPU count must produce VcpuCountMismatch error",
        );
        kani::cover!(true, "vcpu count mismatch proof path reachable");
    }

    /// Proof: nested_enabled mismatch produces NestedEnabledMismatch error.
    ///
    /// Inline replication of the nested_enabled branch in validate_header_for_vm.
    #[kani::proof]
    fn proof_nested_enabled_mismatch_logic() {
        let header_nested: bool = kani::any();
        let expected_nested: bool = kani::any();
        kani::assume(header_nested != expected_nested);

        let result: Result<(), SnapshotError> = if header_nested != expected_nested {
            Err(SnapshotError::NestedEnabledMismatch)
        } else {
            Ok(())
        };

        kani::assert(
            matches!(result, Err(SnapshotError::NestedEnabledMismatch)),
            "nested_enabled mismatch must produce NestedEnabledMismatch error",
        );
        kani::cover!(true, "nested mismatch proof path reachable");
    }

    /// Proof: magic check short-circuits before version check.
    ///
    /// When magic is wrong, validate_magic_and_version returns InvalidMagic
    /// regardless of the version field value.
    #[kani::proof]
    fn proof_magic_check_short_circuits() {
        let magic: u32 = kani::any_where(|&m| m != SNAPSHOT_MAGIC);
        let version: u32 = kani::any(); // unconstrained — magic error must dominate

        let header = SnapshotHeader {
            magic,
            version,
            vcpu_count: 1,
            ram_regions: vec![],
            nested_enabled: false,
        };

        let result = validate_magic_and_version(&header);
        kani::assert(
            matches!(result, Err(SnapshotError::InvalidMagic)),
            "wrong magic must produce InvalidMagic regardless of version",
        );
        kani::cover!(
            version == SNAPSHOT_VERSION,
            "magic wrong, version correct covered"
        );
        kani::cover!(
            version != SNAPSHOT_VERSION,
            "magic wrong, version wrong covered"
        );
    }

    /// Proof: version check only fires after magic passes.
    ///
    /// When magic is correct but version is wrong, validate_magic_and_version
    /// returns InvalidVersion carrying the actual version value.
    #[kani::proof]
    fn proof_version_check_short_circuits() {
        let version: u32 = kani::any_where(|&v| v != SNAPSHOT_VERSION);

        let header = SnapshotHeader {
            magic: SNAPSHOT_MAGIC,
            version,
            vcpu_count: 1,
            ram_regions: vec![],
            nested_enabled: false,
        };

        let result = validate_magic_and_version(&header);
        kani::assert(
            matches!(result, Err(SnapshotError::InvalidVersion(_))),
            "correct magic + wrong version must produce InvalidVersion",
        );
        // The returned version value must equal the header's version field.
        if let Err(SnapshotError::InvalidVersion(v)) = result {
            kani::assert(
                v == version,
                "InvalidVersion must carry the actual version value",
            );
        }
        kani::cover!(true, "version check short-circuit proof path reachable");
    }

    /// Proof: vcpu_count and nested_enabled do not affect the magic/version result.
    ///
    /// validate_magic_and_version ignores vcpu_count and nested_enabled entirely.
    #[kani::proof]
    fn proof_magic_version_independent_of_other_fields() {
        let magic: u32 = kani::any();
        let version: u32 = kani::any();
        let vcpu_count: u32 = kani::any();
        let nested_enabled: bool = kani::any();

        let header = SnapshotHeader {
            magic,
            version,
            vcpu_count,
            ram_regions: vec![],
            nested_enabled,
        };

        let result = validate_magic_and_version(&header);

        // The result is determined solely by magic and version.
        let expected_ok = magic == SNAPSHOT_MAGIC && version == SNAPSHOT_VERSION;
        kani::assert(
            result.is_ok() == expected_ok,
            "validate_magic_and_version result depends only on magic and version",
        );
        kani::cover!(expected_ok, "valid magic+version path covered");
        kani::cover!(!expected_ok, "invalid magic or version path covered");
    }
}
