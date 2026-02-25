# libkrun Crate

Last verified: 2026-02-25

## Purpose
Public API crate providing both C FFI (`krun_*` functions) and Rust `Builder` API for configuring and starting microVMs.

## Contracts
- **Exposes**: C API (`krun_set_vm_config`, `krun_start_enter`, etc.), Rust `Builder` struct, `Context` struct, `StartError` enum, `VmExit` enum (re-exported from vmm), `Builder::add_virtiofs_vhost_user()` (behind `vhost-user` feature)
- **Guarantees**:
  - `krun_set_vm_config` returns `-EINVAL` when `num_vcpus == 0`
  - `Builder::vm_config()` returns `Result<&mut Self, StartError>` (was infallible before)
  - `StartError::ZeroVcpus` variant for 0-vCPU validation
  - `StartError::TagTooLong(usize)` variant for filesystem tag > 36 bytes
  - `Builder::add_virtiofs_vhost_user(tag, socket_path, dax_window_mib)` returns `Err(TagTooLong)` if tag > 36 bytes; gated behind `vhost-user` + `not(tee)` features
  - `Context::run()` returns `Result<VmExit, StartError>` -- process stays alive after VM exits
  - `Context::restore_and_run()` returns `Result<VmExit, StartError>` -- same contract as `run()`
  - `VmExit::Shutdown { exit_code }` for normal guest shutdown, `VmExit::RebootRequested` for reboot, `VmExit::Error { message }` for fatal errors
- **Expects**: Callers set vm_config before start; valid feature flags at compile time

## Dependencies
- **Uses**: `vmm` (build_microvm, Vmm lifecycle, VmExit), `devices` (VirtioNetBackend, console, block, VhostUserFs)
- **Used by**: External consumers via C API or Rust crate
- **Boundary**: This is the outermost crate; nothing in src/ should depend on it

## Key Decisions
- `Builder::vm_config()` changed from `&mut Self` to `Result<&mut Self, StartError>` for validation
- C API and Rust API both validate num_vcpus > 0
- `Context::run()` polls `SharedVmExit` after each event loop tick to detect VM exit
- `Context` takes ownership of `SharedVmExit` from `BuiltVm` at construction time
- `VmExit` is re-exported as `pub use vmm::vm_exit::VmExit` for consumer convenience

## Key Files
- `lib.rs` - All API functions, Builder struct, Context struct, StartError enum (single-file crate)
