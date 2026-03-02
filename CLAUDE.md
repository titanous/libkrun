# libkrun

Last verified: 2026-03-02

## Tech Stack
- Language: Rust (workspace) + C (init binary)
- Hypervisor: KVM (Linux), HVF (macOS)
- Network stack: tokio (async workers)
- Serialization: bincode (snapshots)
- Build: Makefile + Cargo workspace

## Commands
- `make` - Build release library (auto-builds init with `embedded_init` feature)
- `make test FEATURE_FLAGS="--features embedded_init"` - Run integration tests (embedded_init required or init.krun is empty)
- `cargo test -p devices --features net` - Run devices crate unit tests (net feature needed for async_worker tests)
- `cargo test -p devices --features net,snapshot` - Devices tests including snapshot-dependent tests
- `cargo test -p vmm --features snapshot` - VMM crate unit tests (snapshot feature for snapshot.rs tests)

## Project Structure
- `src/libkrun/` - Public C API (`krun_*` functions) and Rust `Builder` API
- `src/vmm/` - Virtual machine manager: builder, snapshot/restore, dirty tracking
- `src/devices/` - Virtio and legacy device implementations (net, console, block, balloon, vsock, fs, vhost-user, serial, CMOS, i8042, RTC)
- `src/arch/`, `src/kernel/` - Architecture and kernel loading support
- `tests/` - Integration test workspace (host+guest test cases run inside VMs)
- `init/` - C init binary compiled for guest (embedded when `embedded_init` feature on)
- `vendor/vhost/` - Patched vhost 0.15.0 crate (adds DEVICE_STATE protocol methods); used via `[patch.crates-io]`
- `vendor/vhost-user-backend/` - Patched vhost-user-backend 0.21.0 (vm-memory 0.18 compat); used by test daemons
- `vendor/virtio-queue/` - Patched virtio-queue 0.17.0 (vm-memory 0.18 compat); used by test daemons
- `tests/test_daemon/` - Vhost-user FS test daemon binary (used by integration tests)
- `tests/test_vsock_proxy/` - Vhost-user vsock proxy binary (echo + counter ports, DEVICE_STATE; used by integration tests)

## Feature Flags (Cargo)
- `embedded_init` - Embeds init binary in library; required for tests
- `net` - Enables virtio-net async backend (tokio, bytes)
- `blk` - Enables virtio-block backends (tokio, futures)
- `snapshot` - Enables snapshot/restore (serde, bincode, futures, tokio); includes `SnapshotStore` trait and `FsSnapshotStore`
- `efi` - EFI boot support (implies blk + net)
- `vhost-user` - Enables vhost-user device support (virtio-fs with DAX, vsock); build with `VHOST_USER=1 make`
- `uffd` - Enables userfaultfd demand-paging for snapshot restore (implies `snapshot`; Linux-only; adds `userfaultfd` crate)

## Conventions
- Platform-specific code gated with `#[cfg(target_os = "...")]`
- Snapshot format is platform-agnostic (opaque vCPU state bytes); directory-based layout (`vmstate` + `memory` files per snapshot directory)
- Legacy devices implement `Snapshottable` trait; state serialized with bincode behind `snapshot` feature
- Platform-specific serial (x86_64, riscv64) re-export shared `serial_16550.rs` implementation
- Feature flags gate optional dependencies; see `src/devices/Cargo.toml`
- Integration tests use host/guest split: `#[host]`/`#[guest]` proc macros
- Virtio-FS uses generic `FileSystem` trait (`Box<dyn FileSystem + Send + Sync>`); `PassthroughFs` is the built-in backend; Linux-only (no macOS virtiofs)
- See domain CLAUDE.md files for crate-specific contracts

## Boundaries
- `tests/Cargo.lock` is separate from root `Cargo.lock` (different workspace)
- Root workspace uses `vm-memory` 0.18; test daemons also use 0.18 with vendored patches for compatibility
- `init/init` is a C binary, not part of the Cargo workspace
- `vendor/vhost/`, `vendor/vhost-user-backend/`, `vendor/virtio-queue/` are patched via `[patch.crates-io]` in root `Cargo.toml`; do not update versions without verifying patches (DEVICE_STATE, vm-memory compat) are preserved
