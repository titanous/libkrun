# Test Requirements: test-coverage

This document maps every acceptance criterion from the [test-coverage design plan](../../design-plans/2026-02-23-test-coverage.md) to either an automated test or a documented human verification step. Each entry includes the criterion text, test type, expected file path, and what the test verifies. The mappings are rationalized against the implementation decisions made in the eight phase plans.

## Conventions

- **Unit tests** run in-process without KVM (`#[test]` or `#[tokio::test]` inside `#[cfg(test)]` modules).
- **Integration tests** (e2e) run real VMs via KVM using the existing test runner (`make test`). They are registered as `TestCase` entries in `tests/test_cases/src/lib.rs`.
- **Test file paths** are relative to the repository root.
- **Test names** are the expected function or struct names from the phase plans. Implementors may adjust names; the AC mapping remains the same.

---

## Phase 1: Snapshot Header Serialization and Validation (AC1)

### test-coverage.AC1.1 -- Valid header roundtrip

- **Criterion:** Valid `SnapshotHeader` serializes and deserializes to an identical value.
- **Test type:** Unit
- **File:** `src/vmm/src/snapshot.rs` (`mod tests`)
- **Test name:** `test_header_roundtrip`
- **Verifies:** `save_vmstate` followed by `load_vmstate` produces a struct equal to the original.

### test-coverage.AC1.2 -- Wrong magic bytes

- **Criterion:** Wrong magic bytes produce `SnapshotError::InvalidMagic`.
- **Test type:** Unit
- **File:** `src/vmm/src/snapshot.rs` (`mod tests`)
- **Test name:** `test_invalid_magic`
- **Verifies:** A header with `magic = 0xDEADBEEF` fails validation with `InvalidMagic`.

### test-coverage.AC1.3 -- Wrong version

- **Criterion:** Version != 1 produces `SnapshotError::InvalidVersion(n)`.
- **Test type:** Unit
- **File:** `src/vmm/src/snapshot.rs` (`mod tests`)
- **Test name:** `test_invalid_version`
- **Verifies:** A header with `version = 99` fails with `InvalidVersion(99)`.

### test-coverage.AC1.4 -- vCPU count mismatch

- **Criterion:** vCPU count mismatch between header and current VM produces `SnapshotError::VcpuCountMismatch`.
- **Test type:** Unit
- **File:** `src/vmm/src/snapshot.rs` (`mod tests`)
- **Test name:** `test_vcpu_count_mismatch`
- **Verifies:** `validate_header_for_vm` with header `vcpu_count=2` and expected 4 returns `VcpuCountMismatch { expected: 4, got: 2 }`.

### test-coverage.AC1.5 -- RAM region layout mismatch

- **Criterion:** RAM region layout mismatch produces `SnapshotError::MemoryLayoutMismatch`.
- **Test type:** Unit
- **File:** `src/vmm/src/snapshot.rs` (`mod tests`)
- **Test name:** `test_layout_mismatch`
- **Verifies:** A header with a different number of RAM regions than the `GuestMemoryMmap` returns `MemoryLayoutMismatch`.

### test-coverage.AC1.6 -- Memory file size mismatch

- **Criterion:** Memory file size mismatch produces `SnapshotError::MemorySizeMismatch`.
- **Test type:** Unit
- **File:** `src/vmm/src/snapshot.rs` (`mod tests`)
- **Test name:** `test_size_mismatch`
- **Verifies:** A header whose region sizes differ from the actual memory returns `MemorySizeMismatch`.

### test-coverage.AC1.7 -- Truncated vmstate file

- **Criterion:** Truncated vmstate file produces `SnapshotError::Deserialize`.
- **Test type:** Unit
- **File:** `src/vmm/src/snapshot.rs` (`mod tests`)
- **Test name:** `test_truncated_file`
- **Verifies:** `load_vmstate` on a file with only a few bytes returns `Deserialize(_)`.

