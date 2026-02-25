# Integration Tests

Last verified: 2026-02-25

## Purpose
Host/guest integration test workspace. Tests run inside real microVMs to verify end-to-end behavior.

## Contracts
- **Exposes**: `test_cases()` function returning all registered test cases
- **Guarantees**: Each test case has a unique name; `TestCase` implements both host and guest `Test` traits
- **Expects**: `embedded_init` feature enabled; libkrunfw available at runtime (symlinked in test-prefix)

## Dependencies
- **Uses**: `libkrun` crate (Rust API, with features: embedded_init, net, blk, snapshot, vhost-user), `krun-sys` (C API)
- **Boundary**: `vm-memory` pinned to 0.16.2 in workspace deps (0.17 breaks upstream crates)

## Running Tests
```
make test FEATURE_FLAGS="--features embedded_init"
```
Tests are inherently flaky (VM + network timing). 5-6/6 passing is normal.

## Key Decisions
- Separate Cargo.lock from root workspace (different dependency resolution)
- `host`/`guest` features are mutually exclusive (compile_error if both enabled)
- Test cases use `krun_rust.rs` helpers for Rust API tests (Builder pattern)
- `mem_block_backend.rs` provides in-memory AsyncBlockBackend for block tests (host-only)

## Test Cases
- `snapshot-restore-full`, `snapshot-restore-incremental` - Full snapshot cycle
- `snapshot-serial-scratch` - Verifies serial scratch register survives snapshot/restore
- `snapshot-block-data` - Verifies block device data survives snapshot/restore
- `snapshot-incremental-state` - Verifies incremental snapshot preserves guest state after workload
- `snapshot-net-connectivity` - Verifies network connectivity after snapshot/restore
- `snapshot-error-*` - Snapshot validation error paths
- `rust-api-*` - Builder/lifecycle API tests (zero-vcpu, device-info, pause/resume, shutdown)
- `vm-exit-clean-shutdown` - Verifies `Context::run()` returns `VmExit::Shutdown`, thread/FD/mmap cleanup
- `custom-block-backend` - AsyncBlockBackend with in-memory backend
- `net-async-loopback` - AsyncNetBackend loopback ICMP echo through CustomAsyncFactory
- `vhost-user-fs-dax-read` - Vhost-user FS DAX read (mounts virtio-fs, reads file via DAX window)
- `vhost-user-fs-dax-write` - Vhost-user FS DAX write (writes file via DAX window, verifies content)
- `vhost-user-fs-dax-snapshot` - Vhost-user FS snapshot/restore (verifies file content survives snapshot cycle)

## Key Files
- `test_cases/src/lib.rs` - Test case registry
- `test_cases/src/krun_rust.rs` - Rust API test helpers
- `test_cases/src/test_vm_exit.rs` - VM exit handling and resource cleanup tests
- `test_cases/src/mem_block_backend.rs` - In-memory block backend for tests
- `test_cases/src/loopback_net.rs` - Loopback AsyncNetBackend and factory for net tests (host-only)
- `test_cases/src/net_helpers.rs` - Shared network config/ICMP helpers for guest-side tests
- `test_cases/src/test_vhost_user_fs.rs` - Vhost-user FS integration tests (DAX read, write, snapshot)
- `test_daemon/` - Standalone vhost-user FS daemon binary for integration testing
- `test_cases/Cargo.toml` - Feature flags and dependency pins
