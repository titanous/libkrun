# Test Plan: libkrun Test Coverage (2026-02-23)

## Coverage Summary

**Automated Criteria:** 38 | **Covered:** 38 | **Missing:** 0 | **Result: PASS**

## Prerequisites

- Linux host with KVM support (`/dev/kvm` accessible)
- Inside the nix develop shell (or equivalent environment with all dependencies)
- `libkrunfw` symlinked to `test-prefix/lib64` (done by shellHook)
- All unit tests passing: `cargo test -p vmm --features snapshot` and `cargo test -p devices --features net`
- All integration tests passing: `make test FEATURE_FLAGS="--features embedded_init"`

---

## Phase 1: Snapshot Header Validation

| Step | Action | Expected |
|------|--------|----------|
| 1.1 | `cargo test -p vmm --features snapshot -- snapshot::tests::test_header_roundtrip` | Passes. Serialization/deserialization produces identical header fields. |
| 1.2 | `cargo test -p vmm --features snapshot -- snapshot::tests::test_invalid_magic` | Passes. `0xDEADBEEF` magic triggers `InvalidMagic`. |
| 1.3 | `cargo test -p vmm --features snapshot -- snapshot::tests::test_invalid_version` | Passes. Version 99 triggers `InvalidVersion(99)`. |
| 1.4 | `cargo test -p vmm --features snapshot -- snapshot::tests::test_vcpu_count_mismatch` | Passes. Header vcpu_count=2 vs expected=4 triggers `VcpuCountMismatch`. |
| 1.5 | `cargo test -p vmm --features snapshot -- snapshot::tests::test_layout_mismatch` | Passes. Corrupted `ram_regions` triggers `MemoryLayoutMismatch`. |
| 1.6 | `cargo test -p vmm --features snapshot -- snapshot::tests::test_memory_file_size_mismatch` | Passes. File with 0x1000 bytes vs expected 0x2000 triggers `MemorySizeMismatch`. |
| 1.7 | `cargo test -p vmm --features snapshot -- snapshot::tests::test_truncated_vmstate_file` | Passes. 4-byte file triggers `Deserialize` error. |
| 1.8 | `cargo test -p vmm --features snapshot -- snapshot::tests::test_nested_enabled_mismatch` | Passes. Header nested=true vs expected=false triggers `NestedEnabledMismatch`. |
| 1.9 | `cargo test -p vmm -- tests::test_incremental_snapshot_requires_dirty_tracking` | Passes. `dirty_tracking_enabled=false` triggers `DirtyTrackingNotEnabled`. |

---

## Phase 2: Dirty Bitmap (macOS only)

| Step | Action | Expected |
|------|--------|----------|
| 2.1 | **macOS only:** `cargo test -p vmm -- dirty_bitmap::tests` | All 5 tests pass: `test_last_valid_page`, `test_out_of_bounds_ignored`, `test_all_pages_dirty`, `test_no_pages_dirty`, `test_duplicate_mark`. |
| 2.2 | **Linux:** confirm `dirty_bitmap.rs` is gated `#[cfg(target_os = "macos")]` | Tests don't exist on Linux (expected). Dirty tracking on Linux is verified by integration tests in Phase 6. |

---

## Phase 3: Net Async Worker TX/RX

| Step | Action | Expected |
|------|--------|----------|
| 3.1 | `cargo test -p devices -- virtio::net::async_worker::tests::test_header_only_tx_returns_none` | Passes. Header-only packet returns `None`. |
| 3.2 | `cargo test -p devices -- virtio::net::async_worker::tests::test_max_size_tx_packet` | Passes. 65535-byte payload returned intact. |
| 3.3 | `cargo test -p devices -- virtio::net::async_worker::tests::test_multi_descriptor_tx` | Passes. Chained descriptors reassembled to 100-byte payload. |
| 3.4 | `cargo test -p devices -- virtio::net::async_worker::tests::test_truncated_header_returns_none` | Passes. Short descriptor returns `None`, no panic. |
| 3.5 | `cargo test -p devices -- virtio::net::async_worker::tests::test_rx_packet_delivered` | Passes. "hello-world" delivered to RX queue with correct header + payload. |
| 3.6 | `cargo test -p devices -- virtio::net::async_worker::tests::test_rx_drop_no_buffers` | Passes. Packet dropped silently, worker thread joins without panic. |
| 3.7 | `cargo test -p devices -- virtio::net::async_worker::tests::test_snapshot_state_survives_quiesce_resync` | Passes. `b"state-bytes"` persists through quiesce/resume/resync cycle. |
| 3.8 | `cargo test -p devices -- virtio::net::async_worker::tests::test_wake_rx_triggers_poll` | Passes. `poll_count` increments after wake signal. |
| 3.9 | `cargo test -p devices -- virtio::net::async_worker::tests::test_poll_delay_timer` | Passes. `poll_count` increments via 50ms timer. |

---

## Phase 4: Net Proxy Internals

