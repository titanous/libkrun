# Test Coverage for rust-api-blk Branch Features

## Summary

The `rust-api-blk` branch extends libkrun — a library for creating lightweight, KVM-backed virtual machines — with a Rust-native API (`Builder` / `Context` / `VmHandle`), pluggable async block and network backends, and VM snapshot/restore support including incremental (dirty-page) snapshots. These features have meaningful surface area but currently lack dedicated tests. This document specifies a test suite that covers them.

The approach has two layers. Unit tests (Phases 1–4) run in-process without KVM and exercise individual modules — snapshot serialization, dirty-page bitmap tracking, the virtio-net async worker's TX/RX packet paths, the smoltcp-based network proxy, and the virtio console TX path — using mock backend implementations modeled on the existing `TrackingBackend` pattern. Integration tests (Phases 5–8) run real VMs via KVM and use the Rust API directly rather than the C FFI, making the API itself a test subject alongside the features it exposes. Three known bugs are intentionally covered by tests asserting the correct behavior; the code is fixed to satisfy the tests rather than the tests written to match the existing behavior.

## Definition of Done

All new features introduced on `rust-api-blk` have test coverage confirming correct behavior:

1. Unit tests for the block backend trait contract, net backend trait, net proxy, console TX, snapshot serialization, and dirty-page tracking edge cases — following the existing `TrackingBackend` mock pattern.
2. Integration test cases in the existing runner (host/guest split) for: snapshot/restore full cycle, Rust API builder, and end-to-end custom backend usage.
3. Tests cover both happy paths and meaningful error/edge cases.
4. All tests pass in CI.

## Acceptance Criteria

### test-coverage.AC1: Snapshot header serialization and validation
- **test-coverage.AC1.1 Success:** Valid `SnapshotHeader` serializes and deserializes to an identical value
- **test-coverage.AC1.2 Failure:** Wrong magic bytes → `SnapshotError::InvalidMagic`
- **test-coverage.AC1.3 Failure:** Version ≠ 1 → `SnapshotError::InvalidVersion(n)`
- **test-coverage.AC1.4 Failure:** vCPU count mismatch between header and current VM → `SnapshotError::VcpuCountMismatch`
- **test-coverage.AC1.5 Failure:** RAM region layout mismatch → `SnapshotError::MemoryLayoutMismatch`
- **test-coverage.AC1.6 Failure:** Memory file size mismatch → `SnapshotError::MemorySizeMismatch`
- **test-coverage.AC1.7 Failure:** Truncated vmstate file → `SnapshotError::Deserialize`
- **test-coverage.AC1.8 Failure:** `nested_enabled` differs between snapshot and current VM → error (new behavior; current code ignores this)
- **test-coverage.AC1.9 Failure:** `create_incremental_snapshot` called without prior `enable_dirty_tracking` → error (new behavior; current code silently produces an empty snapshot)

### test-coverage.AC2: Dirty bitmap edge cases
- **test-coverage.AC2.1 Success:** Page at exactly `num_pages - 1` (last valid index) is tracked and returned by `drain_dirty_pages`
- **test-coverage.AC2.2 Edge:** Page at `num_pages` (out-of-bounds) is silently ignored; no panic
- **test-coverage.AC2.3 Edge:** All pages marked dirty → `drain_dirty_pages` returns the full set
- **test-coverage.AC2.4 Edge:** No pages marked → `drain_dirty_pages` returns empty vec
- **test-coverage.AC2.5 Edge:** Same page marked dirty twice → appears exactly once in drain output

