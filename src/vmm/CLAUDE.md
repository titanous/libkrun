# VMM Crate

Last verified: 2026-02-24

## Purpose
Core virtual machine manager. Orchestrates VM lifecycle: build, run, snapshot/restore, dirty page tracking.

## Contracts
- **Exposes**: `Vmm` struct (VM lifecycle), `build_microvm()`, snapshot/restore functions, `DirtyBitmap`
- **Guarantees**:
  - `validate_header_for_vm` checks magic, version, RAM layout, vCPU count, and nested_enabled match
  - Incremental snapshots require `dirty_tracking_enabled` (returns `DirtyTrackingNotEnabled` otherwise)
  - `DirtyBitmap::mark_dirty` silently ignores out-of-bounds addresses (no panic)
  - Snapshot header includes `nested_enabled` field; restores validate it matches current VM
- **Expects**: Valid `VmResources` from libkrun crate; KVM/HVF available at runtime

## Dependencies
- **Uses**: `devices` (mmio device manager, virtio devices), `arch`, `kernel`, `vm-memory`
- **Used by**: `libkrun` (public API crate)
- **Boundary**: Does not know about C API; only receives structured `VmResources`

## Key Decisions
- `nested_enabled` tracked on `Vmm` struct and validated on restore (was previously hardcoded to false)
- `dirty_tracking_enabled` is an explicit flag on `Vmm`; set to true when dirty tracking starts
- `DirtyBitmap::mark_dirty` uses silent bounds check instead of `debug_assert!` (safe for vCPU fault handlers)
- Snapshot format version 1, magic `0x4B52_534E` ("KRSN")

## Invariants
- `validate_header_for_vm` is called before every snapshot restore (full and incremental)
- `SnapshotError` variants have Display impls used by integration tests for error matching
- `VmDeviceInfo` carries `vcpu_count` and `ram_mib` populated from `VmResources` in `build_microvm`

## Key Files
- `snapshot.rs` - Snapshot format, validation, save/load functions
- `dirty_bitmap.rs` - Lock-free dirty page tracking for incremental snapshots
- `builder.rs` - `build_microvm()` VM construction
- `lib.rs` - `Vmm` struct, snapshot orchestration, dirty tracking state
- `resources.rs` - `VmResources`, `VmDeviceInfo` configuration types

## Gotchas
- `create_full_snapshot` still hardcodes `nested_enabled: false` (pre-existing TODO)
- Vsock timesync quiesce is macOS-only; on Linux the timesync thread is not started
