# VMM Crate

Last verified: 2026-03-04

## Purpose
Core virtual machine manager. Orchestrates VM lifecycle: build, run, snapshot/restore, dirty page tracking.

## Contracts
- **Exposes**: `Vmm` struct (VM lifecycle, `get_balloon()`), `build_microvm()`, snapshot/restore functions, `DirtyBitmap`, `VmExit` enum, `SharedVmExit` type, `VhostUserFsConfig` (behind `vhost-user` feature), `Vm::register_memory_region()`, `snapshot_store` module (`SnapshotStore` trait, `SnapshotStoreFactory` trait, `FsSnapshotStore`, `FsSnapshotStoreFactory`) behind `snapshot` feature, `uffd` module (`UffdHandler`, `PageTracker`, `PageTrackerStats`, `LoadSource`, `UffdRegion`, `guest_to_host`, `host_to_guest`, `guest_addr_to_page_index`) behind `uffd` feature
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
  - `VmResources::fs` stores `Vec<FsMount>` (`FsMount { tag, fs: Box<dyn FileSystem + Send + Sync>, shm_size }`); `add_fs_mount()` appends to it (replaces old `FsDeviceConfig`/`add_fs_device`)
  - `attach_fs_devices` takes `&mut Vec<FsMount>` and `.drain(..)`s it (moves ownership of `Box<dyn FileSystem>` into `Fs` device)
  - `VmResources::vhost_user_fs` stores `VhostUserFsConfig` list; `add_vhost_user_fs_device()` appends to it
  - `VmResources::vhost_user_vsock` stores `Option<VhostUserVsockConfig>`; `set_vhost_user_vsock()` sets it (only one vsock device per VM)
  - `VhostUserVsockConfig` contains `VhostUserVsockConnection` enum: `SocketPath(String)` or `Stream(UnixStream)`
  - `StartMicrovmError` gains `MmapDaxWindow`, `RegisterDaxMemoryRegion`, `RegisterVhostUserDevice`, `RegisterVhostUserFsDevice`, `RegisterVhostUserVsockDevice` variants (behind `vhost-user` feature)
  - `attach_vhost_user_fs_device` creates VhostUserFs, mmaps DAX memfd, registers DAX region with KVM, attaches to MMIO bus
  - `attach_vhost_user_vsock_device` creates VhostUserVsock (via socket path or pre-connected stream), attaches to MMIO bus
  - `Vmm::get_balloon()` returns `Option<&Arc<Mutex<Balloon>>>` for API access to balloon device (behind `not(tee)` feature)
  - `VmResources::balloon_enabled` flag controls whether balloon device is attached during VM build
  - `build_microvm` attaches balloon device and stores `Arc<Mutex<Balloon>>` on `Vmm` when `balloon_enabled` is true
  - `VmSnapshot` has `excluded_pages: Vec<u64>` field (`#[serde(default)]` for backward compat); balloon-inflated pages excluded from full snapshots
  - `IncrementalSnapshot` has `reclaimed_pages: Vec<u64>` field (`#[serde(default)]` for backward compat); reclaimed pages zero-filled on restore
  - `apply_reclaimed_pages(mem, pages)` zero-fills reclaimed page addresses in guest memory during incremental restore
  - `SnapshotStore::read_page` returns `io::Result<Option<Vec<u8>>>` -- `None` means page was excluded (balloon-reclaimed); callers must handle absent pages
  - `SnapshotStore` trait has default no-op methods: `set_excluded_pages(Vec<u64>)` and `set_ram_regions(Vec<(u64, u64)>)` for balloon snapshot integration
  - `FsSnapshotStore` writes sparse memory files when excluded pages are set; stores `page_index` file mapping guest addresses to file offsets
  - `SnapshotStore` trait is object-safe (`dyn SnapshotStore`), `Send + Sync + 'static`; all async methods return `SendBoxFuture` (Send futures for tokio::spawn)
  - `SnapshotStoreFactory::create` consumes `Box<Self>` (factory is single-use)
  - `FsSnapshotStore` reads from directory-based layout: `base_path/vmstate`, `base_path/memory`, with ordered incremental directories each containing `vmstate`
  - `FsSnapshotStore::read_vmstate` with incrementals merges base header + latest incremental state (vcpu_states, device_states, gic_state, vm_state)
  - `FsSnapshotStore::read_page` checks dirty_page_index (newest-first) before falling back to base memory file
  - `FsSnapshotStore::preload` yields 4MB chunks with dirty pages overlaid from incrementals
  - `Vmm::restore_from_store` drains preload stream to eagerly populate guest memory (Linux-only)
  - `Vmm::restore_from_store_with_uffd(vmstate_bytes, store, rt)` creates UFFD handler, registers memory regions, restores device/vCPU states, signals handler ready, returns handler thread handle (Linux + `uffd` feature)
  - `Vmm::snapshot_to_store` and `Vmm::incremental_snapshot_to_store` write via `SnapshotStore` trait (both platforms)
  - `UffdHandler` runs on a dedicated thread; the caller passes in a `tokio::runtime::Runtime` via `UffdHandler::run(rt)`; preload and fault loop run concurrently via `futures::join!`
  - `UffdHandler` page fault resolution: reads page from store, copies via `uffd.copy()`, handles EEXIST races silently; when store returns `None` (excluded page), resolves via `uffd.zeropage()` with `LoadSource::Zero`
  - `PageTracker` uses atomic bitmap (`AtomicU64` words) for lock-free page tracking; `mark_loaded` deduplicates via atomic OR
  - `BuiltVm::restore_from_store(vmstate_bytes, store, &rt)` starts vCPUs paused, restores memory+state via eager preload, then resumes (Linux-only)
  - `BuiltVm::restore_from_store_with_uffd(vmstate_bytes, store, rt)` pre-validates vmstate at `BuiltVm` level (defense-in-depth), starts vCPUs paused, delegates to `Vmm::restore_from_store_with_uffd`, returns handler thread handle (Linux + `uffd` feature)
  - `restore_incremental_snapshot` now reads from `path.join("vmstate")` (directory-based format, not flat file)
  - `build_microvm` injects `KRUN_STDIN_DEV`, `KRUN_STDOUT_DEV`, `KRUN_STDERR_DEV` kernel cmdline params with virtio console port device paths (e.g., `/dev/vportNpM`) for named console ports (`krun-stdin`, `krun-stdout`, `krun-stderr`); guest init reads these to set up stdio redirects without scanning sysfs
