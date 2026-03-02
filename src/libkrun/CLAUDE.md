# libkrun Crate

Last verified: 2026-03-01

## Purpose
Public API crate providing both C FFI (`krun_*` functions) and Rust `Builder` API for configuring and starting microVMs.

## Contracts
- **Exposes**: C API (`krun_set_vm_config`, `krun_start_enter`, etc.), Rust `Builder` struct, `Context` struct, `StartError` enum, `VmExit` enum (re-exported from vmm), `Builder::add_virtiofs_vhost_user()` (behind `vhost-user` feature), `vmm::snapshot_store` re-export (behind `snapshot` feature), `VmHandle::snapshot_to_store()`, `VmHandle::incremental_snapshot_to_store()` (behind `snapshot` feature), re-exports of `devices::virtio::fs::{FileSystem, passthrough, dax_mapper}` (behind `not(tee)` feature)
- **Guarantees**:
  - `krun_set_vm_config` returns `-EINVAL` when `num_vcpus == 0`
  - `Builder::vm_config()` returns `Result<&mut Self, StartError>` (was infallible before)
  - `StartError::ZeroVcpus` variant for 0-vCPU validation
  - `StartError::TagTooLong(usize)` variant for filesystem tag > 36 bytes
  - `Builder::add_virtiofs_vhost_user(tag, socket_path, dax_window_mib)` returns `Err(TagTooLong)` if tag > 36 bytes; gated behind `vhost-user` + `not(tee)` features
  - `Builder::add_virtiofs(tag, Box<dyn FileSystem + Send + Sync>, shm_size)` accepts any filesystem backend; gated behind `not(tee)` feature
  - `Builder::add_virtiofs_path(tag, host_path, shm_size, allow_root_dir_delete)` convenience method creating `PassthroughFs` internally; gated behind `not(tee)` feature
  - `Context::run()` returns `Result<VmExit, StartError>` -- process stays alive after VM exits
  - `Context::restore_and_run()` returns `Result<VmExit, StartError>` -- same contract as `run()`; on Linux delegates to `restore_and_run_with_store`
  - `Context::restore_and_run_with_store(factory)` accepts `Box<dyn SnapshotStoreFactory>`; Linux-only (returns error on other platforms)
  - `VmHandle::snapshot_to_store(store)` and `VmHandle::incremental_snapshot_to_store(store)` pause vCPUs, snapshot via store, resume vCPUs
  - `VmExit::Shutdown { exit_code }` for normal guest shutdown, `VmExit::RebootRequested` for reboot, `VmExit::Error { message }` for fatal errors
- **Expects**: Callers set vm_config before start; valid feature flags at compile time

## Dependencies
- **Uses**: `vmm` (build_microvm, Vmm lifecycle, VmExit), `devices` (VirtioNetBackend, console, block, VhostUserFs, FileSystem, passthrough, dax_mapper)
- **Used by**: External consumers via C API or Rust crate
- **Boundary**: This is the outermost crate; nothing in src/ should depend on it

## Key Decisions
- `Builder::vm_config()` changed from `&mut Self` to `Result<&mut Self, StartError>` for validation
- C API and Rust API both validate num_vcpus > 0
- `Builder::add_virtiofs()` signature changed from `(tag, host_path)` to `(tag, Box<dyn FileSystem>, shm_size)` -- old convenience path moved to `add_virtiofs_path()`
- C API functions (`krun_add_virtiofs`, `krun_add_virtiofs2`) now delegate to `add_virtiofs_path()` internally
- `FileSystem`, `passthrough`, `dax_mapper` re-exported from crate root for consumer use
- `Context::run()` polls `SharedVmExit` after each event loop tick to detect VM exit
- `Context` takes ownership of `SharedVmExit` from `BuiltVm` at construction time
- `VmExit` is re-exported as `pub use vmm::vm_exit::VmExit` for consumer convenience
- `vmm::snapshot_store` re-exported so consumers can implement custom `SnapshotStore` backends
- `restore_and_run` on Linux now delegates to `restore_and_run_with_store` with `FsSnapshotStoreFactory` (backward compatible)

## Key Files
- `lib.rs` - All API functions, Builder struct, Context struct, StartError enum (single-file crate)