### test-coverage.AC3: Net async worker packet paths
- **test-coverage.AC3.1 Success:** TX empty packet (virtio header only, zero payload) → backend `handle_guest_tx` receives 0-byte slice
- **test-coverage.AC3.2 Success:** TX max-size packet (65535 B payload) → backend receives full payload intact
- **test-coverage.AC3.3 Success:** TX packet split across multiple virtio descriptors → payload reassembled correctly before delivery to backend
- **test-coverage.AC3.4 Edge:** TX descriptor with header smaller than `VIRTIO_NET_HDR_SIZE` → `read_tx_packet` returns `None`; descriptor consumed without panic
- **test-coverage.AC3.5 Success:** Backend injects RX packet via `to_guest_rx` channel → packet appears in guest RX virtio queue
- **test-coverage.AC3.6 Edge:** RX packet arrives with no guest RX buffers available → packet dropped; no panic
- **test-coverage.AC3.7 Success:** Backend implementing `save_snapshot_state` returning `Some(data)` → state bytes survive a quiesce → resync cycle
- **test-coverage.AC3.8 Success:** `NetBackendHandle` with `wake_rx = Some(_)` → `poll()` called when wake signal fires
- **test-coverage.AC3.9 Success:** Backend `poll_delay` returning `Some(d < 1s)` → poll timer fires within `d`

### test-coverage.AC4: Net proxy internals
- **test-coverage.AC4.1 Success:** `VirtualDevice::receive_raw_from_guest` strips the virtio-net header and delivers the raw Ethernet payload
- **test-coverage.AC4.2 Edge:** Packet with zero-length payload after header → backend receives empty slice; no panic
- **test-coverage.AC4.3 Success:** TCP SYN packet → `intercept_new_session` creates a host-side `TcpStream` and a smoltcp twin socket
- **test-coverage.AC4.4 Success:** First UDP datagram to a new endpoint → NAT entry created in `nat_table`
- **test-coverage.AC4.5 Success:** Second UDP datagram to the same endpoint → forwarded via existing NAT entry without creating a duplicate
- **test-coverage.AC4.6 Failure:** All ephemeral ports exhausted → port allocation returns an error (new behavior; current code loops indefinitely)

### test-coverage.AC5: Console TX processing
- **test-coverage.AC5.1 Success:** TX data written to an open port → bytes forwarded to the port's output sink correctly
- **test-coverage.AC5.2 Failure:** TX to a closed/absent port → returns an error; no panic
- **test-coverage.AC5.3 Edge:** Empty TX buffer (zero bytes) → handled without panic or error

### test-coverage.AC6: Snapshot/restore integration (full cycle)
- **test-coverage.AC6.1 Success:** VM starts, guest signals `"READY"` via vsock, host creates full snapshot, VM restores, guest verifies a pre-snapshot counter value is preserved
- **test-coverage.AC6.2 Success:** Full snapshot taken → dirty tracking enabled → guest modifies a memory region → incremental snapshot taken → restore from incremental → guest verifies only the written region changed
- **test-coverage.AC6.3 Failure:** Restore from file with wrong magic → `InvalidMagic` returned to caller
- **test-coverage.AC6.4 Failure:** Restore with vCPU count mismatch → `VcpuCountMismatch` returned
- **test-coverage.AC6.5 Failure:** Restore with `nested_enabled` mismatch → error returned

### test-coverage.AC7: Rust API and custom block backend
- **test-coverage.AC7.1 Failure:** `Builder` configured with 0 vCPUs → `BuildError` before VM starts
- **test-coverage.AC7.2 Success:** `Builder::build()` → `Context::device_info()` reflects the configured vCPU count and RAM size
- **test-coverage.AC7.3 Success:** `VmHandle::pause()` followed by `VmHandle::resume()` → guest continues execution and prints `"OK"`
- **test-coverage.AC7.4 Success:** `VmHandle::trigger_shutdown_event()` → VM process exits cleanly (zero exit code)
- **test-coverage.AC7.5 Success:** VM started with an in-memory `AsyncBlockBackend` pre-filled with test data → guest reads and verifies the data
- **test-coverage.AC7.6 Success:** Guest writes data to the custom block backend → host verifies the backend received the written bytes after VM exits

### test-coverage.AC8: Net proxy integration
- **test-coverage.AC8.1 Success:** Guest makes a TCP connection to a host listener through the smoltcp `ProxyNetWorker` backend, exchanges a `"PING"` / `"PONG"` message pair successfully

