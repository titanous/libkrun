# libkrun Crate

Last verified: 2026-02-24

## Purpose
Public API crate providing both C FFI (`krun_*` functions) and Rust `Builder` API for configuring and starting microVMs.

## Contracts
- **Exposes**: C API (`krun_set_vm_config`, `krun_start_enter`, etc.), Rust `Builder` struct, `StartError` enum
- **Guarantees**:
  - `krun_set_vm_config` returns `-EINVAL` when `num_vcpus == 0`
  - `Builder::vm_config()` returns `Result<&mut Self, StartError>` (was infallible before)
  - `StartError::ZeroVcpus` variant for 0-vCPU validation
- **Expects**: Callers set vm_config before start; valid feature flags at compile time

## Dependencies
- **Uses**: `vmm` (build_microvm, Vmm lifecycle), `devices` (VirtioNetBackend, console, block)
- **Used by**: External consumers via C API or Rust crate
- **Boundary**: This is the outermost crate; nothing in src/ should depend on it

## Key Decisions
- `Builder::vm_config()` changed from `&mut Self` to `Result<&mut Self, StartError>` for validation
- C API and Rust API both validate num_vcpus > 0

## Key Files
- `lib.rs` - All API functions, Builder struct, StartError enum (single-file crate)
