# libkrun Crate

Last verified: 2026-03-03

## Purpose
Public Rust API crate providing `Builder`, `Context`, and `VmHandle` for configuring and starting microVMs. C API removed.

## Contracts
- **Exposes**: Rust `Builder` struct, `Context` struct, `StartError` enum, `VmExit` enum (re-exported from vmm), `Builder::add_virtiofs_vhost_user()` (behind `vhost-user` feature), `Builder::add_vsock_vhost_user()` and `Builder::add_vsock_vhost_user_fd()` (behind `vhost-user` feature), `vmm::snapshot_store` re-export (behind `snapshot` feature), `VmHandle::snapshot_to_store()`, `VmHandle::incremental_snapshot_to_store()` (behind `snapshot` feature), re-exports of `devices::virtio::fs::{FileSystem, passthrough, dax_mapper}` (behind `not(tee)` feature), `Builder::enable_balloon()`, `BalloonHandle`, `BalloonResult`, `BalloonError`, `VmHandle::balloon()` (behind `not(tee)` feature)
- **Guarantees**:
  - `Builder::vm_config()` returns `Result<&mut Self, StartError>` and validates num_vcpus > 0
  - `StartError::ZeroVcpus` variant for 0-vCPU validation
  - `StartError::TagTooLong(usize)` variant for filesystem tag > 36 bytes
  - `Builder::add_virtiofs_vhost_user(tag, socket_path, dax_window_mib)` returns `Err(TagTooLong)` if tag > 36 bytes; gated behind `vhost-user` + `not(tee)` features
  - `Builder::add_vsock_vhost_user(socket_path)` configures vhost-user vsock via socket path; returns `Err(VsockConflict)` if explicit userspace vsock or another vhost-user vsock already configured
  - `Builder::add_vsock_vhost_user_fd(stream)` configures vhost-user vsock via pre-connected `UnixStream`; same mutual exclusivity as socket path variant
  - `StartError::VsockConflict` variant for mutual exclusivity between userspace vsock and vhost-user vsock
  - When vhost-user vsock is configured, the implicit userspace vsock device is skipped during VM build (port configs stored via `krun_add_vsock_port` are accepted but unused)
  - `Builder::add_virtiofs(tag, Box<dyn FileSystem + Send + Sync>, shm_size)` accepts any filesystem backend; gated behind `not(tee)` feature
  - `Builder::add_virtiofs_path(tag, host_path, shm_size, allow_root_dir_delete)` convenience method creating `PassthroughFs` internally; gated behind `not(tee)` feature
  - `Context::run()` returns `Result<VmExit, StartError>` -- process stays alive after VM exits
  - `Context::restore_and_run()` returns `Result<VmExit, StartError>` -- same contract as `run()`; on Linux delegates to `restore_and_run_with_store`
  - `Context::restore_and_run_with_store(factory)` accepts `Box<dyn SnapshotStoreFactory>`; Linux-only (returns error on other platforms)
  - `VmHandle::snapshot_to_store(store)` and `VmHandle::incremental_snapshot_to_store(store)` pause vCPUs, snapshot via store, resume vCPUs
  - `VmExit::Shutdown { exit_code }` for normal guest shutdown, `VmExit::RebootRequested` for reboot, `VmExit::Error { message }` for fatal errors
  - `Builder::enable_balloon()` sets `balloon_enabled` on VmResources; balloon device attached during VM build
  - `VmHandle::balloon()` returns `Option<&BalloonHandle>` -- `None` if balloon not enabled
  - `BalloonHandle::resize(target_mb)` sets inflation target in MB; returns `Err(BalloonError::DeviceNotActive)` if device not activated, `Err(BalloonError::TargetTooLarge { max_mb })` if target exceeds u32::MAX pages
  - `BalloonHandle::await_target(target_mb, stall_timeout, max_timeout)` blocks on condvar until guest reaches target; returns `BalloonResult::Reached(actual_mb)`, `BalloonResult::Stalled(actual_mb)`, or `Err(BalloonError::Timeout { actual })`
  - `BalloonHandle::actual()` returns current inflation in MB
  - `BalloonHandle` is `Clone` (wraps `Arc`)
- **Expects**: Callers set vm_config before start; valid feature flags at compile time; `enable_balloon()` must be called before `start()` for balloon to be available

## Dependencies
- **Uses**: `vmm` (build_microvm, Vmm lifecycle, VmExit), `devices` (VirtioNetBackend, console, block, Balloon, VhostUserFs, VhostUserVsock, FileSystem, passthrough, dax_mapper)
- **Used by**: External consumers via Rust crate
- **Boundary**: This is the outermost crate; nothing in src/ should depend on it

## Key Decisions
- `Builder::vm_config()` changed from `&mut Self` to `Result<&mut Self, StartError>` for validation
- Rust API validates num_vcpus > 0
- `Builder::add_virtiofs()` signature changed from `(tag, host_path)` to `(tag, Box<dyn FileSystem>, shm_size)` -- old convenience path moved to `add_virtiofs_path()`
- `FileSystem`, `passthrough`, `dax_mapper` re-exported from crate root for consumer use
- `Context::run()` polls `SharedVmExit` after each event loop tick to detect VM exit
- `Context` takes ownership of `SharedVmExit` from `BuiltVm` at construction time
- `VmExit` is re-exported as `pub use vmm::vm_exit::VmExit` for consumer convenience
- `vmm::snapshot_store` re-exported so consumers can implement custom `SnapshotStore` backends
- `restore_and_run` on Linux now delegates to `restore_and_run_with_store` with `FsSnapshotStoreFactory` (backward compatible)
- `BalloonHandle` wraps `Arc<Mutex<Balloon>>` + condvar; `Clone` for sharing across threads
- `BalloonHandle::await_target` uses condvar (not polling) for efficient blocking wait on guest balloon progress
- `BalloonError::TargetTooLarge` prevents u32 truncation when converting MB to pages

## Key Files
- `lib.rs` - All API functions, Builder struct, Context struct, StartError enum (single-file crate)