## Glossary

- **KVM**: Linux Kernel-based Virtual Machine — the kernel subsystem that provides hardware-accelerated virtualization. libkrun uses it to run VMs. Unit tests in this document do not require KVM; integration tests do.
- **vCPU**: Virtual CPU — a guest processor exposed to the VM. The snapshot header records the vCPU count; restoring to a VM with a different count is a hard error.
- **virtio**: A standardized I/O virtualization framework used between a host VMM and a guest OS. The block, network, and console devices in this codebase are all virtio devices.
- **virtio descriptor / descriptor chain**: The mechanism by which the guest passes buffers to a virtio device. A single logical I/O request can span multiple chained descriptors; the net worker must reassemble them before delivering to a backend.
- **virtio-net header (`VIRTIO_NET_HDR_SIZE`)**: A fixed-size metadata prefix that the guest prepends to every network packet. The net async worker strips this header before handing the raw Ethernet payload to a backend.
- **`AsyncBlockBackend` / `AsyncNetBackend`**: Rust traits that user-provided code implements to supply pluggable storage and network behavior to a VM. The block backend handles read/write/flush/discard; the net backend handles TX from the guest and can inject RX packets toward the guest.
- **`TrackingBackend`**: An `AsyncBlockBackend` mock implementation in `src/devices/src/virtio/block/async_worker.rs` that records calls and injects controlled responses using `Arc<Mutex<_>>` interior mutability. New unit-test mocks follow the same pattern.
- **`ProxyNetWorker`**: An `AsyncNetBackend` implementation that routes guest network traffic to the host using smoltcp for the protocol stack and a NAT table for UDP, without requiring a TAP device or elevated privileges.
- **smoltcp**: A no-std Rust TCP/IP stack used by `ProxyNetWorker`. It provides TCP socket management and packet processing in user space, bridged to the VM's virtio queues via the `VirtualDevice` struct.
- **`VirtualDevice`**: A smoltcp `Device` implementation that bridges the VM's virtio RX/TX queues to the smoltcp stack inside `ProxyNetWorker`.
- **NAT table (`nat_table`)**: A hash map inside `ProxyNetWorker` that records UDP source endpoint → host port mappings so subsequent datagrams to the same destination are forwarded via the existing entry.
- **Ephemeral port**: A short-lived port number allocated from a private range by `ProxyNetWorker` when establishing a new UDP NAT entry or TCP host connection. Exhaustion of this range is a known bug fixed in Phase 3.
- **`Builder` / `Context` / `VmHandle`**: The Rust-native libkrun API. `Builder` configures a VM; `Builder::build()` returns a `Context` (with `device_info()`) and a `VmHandle` for runtime control (`pause`, `resume`, `trigger_shutdown_event`).
- **`krun-sys`**: A Rust crate providing raw `unsafe extern "C"` bindings to the libkrun C FFI. Existing integration tests use this; new tests here use the Rust API directly.
- **`cdylib` / `rlib`**: Rust crate output types. `cdylib` produces a C-compatible shared library; `rlib` is the native Rust library format. libkrun declares both, enabling use as a C library and as a Rust test dependency.
- **`embedded_init`**: A Cargo feature that bundles the guest init binary into the libkrun shared library at build time. Required on `rust-api-blk` to boot VMs in tests.
- **virtiofs**: A virtio-based filesystem protocol that shares a host directory into the guest. Used by the test infrastructure to expose the root filesystem and guest-agent binary.
- **vsock**: A VM socket transport for communication between guest processes and the host without a network stack. Used in integration tests for coordination signals between guest and host.
- **Quiesce / resync**: The snapshot protocol for the net async worker. On quiesce, the worker pauses and publishes its queue and backend state. On resync (after restore), it reloads that state.
- **Incremental snapshot**: A snapshot recording only pages written since dirty tracking was enabled, rather than full VM memory. Produced by `create_incremental_snapshot` after `enable_dirty_tracking`.
- **Dirty-page tracking**: A mechanism by which the VMM marks which guest memory pages have been written since tracking was enabled. Implemented in `dirty_bitmap.rs`.
- **`bincode`**: A binary serialization format used to serialize `VmSnapshot` and `IncrementalSnapshot` to disk. Snapshot files are validated by a magic number (`0x4B52_534E`) and version field on load.
- **Nested virtualization (`nested_enabled`)**: A CPU feature flag allowing a hypervisor to run inside a guest VM. Recorded in the snapshot header; a mismatch between snapshot and target VM is a correctness bug fixed in Phase 1.
- **`setup_fs_and_enter`**: A test helper in `tests/test_cases/src/common.rs` that configures virtiofs, copies the guest-agent binary, and boots the VM. Reused by new integration tests.
- **Host/guest feature split**: A Cargo feature convention (`host` / `guest`) used in `tests/test_cases` to compile different code paths from the same source file — host-side orchestration under `#[cfg(feature = "host")]`, guest-side workloads under `#[cfg(feature = "guest")]`.
- **`Arc<Mutex<_>>` interior mutability**: Shared mutable state across threads — `Arc` for shared ownership, `Mutex` for exclusive access. Used by mock backends to record calls from async workers.