- **Expects**: Valid `VmResources` from libkrun crate; KVM/HVF available at runtime

## Dependencies
- **Uses**: `devices` (mmio device manager, virtio devices, Balloon, VhostUserFs, VhostUserVsock), `arch`, `kernel`, `vm-memory`, `userfaultfd` (behind `uffd` feature), `tokio` + `futures` (behind `snapshot` feature)
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
- Snapshot save/restore refactored to use `SnapshotStore` trait internally; `create_full_snapshot`/`restore_from_snapshot` delegate to store-based methods
- `restore_device_and_vcpu_states` extracted as shared helper for both eager and UFFD restore paths
- Vmstate bytes are read in `Context`/`BuiltVm` and passed directly to `Vmm::restore_from_store_with_uffd`; a single `tokio::sync::oneshot` channel signals the UFFD handler that the main thread is ready for faults
- `Error::Snapshot(String)` variant on `vmm::Error` (behind `snapshot` feature) used for store/runtime errors in restore paths
- `Vmm` stores `Option<Arc<Mutex<Balloon>>>` for balloon device access; populated by `build_microvm` when `balloon_enabled`
- `SnapshotStore::read_page` returns `Option` to support excluded (balloon-reclaimed) pages without sentinel values
- `SnapshotStore::set_excluded_pages` and `set_ram_regions` have default no-op implementations so existing custom stores are unaffected
- `VmSnapshot::excluded_pages` and `IncrementalSnapshot::reclaimed_pages` use `#[serde(default)]` for backward-compatible deserialization
- `LoadSource::Zero` variant distinguishes zero-filled pages from store-loaded pages in `PageTracker` stats

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
- When `vhost-user` feature is active and any vhost-user device is configured (`vhost_user_devices`, `vhost_user_fs`, or `vhost_user_vsock`), `create_guest_memory` creates memfd-backed regions; without the feature, anonymous mmap is used (no behavior change)
- UFFD handler signals `VmExit::Error` on fatal page fault errors (store read failure, copy failure); EEXIST is non-fatal
- Preload errors are non-fatal; remaining pages are demand-paged via fault handler
- Full snapshots exclude balloon-reclaimed pages (inflated + reported-free); excluded page addresses stored in `VmSnapshot::excluded_pages`
- Incremental snapshots record reclaimed pages in `IncrementalSnapshot::reclaimed_pages`; `apply_reclaimed_pages` zero-fills them on restore
- UFFD fault handler logs warning and signals `VmExit::Error` when encountering an excluded page fault (should not happen in normal operation)

