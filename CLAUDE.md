# libkrun

Last verified: 2026-03-10

## Tech Stack
- Language: Rust (workspace + init binary)
- Hypervisor: KVM (Linux), HVF (macOS)
- Network stack: tokio (async workers)
- Serialization: bincode-next 3.x (snapshots; serde compat bridge for KVM structs)
- Build: justfile + Cargo workspace

## Commands
- `just build-init` - Build the guest init binary (static musl, x86_64)
- `just check` - Format check + clippy (includes init crate)
- `just build` - Build init + release library
- `just test` - Run unit tests for all crates
- `just integration` - Run all integration tests (libkrunfw must be in test-prefix/lib64/)
- `just integration <name>` - Run a single named integration test
- `just all` - Full fast suite: check + test + miri + proptest + loom + shuttle
- `just safety` - Full safety suite: check + fuzz-all + asan + shuttle + kani
- `just miri` - Run pure-logic tests under Miri (requires nightly)
- `just proptest` - Property-based tests for bitmaps, GDT, address translation, round-trips
- `just loom` - Exhaustive concurrency testing on bitmap/tracker types (requires --release)
- `just fuzz <target> [duration]` - Run a single cargo-fuzz target (default 60s)
- `just fuzz-all [duration]` - Run all 5 fuzz targets sequentially
- `just asan` - Unit tests under AddressSanitizer (requires nightly)
- `just integration-asan` - Integration tests with ASan instrumentation
- `just shuttle [iterations]` - Randomized concurrency testing (default 1000 iterations)
- `just kani` - All Kani bounded model checking proofs
- `just kani-proof <name>` - Single Kani proof by harness name
- `just mutants` - Full mutation testing suite (unit + VM boot tests; requires /dev/kvm and libkrunfw)
- `just mutants-diff` - Mutation tests scoped to diff vs origin/main (same requirements as mutants)

## Project Structure
- `src/libkrun/` - Public Rust API (`Builder`, `Context`, `VmHandle`) -- crate type `lib` only (no cdylib)
- `src/vmm/` - Virtual machine manager: builder, snapshot/restore, dirty tracking
- `src/devices/` - Virtio and legacy device implementations (net, console, block, balloon, vsock, fs, vhost-user, serial, CMOS, i8042, RTC)
- `src/arch/`, `src/kernel/` - Architecture and kernel loading support
- `tests/` - Integration test workspace (host+guest test cases run inside VMs)
- `fuzz/` - Cargo-fuzz package with 5 harnesses (snapshot deser, FUSE parsing, block request, descriptor chain, vhost-user msg)
- Kani bounded model checking: 22 proofs live inline as `#[cfg(kani)] mod verification` in their respective source files (dirty_bitmap.rs, gdt.rs, snapshot.rs, page_tracker.rs, reclaimed_bitmap.rs)
- `init/` - Rust init binary (`init/src/main.rs`) compiled as static musl binary for guest (embedded when `embedded_init` feature on); separate Cargo workspace with `init/.cargo/config.toml` targeting x86_64-unknown-linux-musl
- `vendor/vhost/` - Patched vhost 0.15.0 crate (adds DEVICE_STATE protocol methods); used via `[patch.crates-io]`
- `vendor/vhost-user-backend/` - Patched vhost-user-backend 0.21.0 (vm-memory 0.18 compat); used by test daemons
- `vendor/virtio-queue/` - Patched virtio-queue 0.17.0 (vm-memory 0.18 compat); used by test daemons
- `tests/test_daemon/` - Vhost-user FS test daemon binary (used by integration tests)
- `tests/test_vsock_proxy/` - Vhost-user vsock proxy binary (echo + counter ports, DEVICE_STATE; used by integration tests)

