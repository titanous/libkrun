# VMM Crate

Last verified: 2026-02-24

## Purpose
Core virtual machine manager. Orchestrates VM lifecycle: build, run, snapshot/restore, dirty page tracking.

## Contracts
- **Exposes**: `Vmm` struct (VM lifecycle), `build_microvm()`, snapshot/restore functions, `DirtyBitmap`, `VmExit` enum, `SharedVmExit` type
- **Guarantees**:
  - `validate_header_for_vm` checks magic, version, RAM layout, vCPU count, and nested_enabled match
  - Incremental snapshots require `dirty_tracking_enabled` (returns `DirtyTrackingNotEnabled` otherwise)
  - `DirtyBitmap::mark_dirty` silently ignores out-of-bounds addresses (no panic)
  - Snapshot header includes `nested_enabled` field; restores validate it matches current VM
  - `Vmm::stop()` stores `VmExit` in shared state instead of calling `libc::_exit()` -- process stays alive
  - `VcpuHandle::drop()` signals vCPU threads and joins them (no leaked threads)
  - `BuiltVm::vm_exit()` returns a reference to `SharedVmExit` for the caller to poll
  - Full and incremental snapshots include PortIO device states on x86_64 (CMOS, serial, i8042)
  - Virtio used ring pages are explicitly marked dirty during incremental snapshots (host writes not tracked by KVM)
  - `load_vmstate` and `load_incremental_snapshot` reject files larger than 10MB (`FileSizeExceeded`)
  - x86_64 vCPU restore calls `kvmclock_ctrl` to notify guest of time discontinuity (warns on failure)
  - `MMIODeviceManager::restore_all_device_states` silently skips unknown device IDs (forward compat)
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
- `Vmm::stop()` accepts `VmExit` instead of `i32` exit code; no longer terminates the process
- vCPU threads exit via `should_exit` atomic flag (set by `Vmm::stop()`) and channel disconnect
- `VcpuEmulation::Rebooted` variant maps `KVM_SYSTEM_EVENT_RESET` to `FC_EXIT_CODE_REBOOT` (3)
- `VcpuHandle` has a production `Drop` impl (`#[cfg(not(test))]`) that signals and joins threads
- `resolve_vm_exit()` centralizes exit code to `VmExit` variant dispatch logic
- PortIO and MMIO device states share the `device_states` vec; both managers skip unknown IDs silently
- x86_64 `VcpuState` includes `tsc_khz: Option<u32>` with `#[serde(default)]` for backward compat
- `VMSTATE_MAX_SIZE` (10MB) caps deserialization to prevent OOM from corrupted files

## Invariants
- `validate_header_for_vm` is called before every snapshot restore (full and incremental)
- `SnapshotError` variants have Display impls used by integration tests for error matching
- `VmDeviceInfo` carries `vcpu_count` and `ram_mib` populated from `VmResources` in `build_microvm`
- `Vmm::stop()` runs exit observers before storing `VmExit` in shared state
- vCPU threads must exit before `VcpuHandle` is fully dropped (joined in `Drop`)
- `exited()` state polls `should_exit` flag + channel disconnect (replaces infinite `Barrier::wait`)
- PortIO device states are saved/restored alongside MMIO states in every snapshot operation (x86_64)
- Virtio used ring dirty marking runs before `collect_dirty_pages` in incremental snapshots

## Key Files
- `vm_exit.rs` - `VmExit` enum (Shutdown, RebootRequested, Error) and `SharedVmExit` type
- `snapshot.rs` - Snapshot format, validation, save/load functions, `VMSTATE_MAX_SIZE` limit
- `dirty_bitmap.rs` - Lock-free dirty page tracking for incremental snapshots
- `builder.rs` - `build_microvm()` VM construction, creates `SharedVmExit` and `vcpu_exit_flag`
- `lib.rs` - `Vmm` struct, `stop()`, `resolve_vm_exit()`, snapshot orchestration, used ring dirty marking
- `device_manager/legacy.rs` - `PortIODeviceManager` with snapshot save/restore (x86_64)
- `device_manager/kvm/mmio.rs` - `MMIODeviceManager`, `get_virtio_used_ring_ranges()`
- `linux/vstate.rs` - x86_64 vCPU: `tsc_khz`, `kvmclock_ctrl` on restore, `VcpuHandle::drop()`
- `macos/vstate.rs` - macOS HVF vCPU: `VcpuHandle::drop()` with channel disconnect + join
- `resources.rs` - `VmResources`, `VmDeviceInfo` configuration types

## Gotchas
- `create_full_snapshot` still hardcodes `nested_enabled: false` (pre-existing TODO)
- Vsock timesync quiesce is macOS-only; on Linux the timesync thread is not started
- `VcpuHandle::Drop` is `#[cfg(not(test))]` -- tests do not get automatic thread cleanup