### test-coverage.AC1.8 -- nested_enabled mismatch

- **Criterion:** `nested_enabled` differs between snapshot and current VM produces an error.
- **Test type:** Unit
- **File:** `src/vmm/src/snapshot.rs` (`mod tests`)
- **Test name:** `test_nested_enabled_mismatch`
- **Verifies:** `validate_header_for_vm` with header `nested_enabled=true` and expected `false` returns `NestedEnabledMismatch`. Requires the Phase 1 bug fix (Task 3) adding this check.

### test-coverage.AC1.9 -- Incremental snapshot without dirty tracking

- **Criterion:** `create_incremental_snapshot` called without prior `enable_dirty_tracking` produces an error.
- **Test type:** Unit
- **File:** `src/vmm/src/lib.rs` (`mod tests`)
- **Test name:** `test_incremental_snapshot_requires_dirty_tracking`
- **Verifies:** The `check_dirty_tracking_enabled(false)` helper returns `Err(SnapshotError::DirtyTrackingNotEnabled)`. Uses a small extracted helper function since constructing a full `Vmm` without KVM may not be feasible. Requires the Phase 1 bug fix (Task 4) adding the `dirty_tracking_enabled` guard.

---

## Phase 1: Dirty Bitmap Edge Cases (AC2)

**Platform note:** `dirty_bitmap.rs` is gated on `#[cfg(target_os = "macos")]`. These tests compile and run on macOS only. On Linux, dirty tracking uses KVM ioctls and the module does not exist. This is by design.

### test-coverage.AC2.1 -- Last valid page tracked

- **Criterion:** Page at exactly `num_pages - 1` is tracked and returned by `drain_dirty_pages`.
- **Test type:** Unit (macOS only)
- **File:** `src/vmm/src/dirty_bitmap.rs` (`mod tests`)
- **Test name:** `test_last_valid_page`
- **Verifies:** `mark_dirty(base_addr + 3 * PAGE_SIZE)` on a 4-page bitmap appears in drain output.

### test-coverage.AC2.2 -- Out-of-bounds silently ignored

- **Criterion:** Page at `num_pages` (out-of-bounds) is silently ignored; no panic.
- **Test type:** Unit (macOS only)
- **File:** `src/vmm/src/dirty_bitmap.rs` (`mod tests`)
- **Test name:** `test_out_of_bounds_ignored`
- **Verifies:** `mark_dirty(base_addr + 4 * PAGE_SIZE)` does not panic, and `drain_dirty_pages` returns empty. Requires the Phase 1 bug fix (Task 1) replacing `debug_assert!` with a silent bounds check.

### test-coverage.AC2.3 -- All pages dirty

- **Criterion:** All pages marked dirty; `drain_dirty_pages` returns the full set.
- **Test type:** Unit (macOS only)
- **File:** `src/vmm/src/dirty_bitmap.rs` (`mod tests`)
- **Test name:** `test_all_pages_dirty`
- **Verifies:** Marking all 4 pages dirty then draining returns a vec of exactly 4 elements.

### test-coverage.AC2.4 -- No pages marked

- **Criterion:** No pages marked; `drain_dirty_pages` returns empty vec.
- **Test type:** Unit (macOS only)
- **File:** `src/vmm/src/dirty_bitmap.rs` (`mod tests`)
- **Test name:** `test_no_pages_dirty`
- **Verifies:** Draining without any `mark_dirty` calls returns an empty vec.

### test-coverage.AC2.5 -- Deduplication

- **Criterion:** Same page marked dirty twice appears exactly once in drain output.
- **Test type:** Unit (macOS only)
- **File:** `src/vmm/src/dirty_bitmap.rs` (`mod tests`)
- **Test name:** `test_duplicate_mark`
- **Verifies:** `mark_dirty(base_addr + PAGE_SIZE)` called twice then draining returns exactly 1 element.

---

## Phase 2: Net Async Worker Packet Paths (AC3)

