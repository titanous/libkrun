# Integration Tests

Last verified: 2026-03-03

## Purpose
Host/guest integration test workspace. Tests run inside real microVMs to verify end-to-end behavior.

## Contracts
- **Exposes**: `test_cases()` function returning all registered test cases
- **Guarantees**: Each test case has a unique name; `TestCase` implements both host and guest `Test` traits
- **Expects**: `embedded_init` feature enabled; libkrunfw available at runtime (symlinked in test-prefix)

## Dependencies
- **Uses**: `libkrun` crate (Rust API, with features: embedded_init, net, blk, snapshot, vhost-user, uffd)
- **Boundary**: Test daemons (test_daemon, test_vsock_proxy) use `vm-memory` 0.18 with vendored `virtio-queue` 0.17 and `vhost-user-backend` 0.21

## Running Tests
```
just integration
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
- `vhost-user-fs-dax-always` - Vhost-user FS with dax=always: DAX read (0xBB), DAX write (0xCC), snapshot/restore cycle
- `vhost-user-fs-dax-inode` - Vhost-user FS with dax=inode: per-inode DAX (hello.txt=DAX/0xBB, nodax.txt=FUSE_READ/0xAA), write, snapshot/restore
- `vhost-user-fs-dax-never` - Vhost-user FS with dax=never: FUSE_READ path (0xAA), snapshot/restore cycle
- `uffd-demand-page-only` - UFFD demand-paging: snapshot, restore via MockSnapshotStore with UFFD, verify guest state
- `uffd-preload-full` - UFFD with full preload: all pages preloaded before vCPU resume, zero faults expected
- `uffd-preload-partial` - UFFD with partial preload: some pages preloaded, remaining demand-paged
- `uffd-incremental-chain` - UFFD restore from incremental snapshot chain (base + incremental overlay)
- `uffd-error-handling` - UFFD error paths: store read failures during demand-paging
- `uffd-parallel-faults` - UFFD concurrent fault resolution: multiple vCPUs faulting simultaneously
- `virtiofs-generic-passthrough` - Generic virtiofs with `Box<dyn FileSystem>`: constructs `PassthroughFs` manually, passes via `Builder::add_virtiofs()`, verifies read/write through DAX
- `vhost-user-vsock-echo` - Vhost-user vsock via socket path: guest sends data to echo port (9999), verifies echoed response
- `vhost-user-vsock-fd` - Vhost-user vsock via pre-connected fd (`from_stream`): same echo test using fd-provisioned connection
- `vhost-user-vsock-snapshot` - Vhost-user vsock snapshot/restore: echo test, snapshot, restore with new proxy, verify counter query port (9998) returns accumulated byte count

## Key Files
- `test_cases/src/lib.rs` - Test case registry
- `test_cases/src/krun_rust.rs` - Rust API test helpers
- `test_cases/src/test_vm_exit.rs` - VM exit handling and resource cleanup tests
- `test_cases/src/mem_block_backend.rs` - In-memory block backend for tests
- `test_cases/src/loopback_net.rs` - Loopback AsyncNetBackend and factory for net tests (host-only)
- `test_cases/src/net_helpers.rs` - Shared network config/ICMP helpers for guest-side tests
- `test_cases/src/test_vhost_user_fs.rs` - Vhost-user FS integration tests (DAX read, write, snapshot)
- `test_daemon/` - Standalone vhost-user FS daemon binary for integration testing
- `test_cases/src/mock_snapshot_store.rs` - In-memory MockSnapshotStore and MockSnapshotStoreFactory for UFFD tests (host-only)
- `test_cases/src/test_uffd_demand_page.rs` - UFFD demand-page-only test
- `test_cases/src/test_uffd_preload.rs` - UFFD preload-full and preload-partial tests
- `test_cases/src/test_uffd_incremental.rs` - UFFD incremental chain test
- `test_cases/src/test_uffd_error.rs` - UFFD error handling test
- `test_cases/src/test_uffd_parallel.rs` - UFFD parallel faults test
- `test_cases/src/test_virtiofs_generic_passthrough.rs` - Generic virtiofs integration test (host constructs PassthroughFs, guest reads/writes)
- `test_cases/src/test_vhost_user_vsock.rs` - Vhost-user vsock integration tests (echo, fd, snapshot)
- `test_cases/src/vsock_helpers.rs` - Shared vsock_connect helper with retry for guest-side tests
- `test_vsock_proxy/` - Standalone vhost-user vsock proxy binary for integration testing (echo port 9999, counter query port 9998, DEVICE_STATE support)
- `test_cases/Cargo.toml` - Feature flags and dependency pins