## Feature Flags (Cargo)
- `embedded_init` - Embeds init binary in library; required for tests
- `net` - Enables virtio-net async backend (tokio, bytes)
- `blk` - Enables virtio-block backends (tokio, futures)
- `snapshot` - Enables snapshot/restore (bincode-next, futures, tokio); includes `SnapshotStore` trait and `FsSnapshotStore`; devices crate snapshot depends only on bincode-next (no serde)
- `efi` - EFI boot support (implies blk + net)
- `vhost-user` - Enables vhost-user device support (virtio-fs with DAX, vsock); gated by feature flag
- `uffd` - Enables userfaultfd demand-paging for snapshot restore (implies `snapshot`; Linux-only; adds `userfaultfd` crate)
- `shuttle` - Enables shuttle concurrency tests (devices, vmm); dev-dependency only

## Conventions
- Platform-specific code gated with `#[cfg(target_os = "...")]`
- Snapshot format is platform-agnostic (opaque vCPU state bytes); directory-based layout (`vmstate` + `memory` files per snapshot directory)
- Legacy devices implement `Snapshottable` trait; state serialized via `snapshot_serde` module (bincode-next with per-device byte limits) behind `snapshot` feature
- `snapshot_serde` module (`src/devices/src/snapshot_serde.rs`): centralized serialize/deserialize with `with_limit()` const generic to cap deserialization allocations; all device snapshot state goes through this module
- x86_64 VcpuState/VmState use serde derive + `bincode_next::serde` compat bridge (KVM structs derive serde, not bincode-next Encode/Decode); all other state structs use bincode-next Encode/Decode directly
- Platform-specific serial (x86_64, riscv64) re-export shared `serial_16550.rs` implementation
- Feature flags gate optional dependencies; see `src/devices/Cargo.toml`
- Integration tests use host/guest split: `#[host]`/`#[guest]` proc macros
- Virtio-FS uses generic `FileSystem` trait (`Box<dyn FileSystem + Send + Sync>`); `PassthroughFs` is the built-in backend; Linux-only (no macOS virtiofs)
- Loom shims: atomic types in `dirty_bitmap.rs`, `page_tracker.rs`, `reclaimed_bitmap.rs`, `request.rs` use `#[cfg(loom)] loom::sync::atomic` / `#[cfg(not(loom))] std::sync::atomic` for loom concurrency testing
- Test infrastructure: proptest (property-based), loom (exhaustive concurrency), shuttle (randomized concurrency), Miri (UB detection), cargo-fuzz (fuzzing), Kani (bounded model checking), cargo-mutants (mutation testing)
- See domain CLAUDE.md files for crate-specific contracts

## Debugging Guest Boot (earlycon)
To see early kernel boot messages (before hvc0 console is ready), enable earlycon:

1. Add to kernel cmdline in `src/vmm/src/vmm_config/kernel_cmdline.rs`:
   `earlycon=uart8250,io,0x3f8,115200 loglevel=15`
2. Add a debug serial else-block in `src/vmm/src/builder.rs` after the EFI serial setup:
   ```rust
   else {
       let serial_log = std::fs::File::create("/tmp/serial_earlycon.log")
           .expect("Failed to create serial log file");
       serial_devices.push(setup_serial_device(
           event_manager, None, Some(Box::new(serial_log)),
       )?);
   }
   ```
3. Boot output is written to `/tmp/serial_earlycon.log`. Earlycon stops when hvc0 takes over (`printk: legacy bootconsole [uart8250] disabled`).

The guest kernel must have `CONFIG_SERIAL_8250=y`, `CONFIG_SERIAL_8250_CONSOLE=y`, and `CONFIG_SERIAL_EARLYCON=y` (already set in `flake.nix` libkrunfw overlay).

## Boundaries
- `tests/Cargo.lock` is separate from root `Cargo.lock` (different workspace)
- Root workspace uses `vm-memory` 0.18; test daemons also use 0.18 with vendored patches for compatibility
- `init/` is a separate Rust workspace (own Cargo.toml with `[workspace]`), not part of the root Cargo workspace; built via `just build-init` targeting x86_64-unknown-linux-musl
- `vendor/vhost/`, `vendor/vhost-user-backend/`, `vendor/virtio-queue/` are patched via `[patch.crates-io]` in root `Cargo.toml`; do not update versions without verifying patches (DEVICE_STATE, vm-memory compat) are preserved