### test-coverage.AC3.1 -- TX empty packet

- **Criterion:** TX empty packet (virtio header only, zero payload) delivers 0-byte slice to backend.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/async_worker.rs` (`mod tests`)
- **Test name:** `test_empty_tx_packet`
- **Verifies:** `read_tx_packet` with a descriptor of exactly `VIRTIO_NET_HDR_SIZE` bytes returns `Some(0)`.

### test-coverage.AC3.2 -- TX max-size packet

- **Criterion:** TX max-size packet (65535 B payload) is received intact by backend.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/async_worker.rs` (`mod tests`)
- **Test name:** `test_max_size_tx_packet`
- **Verifies:** `read_tx_packet` with `VIRTIO_NET_HDR_SIZE + 65535` bytes returns `Some(65535)` and the buffer matches the written payload pattern.

### test-coverage.AC3.3 -- TX multi-descriptor reassembly

- **Criterion:** TX packet split across multiple virtio descriptors is reassembled correctly.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/async_worker.rs` (`mod tests`)
- **Test name:** `test_multi_descriptor_tx`
- **Verifies:** Two chained descriptors (header-only first, payload second) produce `Some(100)` and the buffer matches the payload.

### test-coverage.AC3.4 -- Truncated header

- **Criterion:** TX descriptor with header smaller than `VIRTIO_NET_HDR_SIZE` returns `None`; no panic.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/async_worker.rs` (`mod tests`)
- **Test name:** `test_truncated_header_returns_none`
- **Verifies:** A descriptor with `len = VIRTIO_NET_HDR_SIZE - 1` causes `read_tx_packet` to return `None`.

### test-coverage.AC3.5 -- RX packet delivery

- **Criterion:** Backend injects RX packet via `to_guest_rx` channel; packet appears in guest RX virtio queue.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/async_worker.rs` (`mod tests`)
- **Test name:** `test_rx_packet_delivered`
- **Verifies:** Pre-populating an RX buffer, sending a packet via `rx_sender`, and checking the used ring shows the packet was delivered.

### test-coverage.AC3.6 -- RX drop with no guest buffers

- **Criterion:** RX packet arrives with no guest RX buffers available; packet dropped without panic.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/async_worker.rs` (`mod tests`)
- **Test name:** `test_rx_drop_no_buffers`
- **Verifies:** Sending a packet with an empty RX queue does not panic; the worker continues running.

### test-coverage.AC3.7 -- Snapshot state survives quiesce/resync

- **Criterion:** Backend `save_snapshot_state` returning `Some(data)` survives a quiesce then resync cycle.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/async_worker.rs` (`mod tests`)
- **Test name:** `test_snapshot_state_survives_quiesce_resync`
- **Verifies:** `TrackingNetBackend` with `snapshot_data = Some(b"state-bytes")` persists through quiesce and is delivered to `restore_snapshot_state` on resync.

### test-coverage.AC3.8 -- wake_rx triggers poll

- **Criterion:** `NetBackendHandle` with `wake_rx = Some(_)` causes `poll()` to be called when wake signal fires.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/async_worker.rs` (`mod tests`)
- **Test name:** `test_wake_rx_triggers_poll`
- **Verifies:** Sending a wake signal and waiting 100ms results in `poll_count >= 1`.

### test-coverage.AC3.9 -- poll_delay timer fires

- **Criterion:** Backend `poll_delay` returning `Some(d < 1s)` causes the poll timer to fire within `d`.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/async_worker.rs` (`mod tests`)
- **Test name:** `test_poll_delay_timer`
- **Verifies:** With `poll_delay_value = Some(Duration::from_millis(50))`, waiting 200ms results in `poll_count >= 1`.

---

## Phase 3: Net Proxy Internals (AC4)

### test-coverage.AC4.1 -- Header stripping

- **Criterion:** `VirtualDevice::receive_raw_from_guest` strips the virtio-net header and delivers the raw Ethernet payload.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/proxy.rs` (`mod tests`)
- **Test name:** `test_receive_raw_strips_header`
- **Verifies:** A descriptor with `VIRTIO_NET_HDR_SIZE + 5` bytes returns `Some(payload)` where the payload is the 5 data bytes after the header.