## Architecture

Two test layers, chosen to match the granularity of what each layer can meaningfully verify.

**Layer 1 — Unit tests** (in-process, no KVM required). Synchronous `#[test]` functions using mock trait implementations. Added to six currently-untested files in `src/`. Mock backends track calls and inject controlled responses; they never touch real virtio queues or hardware. This layer verifies trait contracts, serialization correctness, packet-path logic, and error propagation.

**Layer 2 — Integration tests** (KVM required, host/guest split). New test cases added to `tests/test_cases/src/`, registered in `tests/test_cases/src/lib.rs`, and run by the existing `tests/runner`. The host side uses the Rust API (`libkrun::Builder` / `Context` / `VmHandle`) directly rather than the C FFI, making the Rust API itself a test subject. Guest-side binaries are compiled from the same source file via the `guest`/`host` proc-macro feature split. Snapshot coordination uses vsock signaling: the guest connects to a host Unix socket, sends `"READY"`, and the host proceeds with the snapshot lifecycle.

**Bug-driven tests**: Three known bugs are covered by tests that assert the correct (not current) behavior. The implementation is fixed to satisfy the tests, not the other way around:
1. `create_incremental_snapshot` called without `enable_dirty_tracking` → must return an error (currently silently produces an empty snapshot).
2. `validate_header_for_vm` with mismatched `nested_enabled` flag → must return an error (currently ignored).
3. Ephemeral port exhaustion in `ProxyNetWorker` → must return an error (currently loops forever).

## Existing Patterns

**Unit test mock pattern:** `TrackingBackend` in `src/devices/src/virtio/block/async_worker.rs` — a full `AsyncBlockBackend` implementation using `Arc<Mutex<_>>` interior mutability to record calls and inject controlled I/O. All new unit-test mock backends follow this same shape.

**Minimal mock pattern:** `DummyNetBackend` in `src/devices/src/virtio/net/async_worker.rs` — a no-op `AsyncNetBackend`. The existing net tests use this for quiesce-protocol testing. New net unit tests extend it into a tracking variant that records `handle_guest_tx` calls and can push packets back via the `to_guest_rx` channel.

**Integration test structure:** `TestVsockGuestConnect` in `tests/test_cases/src/test_vsock_guest_connect.rs` — host spawns a Unix listener and a coordination thread, starts the VM via `setup_fs_and_enter`, and exchanges messages with the guest over a vsock-backed socket. New snapshot and net-proxy integration tests follow this exact coordination pattern.

**VM setup helper:** `common::setup_fs_and_enter` in `tests/test_cases/src/common.rs` — configures the virtiofs root, copies the guest-agent binary, and calls `krun_start_enter`. Reused without modification by new integration tests.

