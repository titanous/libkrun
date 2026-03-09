# Safe Snapshot Serialization — Test Requirements

## Automated Tests

| AC ID | Criterion | Test Type | Expected Test Location | Phase |
|-------|-----------|-----------|----------------------|-------|
| AC2.1 | `snapshot_serde::deserialize` with valid data under limit succeeds | unit | `src/devices/src/snapshot_serde.rs` — `test_roundtrip_under_limit` (TestState with u32, String, Vec<u8> fields; serialize then deserialize with 1024-byte limit) | 1 |
| AC2.2 | `snapshot_serde::deserialize` with payload exceeding `max_bytes` returns `SnapshotError::Deserialize` without allocating the claimed size | unit | `src/devices/src/snapshot_serde.rs` — `test_rejects_payload_exceeding_limit` (serialize valid TestState, deserialize with 1-byte limit; assert Err and message contains "exceeds limit") | 1 |
| AC2.3 | Crafted payload with Vec length prefix claiming 1GB+ is rejected before allocation | unit | `src/devices/src/snapshot_serde.rs` — `test_rejects_crafted_huge_vec_prefix` (serialize TestState with 256-byte Vec, deserialize with 64-byte limit; `with_limit()` rejects mid-decode before allocating) | 1 |
| AC1.1 | Serial 16550 Snapshottable impl uses `snapshot_serde` | unit | `src/devices/src/legacy/serial_16550.rs` — existing `snapshot_tests` module (5 tests: register roundtrip, buffer roundtrip, corrupted state, huge length prefix rejection, oversized buffer rejection) | 2 |
| AC1.1 | i8042 Snapshottable impl uses `snapshot_serde` | unit | `src/devices/src/legacy/i8042.rs` — existing `snapshot_tests` module (4 tests: registers, buffer, corrupted state, invalid buf length) | 2 |
| AC1.1 | CMOS Snapshottable impl uses `snapshot_serde` | unit | `src/devices/src/legacy/x86_64/cmos.rs` — existing `snapshot_tests` module (`test_cmos_snapshot_preserves_index_and_data`) | 2 |
| AC1.1 | PL011 Snapshottable impl uses `snapshot_serde` | unit | `src/devices/src/legacy/aarch64/serial.rs` — new `snapshot_tests` module (round-trip test, FIFO validation test) | 2 |
| AC1.1 | RTC PL031 Snapshottable impl uses `snapshot_serde` | unit | `src/devices/src/legacy/rtc_pl031.rs` — existing `snapshot_tests` module (`test_rtc_snapshot_preserves_registers`) | 2 |
| AC1.2 | Legacy device snapshot round-trip tests pass with new backend | unit | All tests listed for AC1.1 above; run via `cargo test --features snapshot -p devices` | 2 |
| AC3.1 | Serial 16550 `in_buffer.len() <= 64` check preserved | unit | `src/devices/src/legacy/serial_16550.rs` — `test_serial_snapshot_rejects_oversized_buffer` (existing test, unchanged) | 2 |
| AC3.1 | i8042 `buf.len() == 16` check preserved | unit | `src/devices/src/legacy/i8042.rs` — `test_i8042_snapshot_invalid_buf_length` (existing test, unchanged) | 2 |
| AC3.1 | CMOS `data.len() == 128` check preserved | unit | `src/devices/src/legacy/x86_64/cmos.rs` — `test_cmos_snapshot_preserves_index_and_data` (existing test exercises the data length check) | 2 |
| AC3.2 | PL011 gains `read_fifo` length validation bounded by FIFO size | unit | `src/devices/src/legacy/aarch64/serial.rs` — new test `test_pl011_snapshot_rejects_oversized_fifo` (construct state with read_fifo exceeding PL011_FIFO_SIZE, assert restore_state returns error) | 2 |
| AC4.1 | Serial 16550 state struct uses bincode-next Encode/Decode | unit | `src/devices/src/legacy/serial_16550.rs` — existing snapshot round-trip tests implicitly verify (tests fail to compile if derives are wrong) | 2 |
| AC4.1 | i8042 state struct uses bincode-next Encode/Decode | unit | `src/devices/src/legacy/i8042.rs` — existing snapshot tests implicitly verify | 2 |
| AC4.1 | CMOS state struct uses bincode-next Encode/Decode | unit | `src/devices/src/legacy/x86_64/cmos.rs` — existing snapshot tests implicitly verify | 2 |
| AC4.1 | PL011 state struct uses bincode-next Encode/Decode | unit | `src/devices/src/legacy/aarch64/serial.rs` — new snapshot round-trip test implicitly verifies | 2 |
| AC4.1 | RTC PL031 state struct uses bincode-next Encode/Decode | unit | `src/devices/src/legacy/rtc_pl031.rs` — existing snapshot tests implicitly verify | 2 |
| AC1.3 | MMIO transport snapshot uses `snapshot_serde` | unit | `src/devices/src/virtio/mmio.rs` — compilation verifies (MMIO snapshot tested end-to-end via integration tests) | 3 |
| AC1.3 | Balloon snapshot uses `snapshot_serde` | unit | `src/devices/src/virtio/balloon/device.rs` — compilation verifies (balloon backend state tested via save/restore_backend_state) | 3 |
| AC1.3 | VhostUser Vsock snapshot uses `snapshot_serde` | unit | `src/devices/src/virtio/vhost_user/vsock.rs` — existing tests (`test_vsock_state_roundtrip`, `test_restore_backend_state_stores_pending`) migrated to `snapshot_serde` calls | 3 |
| AC1.3 | VhostUser FS snapshot uses `snapshot_serde` | unit | `src/devices/src/virtio/vhost_user/fs.rs` — existing tests (`test_fs_state_roundtrip`, `test_restore_backend_state_stores_pending`) migrated to `snapshot_serde` calls | 3 |
| AC1.4 | VhostUser vsock `save_backend_state`/`restore_backend_state` uses `snapshot_serde` | unit | `src/devices/src/virtio/vhost_user/vsock.rs` — `test_vsock_state_roundtrip` exercises both methods end-to-end | 3 |
| AC1.4 | VhostUser FS `save_backend_state`/`restore_backend_state` uses `snapshot_serde` | unit | `src/devices/src/virtio/vhost_user/fs.rs` — `test_fs_state_roundtrip` exercises both methods end-to-end | 3 |
| AC4.1 | MmioTransportState and QueueState use bincode-next Encode/Decode | unit | `src/devices/src/virtio/mmio.rs` — compilation verifies; integration tests exercise round-trip | 3 |
| AC4.1 | BalloonState uses bincode-next Encode/Decode | unit | `src/devices/src/virtio/balloon/device.rs` — compilation verifies | 3 |
| AC4.1 | VhostUserVsockState uses bincode-next Encode/Decode | unit | `src/devices/src/virtio/vhost_user/vsock.rs` — `test_vsock_state_roundtrip` verifies | 3 |
| AC4.1 | VhostUserFsState uses bincode-next Encode/Decode | unit | `src/devices/src/virtio/vhost_user/fs.rs` — `test_fs_state_roundtrip` verifies | 3 |
| AC1.5 | VmSnapshot, IncrementalSnapshot, SnapshotHeader serialize/deserialize via bincode-next | unit | `src/vmm/src/snapshot.rs` — existing tests (`test_header_roundtrip`, `prop_vm_snapshot_bincode_roundtrip`) migrated to bincode-next | 4 |
| AC1.5 | vCPU state (x86_64) serializes via bincode-next serde compat | unit | `src/vmm/src/linux/vstate.rs` — compilation verifies (vCPU tests require KVM) | 4 |
| AC1.5 | vCPU state (aarch64) serializes via bincode-next native | unit | `src/vmm/src/linux/vstate.rs` — compilation verifies (aarch64-only) | 4 |
| AC1.5 | VmState (x86_64) serializes via bincode-next serde compat | unit | `src/vmm/src/lib.rs` — existing roundtrip tests at ~lines 2367-2422 migrated to bincode-next | 4 |
| AC1.5 | snapshot_store.rs merged snapshot and excluded pages use bincode-next | unit | `src/vmm/src/snapshot_store.rs` — 11 existing tests migrated (serialize/deserialize calls updated) | 4 |
| AC1.5 | builder.rs restore path uses bincode-next | unit | `src/vmm/src/builder.rs` — compilation verifies (full restore tested via integration tests) | 4 |
| AC4.1 | VmSnapshot, IncrementalSnapshot, SnapshotHeader, DirtyPage, InterruptControllerSnapshot use bincode-next Encode/Decode | unit | `src/vmm/src/snapshot.rs` — roundtrip tests fail to compile if derives are wrong | 4 |
| AC4.1 | VcpuState and VmState (x86_64) retain serde derives for KVM compat | unit | `src/vmm/src/linux/vstate.rs` — compilation verifies serde compat layer works | 4 |
| AC4.1 | Aarch64VcpuState uses bincode-next Encode/Decode | unit | `src/vmm/src/linux/vstate.rs` — compilation verifies (aarch64-only) | 4 |
| AC5.1 | No bincode 1.x in root `Cargo.lock` | build | `Cargo.lock` — `grep -A2 'name = "bincode"' Cargo.lock` must show no bincode 1.x entries | 5 |
| AC5.1 | No bincode 1.x in fuzz `Cargo.lock` | build | `fuzz/Cargo.lock` — `grep -A2 'name = "bincode"' fuzz/Cargo.lock` must show no bincode 1.x entries | 5 |
| AC5.1 | `cargo build --all-features` succeeds without bincode 1.x | build | Root workspace — full build verification | 5 |
| AC5.1 | `just check` passes (format + clippy) | build | Root workspace — linting verification | 5 |
| AC5.1 | `just test` passes (all unit tests) | unit | Root workspace — full unit test suite | 5 |
| AC5.2 | Fuzz workspace uses bincode-next for snapshot targets | build | `fuzz/Cargo.toml` — bincode-next dependency; `fuzz/fuzz_targets/fuzz_snapshot_deser.rs` uses `bincode_next::decode_from_slice` | 5 |
| AC5.2 | Fuzz targets compile | build | `cd fuzz && cargo check` | 5 |