### test-coverage.AC4.2 -- Zero-length payload after header

- **Criterion:** Packet with zero-length payload after header causes no panic.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/proxy.rs` (`mod tests`)
- **Test name:** `test_receive_raw_header_only_no_panic`
- **Verifies:** A descriptor with exactly `VIRTIO_NET_HDR_SIZE` bytes returns `None` (no panic). The current code treats `read_count == header_len` as no packet; the key invariant is absence of panic.

### test-coverage.AC4.3 -- TCP SYN interception

- **Criterion:** TCP SYN packet causes `intercept_new_session` to create a host-side `TcpStream` and a smoltcp twin socket.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/proxy.rs` (`mod tests`)
- **Test name:** `test_tcp_syn_interception`
- **Verifies:** Crafting a raw Ethernet frame with TCP SYN flags targeting a localhost listener, calling `intercept_new_session`, and asserting it returns `true` with socket count increased by 1.

### test-coverage.AC4.4 -- First UDP datagram creates NAT entry

- **Criterion:** First UDP datagram to a new endpoint creates a NAT entry in `nat_table`.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/proxy.rs` (`mod tests`)
- **Test name:** `test_udp_nat_entry_created`
- **Verifies:** `handle_udp_datagram` on a fresh `ProxyNetWorker` increases `nat_table.len()` from 0 to 1.

### test-coverage.AC4.5 -- Second UDP datagram reuses NAT entry

- **Criterion:** Second UDP datagram to the same endpoint uses the existing NAT entry without creating a duplicate.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/proxy.rs` (`mod tests`)
- **Test name:** `test_udp_nat_entry_reused`
- **Verifies:** After the first datagram (AC4.4 state), a second `handle_udp_datagram` to the same endpoint keeps `nat_table.len()` at 1.

### test-coverage.AC4.6 -- Ephemeral port exhaustion

- **Criterion:** All ephemeral ports exhausted causes port allocation to return an error.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/net/proxy.rs` (`mod tests`)
- **Test name:** `test_ephemeral_port_exhaustion`
- **Verifies:** After the Phase 3 bug fix (Task 1) replacing the infinite loop with a bounded loop, `get_ephemeral_port` returns `Err(ProxyError::EphemeralPortsExhausted)` when all ports are in use. The implementation may use a narrow test range or an extracted helper to make exhaustion practical to test.

---

## Phase 4: Console TX Processing (AC5)

### test-coverage.AC5.1 -- Data forwarded to port output

- **Criterion:** TX data written to an open port is forwarded to the port's output sink correctly.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/console/process_tx.rs` (`mod tests`)
- **Test name:** `test_tx_data_forwarded_to_output`
- **Verifies:** `process_tx` with a `RecordingPortOutput` mock receives the exact payload bytes written to the descriptor.

### test-coverage.AC5.2 -- Closed port error without panic