| Step | Action | Expected |
|------|--------|----------|
| 4.1 | `cargo test -p devices --features net -- virtio::net::proxy::tests::test_receive_raw_strips_header` | Passes. 5-byte payload extracted after header stripping. |
| 4.2 | `cargo test -p devices --features net -- virtio::net::proxy::tests::test_receive_raw_header_only_no_panic` | Passes. Header-only returns `None`, no panic. |
| 4.3 | `cargo test -p devices --features net -- virtio::net::proxy::tests::test_tcp_syn_interception` | Passes. SYN packet intercepted, smoltcp socket count increased by 1. |
| 4.4 | `cargo test -p devices --features net -- virtio::net::proxy::tests::test_udp_nat_entry_created` | Passes. `nat_table.len()` goes from 0 to 1. |
| 4.5 | `cargo test -p devices --features net -- virtio::net::proxy::tests::test_udp_nat_entry_reused` | Passes. Second datagram to same endpoint keeps `nat_table.len()` at 1. |
| 4.6 | `cargo test -p devices --features net -- virtio::net::proxy::tests::test_ephemeral_port_exhaustion` | Passes. First call returns `Ok` with port >= 49152. |

**Note on AC4.6:** The test verifies the bounded loop and matchable error type but does not exhaust all 16384 ports (impractical in CI). Manually verify `get_ephemeral_port()` at `src/devices/src/virtio/net/proxy.rs` contains a `for _ in 0..total_ports` loop (not an infinite loop).

---

## Phase 5: Console TX Processing

| Step | Action | Expected |
|------|--------|----------|
| 5.1 | `cargo test -p devices -- virtio::console::process_tx::tests::test_tx_data_forwarded_to_output` | Passes. "hello console" received by `RecordingPortOutput`. |
| 5.2 | `cargo test -p devices -- virtio::console::process_tx::tests::test_tx_closed_port_no_panic` | Passes. `FailingPortOutput` returns error, no panic. |
| 5.3 | `cargo test -p devices -- virtio::console::process_tx::tests::test_tx_empty_buffer_no_panic` | Passes. Zero-length descriptor returns `Ok(0)`. |

---

## Phase 6: Integration — Snapshot/Restore (KVM required)

| Step | Action | Expected |
|------|--------|----------|
| 6.1 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=snapshot-restore-full` | Passes. Guest counter survives full snapshot/restore. Output includes "OK". |
| 6.2 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=snapshot-restore-incremental` | Passes. 0xAB pattern survives incremental snapshot/restore. Output includes "OK". |
| 6.3 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=snapshot-error-wrong-magic` | Passes. Hand-crafted bad-magic vmstate triggers "Invalid snapshot magic number". Output includes "OK". |
| 6.4 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=snapshot-error-vcpu-mismatch` | Passes. 4-vCPU vmstate against 1-vCPU VM triggers "vCPU count mismatch". Output includes "OK". |
| 6.5 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=snapshot-error-nested-mismatch` | Passes. `nested_enabled=true` vmstate against false VM triggers "Nested virtualization enabled mismatch". Output includes "OK". |

---

## Phase 7: Integration — Rust API and Custom Block Backend (KVM required)

| Step | Action | Expected |
|------|--------|----------|
| 7.1 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=rust-api-zero-vcpu` | Passes. `vm_config(0, 256)` returns error before VM starts. Output includes "OK". |
| 7.2 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=rust-api-device-info` | Passes. `device_info().vcpu_count == 2` and `ram_mib` within 10% of 512. Output includes "OK". |
| 7.3 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=rust-api-pause-resume` | Passes. VM pauses, resumes, guest continues and prints "OK". |
| 7.4 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=rust-api-shutdown` | Passes. On Linux x86_64: returns meaningful `Err`. On aarch64/macOS: VM exits cleanly. |
| 7.5–7.6 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=custom-block-backend` | Passes. Guest reads 0x5A from sector 0, writes 0xAB to sector 1. Host verifies sector 1 is all 0xAB after VM exit. |

**Note on AC7.4:** Full shutdown path (clean VM exit) is only testable on aarch64/macOS. On Linux x86_64, verify the error message is informative, not a raw panic.

---

## Phase 8: Integration — Net Proxy (KVM required)

| Step | Action | Expected |
|------|--------|----------|
| 8.1 | `make test FEATURE_FLAGS="--features embedded_init" TEST_FILTER=net-proxy-ping-pong` | Passes. Guest configures eth0 via raw ioctls, connects to host TCP listener through smoltcp proxy at 192.168.100.1, exchanges PING/PONG. Output includes "OK". |

---

## End-to-End: Full Integration Suite

| Step | Action | Expected |
|------|--------|----------|
| E2E.1 | `make test FEATURE_FLAGS="--features embedded_init"` | At least 5/6 test groups pass. Snapshot, Rust API, block backend, and net proxy all produce "OK". VSock/TSI tests may flake under load (pre-existing behavior). |
| E2E.2 | Run the full suite twice consecutively | Both runs produce consistent results. No persistent resource leaks (leftover sockets, temp files) between runs. |

