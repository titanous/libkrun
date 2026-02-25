# VMM Crate

Last verified: 2026-02-25

## Purpose
Core virtual machine manager. Orchestrates VM lifecycle: build, run, snapshot/restore, dirty page tracking.

## Contracts
- **Exposes**: `Vmm` struct (VM lifecycle), `build_microvm()`, snapshot/restore functions, `DirtyBitmap`, `VmExit` enum, `SharedVmExit` type, `VhostUserFsConfig` (behind `vhost-user` feature), `Vm::register_memory_region()`
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
  - `Vm::register_memory_region()` registers additional KVM memory slots (e.g., DAX windows); does NOT track slot in `mem_slots` (DAX is volatile cache)
  - When `vhost-user` feature is enabled, guest memory regions use memfd backing (file-backed) so vhost-user daemons can mmap them; kernel region also gets memfd backing
  - `VmResources::vhost_user_fs` stores `VhostUserFsConfig` list; `add_vhost_user_fs_device()` appends to it
  - `StartMicrovmError` gains `MmapDaxWindow`, `RegisterDaxMemoryRegion`, `RegisterVhostUserDevice`, `RegisterVhostUserFsDevice` variants (behind `vhost-user` feature)
  - `attach_vhost_user_fs_device` creates VhostUserFs, mmaps DAX memfd, registers DAX region with KVM, attaches to MMIO bus
- **Expects**: Valid `VmResources` from libkrun crate; KVM/HVF available at runtime

## Dependencies
- **Uses**: `devices` (mmio device manager, virtio devices, VhostUserFs), `arch`, `kernel`, `vm-memory`
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
- DAX KVM memory slots are NOT tracked in `mem_slots` (intentionally excluded from dirty tracking; DAX is volatile cache)
- When `vhost-user` feature is active, `create_guest_memory` creates memfd-backed regions; without the feature, anonymous mmap is used (no behavior change)

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
- `resources.rs` - `VmResources`, `VmDeviceInfo`, `VhostUserDeviceConfig` configuration types
- `vmm_config/vhost_user_fs.rs` - `VhostUserFsConfig` (tag, socket_path, dax_window_mib)

## Gotchas
- `create_full_snapshot` still hardcodes `nested_enabled: false` (pre-existing TODO)
- Vsock timesync quiesce is macOS-only; on Linux the timesync thread is not started
- `VcpuHandle::Drop` is `#[cfg(not(test))]` -- tests do not get automatic thread cleanup