- **Criterion:** TX to a closed/absent port returns an error; no panic.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/console/process_tx.rs` (`mod tests`)
- **Test name:** `test_tx_closed_port_no_panic`
- **Verifies:** `process_tx` with a `FailingPortOutput` mock (returns `BrokenPipe`) completes without panicking. The error is logged internally by `process_tx` (line 73); the test asserts that the worker thread joins successfully.

### test-coverage.AC5.3 -- Empty TX buffer

- **Criterion:** Empty TX buffer (zero bytes) handled without panic or error.
- **Test type:** Unit
- **File:** `src/devices/src/virtio/console/process_tx.rs` (`mod tests`)
- **Test name:** `test_tx_empty_buffer_no_panic`
- **Verifies:** A zero-length descriptor causes no panic. The `RecordingPortOutput` receives no bytes.

---

## Phase 5: Integration Test Infrastructure

Phase 5 has no acceptance criteria of its own. It wires the `libkrun` Rust API into the test workspace by adding `libkrun` as a dependency in `tests/test_cases/Cargo.toml` and creating the `krun_rust.rs` helper module. Verification is that `cargo build --features host -p test_cases` succeeds.

---

## Phase 6: Snapshot/Restore Integration (AC6)

### test-coverage.AC6.1 -- Full snapshot/restore cycle

- **Criterion:** VM starts, guest signals "READY" via vsock, host creates full snapshot, VM restores, guest verifies a pre-snapshot counter value is preserved.
- **Test type:** Integration (e2e, KVM required)
- **File:** `tests/test_cases/src/test_snapshot_restore.rs`
- **Test struct:** `TestSnapshotRestore`
- **Runner name:** `snapshot-restore-full`
- **Verifies:** Guest sets a counter to 42, signals READY, host takes a full snapshot, hot-restores, signals CHECK, guest confirms counter is still 42 and prints "OK".

### test-coverage.AC6.2 -- Incremental snapshot cycle

- **Criterion:** Full snapshot taken, dirty tracking enabled, guest modifies memory, incremental snapshot taken, restore from incremental, guest verifies written region preserved.
- **Test type:** Integration (e2e, KVM required)
- **File:** `tests/test_cases/src/test_snapshot_restore.rs`
- **Test struct:** `TestSnapshotRestoreIncremental`
- **Runner name:** `snapshot-restore-incremental`
- **Verifies:** Guest writes `0xAB` pattern to a static memory region after dirty tracking is enabled, host takes an incremental snapshot, restores it, and the guest confirms the pattern is intact.

### test-coverage.AC6.3 -- Restore with wrong magic

- **Criterion:** Restore from file with wrong magic returns `InvalidMagic` to caller.
- **Test type:** Integration (host-only, KVM required for Builder::build())
- **File:** `tests/test_cases/src/test_snapshot_errors.rs`
- **Test struct:** `TestSnapshotWrongMagic`
- **Runner name:** `snapshot-error-wrong-magic`
- **Verifies:** A manually constructed vmstate file with `magic = 0xDEADBEEF` causes `context.restore_and_run()` to return an error containing "InvalidMagic".

### test-coverage.AC6.4 -- Restore with vCPU count mismatch

- **Criterion:** Restore with vCPU count mismatch returns `VcpuCountMismatch` to caller.
- **Test type:** Integration (host-only, KVM required for Builder::build())
- **File:** `tests/test_cases/src/test_snapshot_errors.rs`
- **Test struct:** `TestSnapshotVcpuMismatch`
- **Runner name:** `snapshot-error-vcpu-mismatch`
- **Verifies:** A vmstate file with `vcpu_count = 4` against a 1-vCPU VM causes an error containing "VcpuCount".

### test-coverage.AC6.5 -- Restore with nested_enabled mismatch

- **Criterion:** Restore with `nested_enabled` mismatch returns an error.
- **Test type:** Integration (host-only, KVM required for Builder::build())
- **File:** `tests/test_cases/src/test_snapshot_errors.rs`
- **Test struct:** `TestSnapshotNestedMismatch`
- **Runner name:** `snapshot-error-nested-mismatch`
- **Verifies:** A vmstate file with `nested_enabled = true` against a VM with `nested_enabled = false` causes an error containing "NestedEnabled". Depends on Phase 1 bug fix.

---

## Phase 7: Rust API and Custom Block Backend (AC7)

### test-coverage.AC7.1 -- Builder with 0 vCPUs

- **Criterion:** `Builder` configured with 0 vCPUs produces a `BuildError` before VM starts.
- **Test type:** Integration (host-only, no guest VM launched)
- **File:** `tests/test_cases/src/test_rust_api.rs`
- **Test struct:** `TestRustApiZeroVcpu`
- **Runner name:** `rust-api-zero-vcpu`
- **Verifies:** `builder.vm_config(0, 256)` returns `Err(StartError::ZeroVcpus)`. Requires the Phase 7 bug fix (Task 1) changing `vm_config()` to return `Result`.

### test-coverage.AC7.2 -- device_info() reflects config

- **Criterion:** `Builder::build()` then `Context::device_info()` reflects the configured vCPU count and RAM size.
- **Test type:** Integration (host-only, KVM required for Builder::build())
- **File:** `tests/test_cases/src/test_rust_api.rs`
- **Test struct:** `TestRustApiDeviceInfo`
- **Runner name:** `rust-api-device-info`
- **Verifies:** After `builder.vm_config(2, 512)` and `build()`, `device_info().vcpu_count == 2` and `device_info().ram_mib` is within 10% of 512. Requires the Phase 7 fix (Task 2) adding `vcpu_count` and `ram_mib` to `VmDeviceInfo`.

### test-coverage.AC7.3 -- Pause/resume cycle

- **Criterion:** `VmHandle::pause()` followed by `VmHandle::resume()` allows the guest to continue execution and print "OK".
- **Test type:** Integration (e2e, KVM required)
- **File:** `tests/test_cases/src/test_rust_api.rs`
- **Test struct:** `TestRustApiPauseResume`
- **Runner name:** `rust-api-pause-resume`
- **Verifies:** Host pauses VM, waits 100ms, resumes, signals guest via vsock; guest continues and prints "OK".

### test-coverage.AC7.4 -- trigger_shutdown_event

- **Criterion:** `VmHandle::trigger_shutdown_event()` causes the VM to exit cleanly (zero exit code).
- **Test type:** Integration (e2e, KVM required)
- **File:** `tests/test_cases/src/test_rust_api.rs`
- **Test struct:** `TestRustApiShutdown`
- **Runner name:** `rust-api-shutdown`
- **Verifies:** On aarch64/macOS: `trigger_shutdown_event()` succeeds and the VM exits. On Linux x86_64: the function returns a meaningful `Err` (unsupported) without panicking. Platform-conditional assertions via `#[cfg]`.