---

## Human Verification Notes

| Area | Verification |
|------|-------------|
| AC2.1-AC2.5 (dirty bitmap, macOS) | Run on a macOS developer machine: `cargo test -p vmm -- dirty_bitmap::tests`. Verify all 5 tests pass. On Linux, confirm `dirty_bitmap.rs` is not compiled (gated on `#[cfg(target_os = "macos")]`). |
| AC4.6 (ephemeral port loop) | Code-review `get_ephemeral_port()` in `src/devices/src/virtio/net/proxy.rs`. Confirm the `for _ in 0..total_ports` bounded loop (not infinite). `total_ports = 16384`. |
| AC7.4 (shutdown, aarch64/macOS) | On macOS aarch64: verify `trigger_shutdown_event()` exits VM cleanly. On Linux x86_64: verify the error message is informative. |
| Flakiness | Run `make test FEATURE_FLAGS="--features embedded_init"` 3+ times. Expect 5-6/6 consistently. Single vsock/TSI failure on one run is pre-existing flakiness, not a regression. |

---

## Traceability

| AC | Test | Phase |
|----|------|-------|
| AC1.1 | `test_header_roundtrip` (snapshot.rs) | 1.1 |
| AC1.2 | `test_invalid_magic` | 1.2 |
| AC1.3 | `test_invalid_version` | 1.3 |
| AC1.4 | `test_vcpu_count_mismatch` | 1.4 |
| AC1.5 | `test_layout_mismatch` | 1.5 |
| AC1.6 | `test_memory_file_size_mismatch` | 1.6 |
| AC1.7 | `test_truncated_vmstate_file` | 1.7 |
| AC1.8 | `test_nested_enabled_mismatch` | 1.8 |
| AC1.9 | `test_incremental_snapshot_requires_dirty_tracking` (lib.rs) | 1.9 |
| AC2.1 | `test_last_valid_page` (dirty_bitmap.rs, macOS) | 2.1 |
| AC2.2 | `test_out_of_bounds_ignored` | 2.1 |
| AC2.3 | `test_all_pages_dirty` | 2.1 |
| AC2.4 | `test_no_pages_dirty` | 2.1 |
| AC2.5 | `test_duplicate_mark` | 2.1 |
| AC3.1 | `test_header_only_tx_returns_none` (async_worker.rs) | 3.1 |
| AC3.2 | `test_max_size_tx_packet` | 3.2 |
| AC3.3 | `test_multi_descriptor_tx` | 3.3 |
| AC3.4 | `test_truncated_header_returns_none` | 3.4 |
| AC3.5 | `test_rx_packet_delivered` | 3.5 |
| AC3.6 | `test_rx_drop_no_buffers` | 3.6 |
| AC3.7 | `test_snapshot_state_survives_quiesce_resync` | 3.7 |
| AC3.8 | `test_wake_rx_triggers_poll` | 3.8 |
| AC3.9 | `test_poll_delay_timer` | 3.9 |
| AC4.1 | `test_receive_raw_strips_header` (proxy.rs) | 4.1 |
| AC4.2 | `test_receive_raw_header_only_no_panic` | 4.2 |
| AC4.3 | `test_tcp_syn_interception` | 4.3 |
| AC4.4 | `test_udp_nat_entry_created` | 4.4 |
| AC4.5 | `test_udp_nat_entry_reused` | 4.5 |
| AC4.6 | `test_ephemeral_port_exhaustion` | 4.6 |
| AC5.1 | `test_tx_data_forwarded_to_output` (process_tx.rs) | 5.1 |
| AC5.2 | `test_tx_closed_port_no_panic` | 5.2 |
| AC5.3 | `test_tx_empty_buffer_no_panic` | 5.3 |
| AC6.1 | `snapshot-restore-full` (test_snapshot_restore.rs) | 6.1 |
| AC6.2 | `snapshot-restore-incremental` | 6.2 |
| AC6.3 | `snapshot-error-wrong-magic` (test_snapshot_errors.rs) | 6.3 |
| AC6.4 | `snapshot-error-vcpu-mismatch` | 6.4 |
| AC6.5 | `snapshot-error-nested-mismatch` | 6.5 |
| AC7.1 | `rust-api-zero-vcpu` (test_rust_api.rs) | 7.1 |
| AC7.2 | `rust-api-device-info` | 7.2 |
| AC7.3 | `rust-api-pause-resume` | 7.3 |
| AC7.4 | `rust-api-shutdown` | 7.4 |
| AC7.5 | `custom-block-backend` (test_custom_block_backend.rs) | 7.5–7.6 |
| AC7.6 | `custom-block-backend` | 7.5–7.6 |
| AC8.1 | `net-proxy-ping-pong` (test_net_proxy.rs) | 8.1 |
