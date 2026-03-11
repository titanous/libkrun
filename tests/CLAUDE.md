# Integration Tests

Last verified: 2026-03-10

## Purpose
Host/guest integration test workspace. Tests run inside real microVMs to verify end-to-end behavior.

## Contracts
- **Exposes**: `test_cases()` function returning all registered test cases
- **Guarantees**: Each test case has a unique name; `TestCase` implements both host and guest `Test` traits
- **Expects**: `embedded_init` feature enabled; libkrunfw available at runtime (symlinked in test-prefix)

## Dependencies
- **Uses**: `libkrun` crate (Rust API, with features: embedded_init, net, blk, snapshot, vhost-user, uffd); `nested` feature flag enables libkrun dependency in test_cases for L1 guest code
- **Boundary**: Test daemons (test_daemon, test_vsock_proxy) use `vm-memory` 0.18 with vendored `virtio-queue` 0.17 and `vhost-user-backend` 0.21

## Running Tests
```
just integration
just integration <name>
```
Tests are inherently flaky (VM + network timing). Some failures under load are expected.

## Key Decisions
- Separate Cargo.lock from root workspace (different dependency resolution)
- `host`/`guest` features are mutually exclusive (compile_error if both enabled)
- Test cases use `krun_rust.rs` helpers for Rust API tests (Builder pattern)
- `mem_block_backend.rs` provides in-memory AsyncBlockBackend for block tests (host-only)

## Test Cases (50 total)
- `configure-vm-*` - VM configuration tests (1cpu-256MiB, 2cpu-1GiB)
- `vsock-guest-connect` - Guest-initiated vsock connection
- `tsi-tcp-guest-connect`, `tsi-tcp-guest-listen` - TSI TCP connectivity tests
- `multiport-console` - Multiport console device test
- `snapshot-restore-full`, `snapshot-restore-incremental` - Full snapshot cycle
- `snapshot-serial-scratch` - Serial scratch register survives snapshot/restore
- `snapshot-block-data` - Block device data survives snapshot/restore
- `snapshot-incremental-state` - Incremental snapshot preserves guest state
- `snapshot-net-connectivity` - Network connectivity after snapshot/restore
- `snapshot-error-*` - Snapshot validation error paths (wrong-magic, vcpu-mismatch, nested-mismatch)
- `snapshot-rng-reseed` - Guest RNG reseeds after snapshot/restore
- `rust-api-*` - Builder/lifecycle API tests (zero-vcpu, device-info, pause/resume, shutdown)
- `vm-exit-clean-shutdown`, `vm-exit-observer` - VmExit handling and exit observer tests
- `custom-block-backend` - AsyncBlockBackend with in-memory backend
- `net-async-loopback` - AsyncNetBackend loopback ICMP echo
- `vhost-user-fs-dax-*` - Vhost-user FS with DAX modes (always, inode, never) + snapshot
- `vhost-user-vsock-*` - Vhost-user vsock (echo, fd, snapshot)
- `virtiofs-generic-passthrough` - Generic virtiofs with `Box<dyn FileSystem>`
- `virtiofs-minimal-fs` - Minimal FileSystem trait implementation (custom backend, no passthrough)
- `virtiofs-dax-snapshot` - Virtiofs DAX read/write with snapshot/restore cycle
- `uffd-*` - UFFD demand-paging tests (demand-page-only, preload-full, preload-partial, incremental-chain, error-handling, parallel-faults)
- `uffd-balloon-parallel` - UFFD restore with concurrent balloon inflation
- `balloon-inflate-deflate-stats` - Balloon inflate, deflate, and stats reporting
- `balloon-snapshot-excludes-pages` - Full snapshot excludes balloon-inflated pages
- `balloon-incremental-reclaimed` - Incremental snapshot records reclaimed pages
- `balloon-uffd-zero-fill` - UFFD restores balloon-excluded pages as zero
- `balloon-snapshot-uffd` - Balloon snapshot with UFFD restore
- `balloon-snapshot-race` - Snapshot during active balloon inflation
- `block-backend-errors` - Block backend error handling (FailingBlockBackend)
- `block-backend-slow` - Block backend latency tolerance (SlowBlockBackend)
- `block-snapshot-uffd` - Block device snapshot with UFFD restore
- `nested-virt` - Nested virtualization (host->L1->L2); requires host KVM nested support, skips if unavailable; uses `nested` feature flag

## Key Files
- `test_cases/src/lib.rs` - Test case registry (50 test cases)
- `test_cases/src/krun_rust.rs` - Rust API test helpers (Builder pattern)
- `test_cases/src/common.rs` - Shared test constants and utilities
- `test_cases/src/mem_block_backend.rs` - In-memory AsyncBlockBackend for block tests (host-only)
- `test_cases/src/failing_block_backend.rs` - Error-producing block backend for error path tests (host-only)
- `test_cases/src/slow_block_backend.rs` - Latency-injecting block backend for timeout tests (host-only)
- `test_cases/src/minimal_filesystem.rs` - Minimal FileSystem trait implementation for virtiofs tests (host-only)
- `test_cases/src/loopback_net.rs` - Loopback AsyncNetBackend for net tests (host-only)
- `test_cases/src/mock_snapshot_store.rs` - In-memory MockSnapshotStore for UFFD tests (host-only)
- `test_cases/src/net_helpers.rs` - Shared network config/ICMP helpers (guest-only)
- `test_cases/src/vsock_helpers.rs` - Shared vsock_connect helper with retry (guest-only)
- `test_daemon/` - Standalone vhost-user FS daemon binary
- `test_vsock_proxy/` - Standalone vhost-user vsock proxy binary (echo port 9999, counter query port 9998, DEVICE_STATE support)
- `test_cases/Cargo.toml` - Feature flags and dependency pins