## Human Verification

| AC ID | Criterion | Verification Approach | Phase |
|-------|-----------|----------------------|-------|
| AC1.1 | GICv3 (gicv3.rs, kvmgicv3.rs) Snapshottable impls use `snapshot_serde` | Code review: both files are aarch64-only and GIC tests require KVM on aarch64 hardware. Verify by inspecting that `bincode::serialize`/`bincode::deserialize` are replaced with `snapshot_serde::serialize`/`snapshot_serde::deserialize` and that `bincode_next::Encode`/`bincode_next::Decode` derives are present. `cargo check --features snapshot -p devices` on x86_64 confirms compilation of the non-arch-gated portions. Full functional test requires aarch64 CI. | 2 |
| AC1.1 | GPIO (aarch64) Snapshottable impl uses `snapshot_serde` | Code review: aarch64-only, no existing tests. GPIO state has only scalar fields. Verify derive change and serialize/deserialize call replacement by inspection. Compilation verified via `cargo check`. | 2 |
| AC3.2 | PL011 `read_fifo` validation is bounded by FIFO size | Code review on x86_64: PL011 is aarch64-only so tests cannot run on x86_64. Verify the new `if state.read_fifo.len() > PL011_FIFO_SIZE` guard exists in restore_state, positioned before field assignment. The pattern is identical to serial 16550's `in_buffer.len() > LOOP_SIZE` check which IS tested on x86_64. Full test execution requires aarch64 CI. | 2 |
| AC4.1 | GICv3 state structs use bincode-next Encode/Decode | Code review: `GicV3SnapshotState` in gicv3.rs and `GicV3State` in kvmgicv3.rs. Verify `#[derive(bincode_next::Encode, bincode_next::Decode)]` replaces serde derives. Compilation check confirms derive macros resolve. | 2 |
| AC4.1 | GPIO state struct uses bincode-next Encode/Decode | Code review: aarch64-only, no tests. Verify derive attribute by inspection. Compilation check confirms. | 2 |
| AC5.1 | No bincode 1.x references remain in source code | Run `grep -r 'bincode::' src/ --include='*.rs'` and `grep -r 'extern crate bincode' src/ --include='*.rs'` after Phase 5. Expected: zero matches. This confirms no stale imports or calls survived the migration. | 5 |
| AC5.1 | test_vsock_proxy workspace migrated from bincode 1.x | Code review + `cd tests/test_vsock_proxy && cargo check`: verify Cargo.toml uses bincode-next, ProxyState has Encode/Decode derives, and serialize/deserialize calls use bincode-next API. This is a separate test workspace not covered by `just test`. | 5 |
| AC5.1 | `serde` removed from devices snapshot feature gate | Code review: verify `src/devices/Cargo.toml` snapshot feature is `["bincode-next"]` (no serde, no bincode). Confirms no accidental serde dependency leak in device crate snapshot path. | 5 |
| AC5.2 | Fuzz target `fuzz_snapshot_deser.rs` intentionally omits `with_limit()` | Code review: verify the fuzz target uses `bincode_next::decode_from_slice` WITHOUT `with_limit()`. Fuzz targets should explore all code paths; the fuzzer's own memory limit handles OOM. This is a deliberate design choice, not an oversight. | 5 |
