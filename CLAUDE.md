# libkrun

Last verified: 2026-02-24

## Tech Stack
- Language: Rust (workspace) + C (init binary)
- Hypervisor: KVM (Linux), HVF (macOS)
- Network stack: smoltcp (proxy mode), tokio (async workers)
- Serialization: bincode (snapshots)
- Build: Makefile + Cargo workspace

## Commands
- `make` - Build release library (auto-builds init with `embedded_init` feature)
- `make test FEATURE_FLAGS="--features embedded_init"` - Run integration tests (embedded_init required or init.krun is empty)
- `cargo test -p devices --features net` - Run devices crate unit tests (net feature needed for proxy/async_worker tests)
- `cargo test -p devices --features net,snapshot` - Devices tests including snapshot-dependent tests
- `cargo test -p vmm --features snapshot` - VMM crate unit tests (snapshot feature for snapshot.rs tests)

## Project Structure
- `src/libkrun/` - Public C API (`krun_*` functions) and Rust `Builder` API
- `src/vmm/` - Virtual machine manager: builder, snapshot/restore, dirty tracking
- `src/devices/` - Virtio device implementations (net, console, block, vsock, etc.)
- `src/arch/`, `src/kernel/` - Architecture and kernel loading support
- `tests/` - Integration test workspace (host+guest test cases run inside VMs)
- `init/` - C init binary compiled for guest (embedded when `embedded_init` feature on)

## Feature Flags (Cargo)
- `embedded_init` - Embeds init binary in library; required for tests
- `net` - Enables virtio-net backends (tokio, smoltcp proxy deps)
- `blk` - Enables virtio-block backends (tokio, futures)
- `snapshot` - Enables snapshot/restore (serde, bincode)
- `efi` - EFI boot support (implies blk + net)

## Conventions
- Platform-specific code gated with `#[cfg(target_os = "...")]`
- Snapshot format is platform-agnostic (opaque vCPU state bytes)
- Feature flags gate optional dependencies; see `src/devices/Cargo.toml`
- Integration tests use host/guest split: `#[host]`/`#[guest]` proc macros
- See domain CLAUDE.md files for crate-specific contracts

## Boundaries
- `tests/Cargo.lock` is separate from root `Cargo.lock` (different workspace)
- `vm-memory` must be pinned to 0.16.2 in tests workspace (0.17 breaks kernel/arch)
- `init/init` is a C binary, not part of the Cargo workspace
