# Generic Virtiofs Test Plan

## Prerequisites

- Development environment set up per `flake.nix` (inside `nix develop` shell)
- `libkrunfw` symlinked to `test-prefix/lib64` (done by shellHook)
- Build succeeds: `make` completes without errors
- Unit tests pass: `cargo test -p devices --features net`
- Integration tests pass: `make test FEATURE_FLAGS="--features embedded_init"` (5-6/6 passing is normal due to known flakiness)

## Phase 1: DAX Mapper Bounds Checking

| Step | Action | Expected |
|------|--------|----------|
| 1.1 | Run `cargo test -p devices --features net -- dax_mapper` | All 10 dax_mapper unit tests pass |
| 1.2 | Inspect the output and confirm all 10 tests are listed | Each test should show `ok` status |

## Phase 2: Object Safety and Type Verification

| Step | Action | Expected |
|------|--------|----------|
| 2.1 | Run `cargo check -p devices --features net` | Compilation succeeds (proves FileSystem is object-safe, Inode/Handle are newtypes, Server is non-generic) |
| 2.2 | Run `cargo check -p vmm` | Compilation succeeds (proves FsMount holds Box\<dyn FileSystem\>, attach_fs_devices passes boxed backend) |
| 2.3 | Run `cargo check -p libkrun` | Compilation succeeds (proves FileSystem/passthrough/dax_mapper are re-exported, Builder API accepts Box\<dyn FileSystem\>) |

## Phase 3: macOS Code Removal Verification

| Step | Action | Expected |
|------|--------|----------|
| 3.1 | Run `grep -rn 'cfg.*target_os.*macos' src/devices/src/virtio/fs/` | Zero matches |
| 3.2 | Inspect `src/devices/src/virtio/fs/mod.rs` | `mod linux;` is NOT wrapped in `#[cfg(target_os = "linux")]` |

## Phase 4: Generic Virtiofs Integration Test

| Step | Action | Expected |
|------|--------|----------|
| 4.1 | Run `make test FEATURE_FLAGS="--features embedded_init"` and check `virtiofs-generic-passthrough` | Test passes: VM boots, guest reads test file, guest writes and reads back |
| 4.2 | Check full suite results | At least 30/33 pass (known flakiness in vsock/tsi). `virtiofs-generic-passthrough` should consistently pass |

## End-to-End: Existing Tests Through Refactored Path

Run `make test FEATURE_FLAGS="--features embedded_init"` and confirm pre-existing tests (`configure-vm-*`, `snapshot-*`, `rust-api-*`, `vm-exit-*`) pass at expected rates. These exercise `set_root()` -> `add_virtiofs_path()` -> `add_virtiofs()` delegation chain.

## Human Verification Required

### AC5.4: No direct libc::mmap in PassthroughFs DAX

1. Run `grep -n 'libc::mmap' src/devices/src/virtio/fs/linux/passthrough.rs`
2. Verify zero matches
3. Verify `setupmapping` exclusively uses `mapper.map_file()` and `mapper.map_data()`
4. Verify `removemapping` exclusively uses `mapper.unmap()`

### AC7.3: No DAX window when shm_size is None

1. In `src/vmm/src/builder.rs`, find `create_guest_memory`
2. Confirm `if let Some(shm_size) = mount.shm_size` guard skips `create_fs_region` when None
3. In `attach_fs_devices`, confirm `if let Some(shm_region) = shm_manager.fs_region(i)` conditionally sets SHM region

## Traceability

| AC | Automated Test | Manual Step |
|----|---------------|-------------|
| AC1.1-AC1.3 | Integration: `virtiofs-generic-passthrough` | Phase 4, 4.1 |
| AC1.4-AC1.5 | Unit: `dax_mapper::tests` | Phase 1, 1.1 |
| AC2.1-AC2.2 | Integration: `virtiofs-generic-passthrough` | Phase 4, 4.1 |
| AC3.1-AC3.2 | Compilation: test workspace | Phase 2, 2.3 |
| AC4.1-AC4.3 | Compilation: `cargo check -p devices` | Phase 2, 2.1 |
| AC5.1-AC5.3 | Integration: `virtiofs-generic-passthrough` | Phase 4, 4.1 |
| AC5.4 | Code inspection (grep) | Human Verification |
| AC6.1-AC6.2 | Compilation + Integration | Phase 2, 2.1 + Phase 4, 4.1 |
| AC7.1-AC7.2 | Integration: `virtiofs-generic-passthrough` | Phase 4, 4.1 |
| AC7.3 | Code inspection | Human Verification |
| AC7.4 | Existing tests (vhost-user-fs + set_root) | End-to-End |
| AC8.1 | Full test suite | Phase 4, 4.2 |