### test-coverage.AC7.5 -- Guest reads from custom block backend

- **Criterion:** VM started with an in-memory `AsyncBlockBackend` pre-filled with test data; guest reads and verifies the data.
- **Test type:** Integration (e2e, KVM required)
- **File:** `tests/test_cases/src/test_custom_block_backend.rs`
- **Test struct:** `TestCustomBlockBackend`
- **Runner name:** `custom-block-backend`
- **Verifies:** Guest reads sector 0 of `/dev/vda` and asserts all bytes are `0x5A` (the fill byte). Uses `MemBlockBackend` from `tests/test_cases/src/mem_block_backend.rs`.

### test-coverage.AC7.6 -- Host verifies guest writes

- **Criterion:** Guest writes data to the custom block backend; host verifies the backend received the written bytes after VM exits.
- **Test type:** Integration (e2e, KVM required)
- **File:** `tests/test_cases/src/test_custom_block_backend.rs`
- **Test struct:** `TestCustomBlockBackend` (same test as AC7.5)
- **Runner name:** `custom-block-backend`
- **Verifies:** After the guest writes `0xAB` to sector 1 and exits, the host reads the shared `Arc<Mutex<Vec<u8>>>` and asserts bytes 512..1024 are all `0xAB`.

---

## Phase 8: Net Proxy Integration (AC8)

### test-coverage.AC8.1 -- TCP PING/PONG through smoltcp proxy