## Key Files
- `vm_exit.rs` - `VmExit` enum (Shutdown, RebootRequested, Error) and `SharedVmExit` type
- `snapshot.rs` - Snapshot format, validation, save/load functions, `VMSTATE_MAX_SIZE` limit
- `dirty_bitmap.rs` - Lock-free dirty page tracking for incremental snapshots
- `builder.rs` - `build_microvm()` VM construction, creates `SharedVmExit` and `vcpu_exit_flag`
- `snapshot_store.rs` - `SnapshotStore` trait, `SnapshotStoreFactory` trait, `FsSnapshotStore`, `FsSnapshotStoreFactory`; directory-based snapshot I/O with incremental overlay support
- `uffd/mod.rs` - Re-exports public UFFD types (`UffdHandler`, `PageTracker`, `PageTrackerStats`, `LoadSource`, `UffdRegion`, `guest_to_host`, `host_to_guest`, `guest_addr_to_page_index`)
- `uffd/handler.rs` - `UffdHandler` (UFFD demand-paging, fault loop, preload); behind `uffd` feature
- `uffd/page_tracker.rs` - `PageTracker` (atomic bitmap), `LoadSource`, `PageTrackerStats`, `UffdRegion`, address translation functions; behind `uffd` feature
- `lib.rs` - `Vmm` struct, `stop()`, `resolve_vm_exit()`, snapshot orchestration (store-based), `restore_device_and_vcpu_states`, used ring dirty marking
- `device_manager/legacy.rs` - `PortIODeviceManager` with snapshot save/restore (x86_64)
- `device_manager/kvm/mmio.rs` - `MMIODeviceManager`, `get_virtio_used_ring_ranges()`
- `linux/vstate.rs` - x86_64 vCPU: `tsc_khz`, `kvmclock_ctrl` on restore, `VcpuHandle::drop()`
- `macos/vstate.rs` - macOS HVF vCPU: `VcpuHandle::drop()` with channel disconnect + join
- `resources.rs` - `VmResources`, `VmDeviceInfo`, `VhostUserDeviceConfig` configuration types
- `vmm_config/fs.rs` - `FsMount` (tag, `Box<dyn FileSystem>`, shm_size)
- `vmm_config/vhost_user_fs.rs` - `VhostUserFsConfig` (tag, socket_path, dax_window_mib)
- `vmm_config/vhost_user_vsock.rs` - `VhostUserVsockConfig`, `VhostUserVsockConnection` (SocketPath or Stream)

## Gotchas
- `create_full_snapshot` still hardcodes `nested_enabled: false` (pre-existing TODO)
- Vsock timesync quiesce is macOS-only; on Linux the timesync thread is not started
- `VcpuHandle::Drop` is `#[cfg(not(test))]` -- tests do not get automatic thread cleanup
- UFFD handler receives its tokio runtime from the caller (`Context` creates it, passes through `BuiltVm` to `Vmm` to `UffdHandler::run`)
- `restore_incremental_snapshot` changed to directory-based path (`path.join("vmstate")`) -- callers must pass directory path, not file path
