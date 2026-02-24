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

use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

pub const SNAPSHOT_MAGIC: u32 = 0x4B52_534E; // "KRSN"
pub const SNAPSHOT_VERSION: u32 = 1;

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
        }
    }
}

impl From<io::Error> for SnapshotError {
    fn from(e: io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

fn validate_magic_and_version(header: &SnapshotHeader) -> Result<(), SnapshotError> {
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

    let expected_layout = ram_layout(guest_memory);
    if header.ram_regions != expected_layout {
        return Err(SnapshotError::MemoryLayoutMismatch {
            expected: expected_layout,
            got: header.ram_regions.clone(),
        });
    }

    if header.vcpu_count as usize != expected_vcpu_count {
        return Err(SnapshotError::VcpuCountMismatch {
            expected: expected_vcpu_count,
            got: header.vcpu_count as usize,
        });
    }

    if header.nested_enabled != expected_nested_enabled {
        return Err(SnapshotError::NestedEnabledMismatch);
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
}

/// Dump guest memory to a file.
pub fn dump_memory(guest_memory: &GuestMemoryMmap, path: &Path) -> Result<(), SnapshotError> {
    let mut file = File::create(path)?;
    for region in guest_memory.iter() {
        let host_addr = guest_memory
            .get_host_address(region.start_addr())
            .map_err(|e| SnapshotError::Serialize(format!("Invalid guest address: {e}")))?;
        let len = region.len() as usize;
        let slice = unsafe { std::slice::from_raw_parts(host_addr, len) };
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
        let host_addr = guest_memory
            .get_host_address(region.start_addr())
            .map_err(|e| SnapshotError::Deserialize(format!("Invalid guest address: {e}")))?;
        let len = region.len() as usize;
        let slice = unsafe { std::slice::from_raw_parts_mut(host_addr, len) };
        file.read_exact(slice)?;
    }
    Ok(())
}

/// Compute total RAM size from guest memory regions.
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
}