- **Criterion:** Guest makes a TCP connection to a host listener through the smoltcp `ProxyNetWorker` backend, exchanges a "PING"/"PONG" message pair successfully.
- **Test type:** Integration (e2e, KVM required)
- **File:** `tests/test_cases/src/test_net_proxy.rs`
- **Test struct:** `TestNetProxy`
- **Runner name:** `net-proxy-ping-pong`
- **Verifies:** Host binds a TCP listener on an ephemeral port, starts VM with `VirtioNetBackend::Proxy`, guest connects to `127.0.0.1:<port>` (routed through smoltcp proxy), sends "PING", receives "PONG", and prints "OK". Requires Phase 8 Task 1 adding the `VirtioNetBackend::Proxy` variant.

---

## Human Verification

All acceptance criteria are covered by automated tests. No criteria require manual human verification. The following notes apply:

- **AC2.1--AC2.5 (dirty bitmap):** These tests are macOS-only because the `dirty_bitmap.rs` module is gated on `#[cfg(target_os = "macos")]`. On Linux, dirty tracking uses KVM ioctls directly and there is no `DirtyBitmap` struct to test. This is not a gap -- it reflects the platform-specific architecture. The Linux dirty tracking path is exercised indirectly by the integration tests in Phase 6 (AC6.1, AC6.2) which require KVM.

- **AC7.4 (trigger_shutdown_event):** On Linux x86_64, the shutdown event fd is not available, so the test verifies the function returns a meaningful error rather than panicking. On aarch64/macOS, the full shutdown path is tested. This platform-conditional behavior is documented in the test itself.

- **AC7.5 and AC7.6:** Both are covered by the same `TestCustomBlockBackend` integration test. AC7.5 is verified by guest-side assertions (read sector 0); AC7.6 is verified by host-side assertions after VM exit (read sector 1 from shared backend state).

---

## Summary Table

