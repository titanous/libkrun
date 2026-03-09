# Safe Snapshot Serialization — Human Test Plan

## Prerequisites
- NixOS dev shell active (all dependencies available)
- `just test` passing (all unit tests green)
- `just check` passing (format + clippy clean)
- `/dev/kvm` available for integration tests
- libkrunfw available at `test-prefix/lib64/`

## Phase 1: snapshot_serde Module Verification

| Step | Action | Expected |
|------|--------|----------|
| 1.1 | Run `cargo test --features snapshot -p devices test_roundtrip_serialize_deserialize` | Test passes; valid TestState round-trips through serialize/deserialize |
| 1.2 | Run `cargo test --features snapshot -p devices test_byte_limit_rejection` | Test passes; 1-byte limit rejects valid payload with "exceeds limit" message |
| 1.3 | Run `cargo test --features snapshot -p devices test_crafted_payload_rejection` | Test passes; crafted 4GB varint rejected by `with_limit()` (not upfront check) |

## Phase 2: Legacy Device Snapshot Verification

| Step | Action | Expected |
|------|--------|----------|
| 2.1 | Run `cargo test --features snapshot -p devices snapshot_tests` | All legacy device snapshot tests pass (serial, i8042, CMOS, RTC) |
| 2.2 | Run `cargo test --features snapshot -p devices test_serial_snapshot_rejects_oversized_buffer` | Passes; buffer of 65 bytes (LOOP_SIZE+1) rejected with "in_buffer length ... exceeds LOOP_SIZE" |
| 2.3 | Run `cargo test --features snapshot -p devices test_serial_snapshot_rejects_huge_length_prefix` | Passes; crafted varint rejected by `with_limit()` during deserialization |
| 2.4 | Run `cargo test --features snapshot -p devices test_i8042_snapshot_invalid_buf_length` | Passes; 2-byte buf rejected with "buf length mismatch" |

## Phase 3: Virtio Device Snapshot Verification

| Step | Action | Expected |
|------|--------|----------|
| 3.1 | Run `cargo test --features "snapshot,vhost-user" -p devices test_snapshot_state_roundtrip` | VhostUser vsock and FS states round-trip correctly |
| 3.2 | Run `cargo test --features "snapshot,vhost-user" -p devices test_restore_backend_state_stores_pending` | VhostUser vsock and FS pending restore states stored correctly |

## Phase 4: VMM Snapshot Verification

| Step | Action | Expected |
|------|--------|----------|
| 4.1 | Run `cargo test --features snapshot -p vmm test_header_roundtrip` | SnapshotHeader round-trips via bincode-next |
| 4.2 | Run `cargo test --features snapshot -p vmm test_snapshot_no_balloon_no_regression` | VmSnapshot with empty excluded_pages round-trips correctly |
| 4.3 | Run `cargo test --features snapshot -p vmm` (all VMM tests) | All snapshot_store tests and snapshot tests pass |

## Phase 5: Full Build and Dependency Verification

| Step | Action | Expected |
|------|--------|----------|
| 5.1 | Run `just check` | Format and clippy pass with no errors |
| 5.2 | Run `just test` | All unit tests across all crates pass |
| 5.3 | Inspect `Cargo.lock`: search for `name = "bincode"` entries | Only bincode 2.0.1 present (transitive dep); no bincode 1.x |
| 5.4 | Inspect `fuzz/Cargo.lock`: search for `name = "bincode"` entries | Only bincode 2.0.1 present; no bincode 1.x |
| 5.5 | Run `cd fuzz && cargo check` (requires nightly) | Fuzz workspace compiles with bincode-next |

## End-to-End: Full Snapshot Save/Restore Cycle

| Step | Action | Expected |
|------|--------|----------|
| E2E.1 | Run `just integration snapshot` (if snapshot integration test exists) | Full VM snapshot and restore succeeds; VM resumes correctly |
| E2E.2 | Run `just integration` (all integration tests) | All integration tests pass |

## End-to-End: Fuzz Target Smoke Test

| Step | Action | Expected |
|------|--------|----------|
| F.1 | Run `just fuzz fuzz_snapshot_deser 10` (10 seconds) | Fuzz target runs without crashes |

## Human Verification Required

| Criterion | Why Manual | Steps |
|-----------|-----------|-------|
| GICv3 migration (gicv3.rs, kvmgicv3.rs) | aarch64-only | Inspect source for `snapshot_serde` calls and Encode/Decode derives. Run `cargo check --features snapshot -p devices`. |
| GPIO migration (aarch64) | aarch64-only, no tests | Inspect source. Run `cargo check --features snapshot -p devices`. |
| PL011 read_fifo validation | aarch64-only tests | Inspect `serial.rs` for `PL011_FIFO_SIZE` guard before field assignment. |
| No bincode 1.x source references | Full-codebase grep | Run `grep -r 'bincode::' src/ --include='*.rs'`. Expected: zero matches. |
| test_vsock_proxy migrated | Separate test workspace | Run `cd tests/test_vsock_proxy && cargo check`. Inspect for Encode/Decode. |
| serde removed from devices snapshot feature | Cargo.toml inspection | Verify `src/devices/Cargo.toml` snapshot feature is `["bincode-next"]`. |
| Fuzz target omits with_limit() | Intentional design | Verify `fuzz_snapshot_deser.rs` uses `decode_from_slice` without `with_limit()`. |

## Traceability

| Acceptance Criterion | Automated Test | Manual Step |
|----------------------|----------------|-------------|
| AC2.1 — roundtrip under limit | `test_roundtrip_serialize_deserialize` | 1.1 |
| AC2.2 — payload exceeding limit | `test_byte_limit_rejection` | 1.2 |
| AC2.3 — crafted huge Vec prefix | `test_crafted_payload_rejection` | 1.3 |
| AC1.1 — Serial 16550 (5 tests) | `serial_16550::snapshot_tests` | 2.1-2.3 |
| AC1.1 — i8042 (4 tests) | `i8042::snapshot_tests` | 2.1, 2.4 |
| AC1.1 — CMOS | `cmos::tests` | 2.1 |
| AC1.1 — PL011 (4 tests) | `serial::snapshot_tests` | aarch64 only |
| AC1.1 — RTC PL031 | `rtc_pl031::tests` | 2.1 |
| AC3.1 — Serial buffer validation | `test_serial_snapshot_rejects_oversized_buffer` | 2.2 |
| AC3.1 — i8042 buf validation | `test_i8042_snapshot_invalid_buf_length` | 2.4 |
| AC3.2 — PL011 fifo validation | `test_pl011_snapshot_fifo_overflow` | Code review |
| AC1.3/AC1.4 — VhostUser devices | `test_snapshot_state_roundtrip`, `test_restore_backend_state_stores_pending` | 3.1-3.2 |
| AC1.5 — VMM-level snapshot | `test_header_roundtrip`, `prop_vm_snapshot_bincode_roundtrip` | 4.1-4.3 |
| AC4.1 — All state structs | Compilation + roundtrip tests | All phases |
| AC5.1 — No bincode 1.x | Cargo.lock inspection | 5.3-5.4 |
| AC5.2 — Fuzz bincode-next | `fuzz_snapshot_deser.rs` | 5.5, F.1 |