**New pattern introduced:** Integration tests that use the Rust API (`libkrun::Builder`) instead of the C FFI (`krun-sys`). This is a deliberate divergence from the existing test cases (which all use `krun-sys`) in order to make the Rust API itself a test subject. The `libkrun` crate already declares `crate-type = ["cdylib", "lib"]`, so it can be added as an `rlib` dependency in `tests/test_cases/Cargo.toml` under the `host` feature.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: Snapshot Unit Tests and Bug Fixes

**Goal:** Full unit-test coverage for `snapshot.rs` and `dirty_bitmap.rs`, plus fixes for the two snapshot-correctness bugs exposed by those tests.

**Components:**
- Unit tests in `src/vmm/src/snapshot.rs` — header roundtrip, all `SnapshotError` variants, `validate_header_for_vm` cases
- Unit tests in `src/vmm/src/dirty_bitmap.rs` — boundary pages, all-dirty, empty, deduplication
- Bug fix in `src/vmm/src/snapshot.rs` — `validate_header_for_vm` must reject `nested_enabled` mismatch
- Bug fix in `src/vmm/src/lib.rs` — `create_incremental_snapshot` / `enable_dirty_tracking` guard (Linux and macOS paths)

**Dependencies:** None

**Done when:** All unit tests for `test-coverage.AC1` and `test-coverage.AC2` pass; `cargo test -p vmm` succeeds
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: Net Async Worker Unit Tests

**Goal:** Unit-test the TX and RX packet paths, snapshot state persistence, and wake channel branches in `AsyncNetWorker`.

**Components:**
- Enhanced `TrackingNetBackend` mock in `src/devices/src/virtio/net/async_worker.rs` — records `handle_guest_tx` calls, can inject RX packets via channel, implements `save_snapshot_state` returning `Some(data)`
- Unit tests in `src/devices/src/virtio/net/async_worker.rs` — TX path (various sizes, multi-descriptor, invalid header), RX path (packet delivery, empty-queue drop), snapshot state survive quiesce→resync, `wake_rx` Some/None branches, `poll_delay` timer behaviour

**Dependencies:** None

**Done when:** All unit tests for `test-coverage.AC3` pass; `cargo test -p devices` succeeds
<!-- END_PHASE_2 -->

<!-- START_PHASE_3 -->
### Phase 3: Net Proxy Unit Tests and Bug Fix

**Goal:** Unit-test the `VirtualDevice` packet bridge and the TCP/UDP interception logic in `ProxyNetWorker`; fix the ephemeral port exhaustion loop.

**Components:**
- Unit tests in `src/devices/src/virtio/net/proxy.rs` — `VirtualDevice::receive_raw_from_guest` (header stripping, zero-payload), TCP SYN interception (host socket + smoltcp twin created), first UDP datagram (NAT entry created), second UDP datagram (forwarded via NAT table)
- Bug fix in `src/devices/src/virtio/net/proxy.rs` — ephemeral port allocation loop (lines ~1156–1164) replaced with bounded search returning an error when all ports are exhausted

**Dependencies:** None

**Done when:** All unit tests for `test-coverage.AC4` pass; `cargo test -p devices` succeeds
<!-- END_PHASE_3 -->

<!-- START_PHASE_4 -->
### Phase 4: Console TX Unit Tests

**Goal:** Unit-test the virtio console TX processing path.

**Components:**
- Unit tests in `src/devices/src/virtio/console/process_tx.rs` — data forwarded to port output, closed-port error without panic, empty buffer handled without panic

**Dependencies:** None

**Done when:** All unit tests for `test-coverage.AC5` pass; `cargo test -p devices` succeeds
<!-- END_PHASE_4 -->

<!-- START_PHASE_5 -->
### Phase 5: Integration Test Infrastructure

**Goal:** Wire the `libkrun` Rust API crate into the integration test workspace so subsequent phases can use `Builder` / `Context` / `VmHandle` directly.