| AC ID | Test Type | File | Test Name / Runner Name |
|---|---|---|---|
| test-coverage.AC1.1 | Unit | `src/vmm/src/snapshot.rs` | `test_header_roundtrip` |
| test-coverage.AC1.2 | Unit | `src/vmm/src/snapshot.rs` | `test_invalid_magic` |
| test-coverage.AC1.3 | Unit | `src/vmm/src/snapshot.rs` | `test_invalid_version` |
| test-coverage.AC1.4 | Unit | `src/vmm/src/snapshot.rs` | `test_vcpu_count_mismatch` |
| test-coverage.AC1.5 | Unit | `src/vmm/src/snapshot.rs` | `test_layout_mismatch` |
| test-coverage.AC1.6 | Unit | `src/vmm/src/snapshot.rs` | `test_size_mismatch` |
| test-coverage.AC1.7 | Unit | `src/vmm/src/snapshot.rs` | `test_truncated_file` |
| test-coverage.AC1.8 | Unit | `src/vmm/src/snapshot.rs` | `test_nested_enabled_mismatch` |
| test-coverage.AC1.9 | Unit | `src/vmm/src/lib.rs` | `test_incremental_snapshot_requires_dirty_tracking` |
| test-coverage.AC2.1 | Unit (macOS) | `src/vmm/src/dirty_bitmap.rs` | `test_last_valid_page` |
| test-coverage.AC2.2 | Unit (macOS) | `src/vmm/src/dirty_bitmap.rs` | `test_out_of_bounds_ignored` |
| test-coverage.AC2.3 | Unit (macOS) | `src/vmm/src/dirty_bitmap.rs` | `test_all_pages_dirty` |
| test-coverage.AC2.4 | Unit (macOS) | `src/vmm/src/dirty_bitmap.rs` | `test_no_pages_dirty` |
| test-coverage.AC2.5 | Unit (macOS) | `src/vmm/src/dirty_bitmap.rs` | `test_duplicate_mark` |
| test-coverage.AC3.1 | Unit | `src/devices/src/virtio/net/async_worker.rs` | `test_empty_tx_packet` |
| test-coverage.AC3.2 | Unit | `src/devices/src/virtio/net/async_worker.rs` | `test_max_size_tx_packet` |
| test-coverage.AC3.3 | Unit | `src/devices/src/virtio/net/async_worker.rs` | `test_multi_descriptor_tx` |
| test-coverage.AC3.4 | Unit | `src/devices/src/virtio/net/async_worker.rs` | `test_truncated_header_returns_none` |
| test-coverage.AC3.5 | Unit | `src/devices/src/virtio/net/async_worker.rs` | `test_rx_packet_delivered` |
| test-coverage.AC3.6 | Unit | `src/devices/src/virtio/net/async_worker.rs` | `test_rx_drop_no_buffers` |
| test-coverage.AC3.7 | Unit | `src/devices/src/virtio/net/async_worker.rs` | `test_snapshot_state_survives_quiesce_resync` |
| test-coverage.AC3.8 | Unit | `src/devices/src/virtio/net/async_worker.rs` | `test_wake_rx_triggers_poll` |
| test-coverage.AC3.9 | Unit | `src/devices/src/virtio/net/async_worker.rs` | `test_poll_delay_timer` |
| test-coverage.AC4.1 | Unit | `src/devices/src/virtio/net/proxy.rs` | `test_receive_raw_strips_header` |
| test-coverage.AC4.2 | Unit | `src/devices/src/virtio/net/proxy.rs` | `test_receive_raw_header_only_no_panic` |
| test-coverage.AC4.3 | Unit | `src/devices/src/virtio/net/proxy.rs` | `test_tcp_syn_interception` |
| test-coverage.AC4.4 | Unit | `src/devices/src/virtio/net/proxy.rs` | `test_udp_nat_entry_created` |
| test-coverage.AC4.5 | Unit | `src/devices/src/virtio/net/proxy.rs` | `test_udp_nat_entry_reused` |
| test-coverage.AC4.6 | Unit | `src/devices/src/virtio/net/proxy.rs` | `test_ephemeral_port_exhaustion` |
| test-coverage.AC5.1 | Unit | `src/devices/src/virtio/console/process_tx.rs` | `test_tx_data_forwarded_to_output` |
| test-coverage.AC5.2 | Unit | `src/devices/src/virtio/console/process_tx.rs` | `test_tx_closed_port_no_panic` |
| test-coverage.AC5.3 | Unit | `src/devices/src/virtio/console/process_tx.rs` | `test_tx_empty_buffer_no_panic` |
| test-coverage.AC6.1 | Integration | `tests/test_cases/src/test_snapshot_restore.rs` | `snapshot-restore-full` |
| test-coverage.AC6.2 | Integration | `tests/test_cases/src/test_snapshot_restore.rs` | `snapshot-restore-incremental` |
| test-coverage.AC6.3 | Integration | `tests/test_cases/src/test_snapshot_errors.rs` | `snapshot-error-wrong-magic` |
| test-coverage.AC6.4 | Integration | `tests/test_cases/src/test_snapshot_errors.rs` | `snapshot-error-vcpu-mismatch` |
| test-coverage.AC6.5 | Integration | `tests/test_cases/src/test_snapshot_errors.rs` | `snapshot-error-nested-mismatch` |
| test-coverage.AC7.1 | Integration | `tests/test_cases/src/test_rust_api.rs` | `rust-api-zero-vcpu` |
| test-coverage.AC7.2 | Integration | `tests/test_cases/src/test_rust_api.rs` | `rust-api-device-info` |
| test-coverage.AC7.3 | Integration | `tests/test_cases/src/test_rust_api.rs` | `rust-api-pause-resume` |
| test-coverage.AC7.4 | Integration | `tests/test_cases/src/test_rust_api.rs` | `rust-api-shutdown` |
| test-coverage.AC7.5 | Integration | `tests/test_cases/src/test_custom_block_backend.rs` | `custom-block-backend` |
| test-coverage.AC7.6 | Integration | `tests/test_cases/src/test_custom_block_backend.rs` | `custom-block-backend` |
| test-coverage.AC8.1 | Integration | `tests/test_cases/src/test_net_proxy.rs` | `net-proxy-ping-pong` |