**Components:**
- `tests/test_cases/Cargo.toml` — add `libkrun = { path = "../../src/libkrun", features = ["embedded_init"] }` under the `host` feature dependency list
- `tests/test_cases/src/krun_rust.rs` (new) — thin host-side helpers wrapping `libkrun::Builder` for common VM setup (filesystem config, vsock port registration), mirroring `krun.rs` for the C API

**Dependencies:** Phases 1–4 complete (ensures libkrun builds cleanly with bug fixes applied)

**Done when:** `cargo build --features host -p test_cases` succeeds with the new dependency; helpers compile without errors
<!-- END_PHASE_5 -->

<!-- START_PHASE_6 -->
### Phase 6: Snapshot Integration Tests

**Goal:** End-to-end snapshot/restore tests using the existing runner, covering the full cycle and all validated error paths.

**Components:**
- `tests/test_cases/src/test_snapshot_restore.rs` — `TestSnapshotRestore` struct; host uses Rust API to pause, full-snapshot, restore, resume; guest writes a counter before ready signal and verifies it after restore; also contains `TestSnapshotRestoreIncremental` variant
- `tests/test_cases/src/test_snapshot_errors.rs` — host-only tests (no guest) that restore from intentionally malformed snapshot files and assert specific `SnapshotError` variants
- Registration in `tests/test_cases/src/lib.rs`

**Dependencies:** Phase 5 (Rust API available in test_cases)

**Done when:** All tests for `test-coverage.AC6` pass via `make test`
<!-- END_PHASE_6 -->

<!-- START_PHASE_7 -->
### Phase 7: Rust API and Custom Block Backend Integration Tests

**Goal:** Validate the `Builder` / `Context` / `VmHandle` lifecycle and an end-to-end custom `AsyncBlockBackend`.

**Components:**
- `tests/test_cases/src/test_rust_api.rs` — `TestRustApi`: Builder error on 0 vCPUs; `device_info()` reflects config; pause → resume → guest prints `"OK"`; `trigger_shutdown_event()` clean exit
- `tests/test_cases/src/test_custom_block_backend.rs` — `TestCustomBlockBackend`: in-memory `AsyncBlockBackend` pre-filled with test data; guest reads and verifies content; guest writes new data; host verifies backend received it after VM exits
- Registration in `tests/test_cases/src/lib.rs`

**Dependencies:** Phase 5 (Rust API available in test_cases)

**Done when:** All tests for `test-coverage.AC7` pass via `make test`
<!-- END_PHASE_7 -->

<!-- START_PHASE_8 -->
### Phase 8: Net Proxy Integration Test

**Goal:** End-to-end validation of `ProxyNetWorker` (smoltcp-based) as a VM network backend.

**Components:**
- `tests/test_cases/src/test_net_proxy.rs` — `TestNetProxy`: host starts TCP listener, starts VM with `VirtioNetBackend::CustomAsyncFactory` wrapping a smoltcp `ProxyNetWorker`; guest connects to host listener via TCP, exchanges `"PING"` / `"PONG"`
- Registration in `tests/test_cases/src/lib.rs`

**Dependencies:** Phase 5 (Rust API available in test_cases), Phase 3 (ephemeral port fix)

**Done when:** `test-coverage.AC8.1` passes via `make test`
<!-- END_PHASE_8 -->

## Additional Considerations

**Bug-driven test ordering:** Tests in Phases 1, 3, and 6 assert behavior that the current code does not satisfy. These tests will fail until the associated code fixes are applied. The fixes are scoped within the same phase as their tests — each phase ends with passing tests.

**KVM availability:** Unit test phases (1–4) have no hardware requirement and run anywhere. Integration test phases (6–8) require `/dev/kvm` and are gated by the existing CI environment, which already provisions KVM for the `make test` target.

**Block backend trait coverage:** `BlockBackend` (synchronous) and `AsyncBlockBackend` (async) trait contracts are exercised through Phase 7's in-memory custom backend integration test. The existing 24 unit tests in `src/devices/src/virtio/block/async_worker.rs` already cover the async worker internals thoroughly; no additional unit tests are needed there.
