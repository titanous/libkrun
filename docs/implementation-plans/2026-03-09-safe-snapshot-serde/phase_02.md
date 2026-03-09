# Safe Snapshot Serialization Implementation Plan — Phase 2

**Goal:** Migrate all legacy device `Snapshottable` and `IrqChipT` snapshot implementations from bincode 1.x to `snapshot_serde`.

**Architecture:** Mechanical transformation: switch serde derives to bincode-next Encode/Decode, replace `bincode::serialize`/`deserialize` calls with `snapshot_serde::serialize`/`deserialize`, preserve existing field validation, add PL011 FIFO validation.

**Tech Stack:** Rust, bincode-next 3.0.0-rc.5, snapshot_serde module from Phase 1

**Scope:** 5 phases from original design (phase 2 of 5)

**Codebase verified:** 2026-03-09

---

## Acceptance Criteria Coverage

This phase implements and tests:

### safe-snapshot-serde.AC1: All bincode call sites migrated to bincode-next
- **safe-snapshot-serde.AC1.1 Success:** Legacy device Snapshottable impls (serial 16550, i8042, CMOS, PL011, RTC, GPIO, GICv3) serialize/deserialize via `snapshot_serde` module
- **safe-snapshot-serde.AC1.2 Success:** Legacy device snapshot round-trip tests pass with new backend

### safe-snapshot-serde.AC3: Existing field-level validation preserved
- **safe-snapshot-serde.AC3.1 Success:** CMOS `data.len() == 128`, i8042 `buf.len() == 16`, serial 16550 `in_buffer.len() <= 64` checks remain and reject invalid sizes
- **safe-snapshot-serde.AC3.2 Success:** PL011 gains `read_fifo` length validation (bounded by FIFO size)

### safe-snapshot-serde.AC4: State struct derives use bincode-next native Encode/Decode
- **safe-snapshot-serde.AC4.1 Success:** All `*State` structs use `#[cfg_attr(feature = "snapshot", derive(bincode_next::Encode, bincode_next::Decode))]` instead of serde derives

---

## Migration Pattern

Every device follows the same transformation:

1. **State struct:** Change `derive(serde::Serialize, serde::Deserialize)` → `derive(bincode_next::Encode, bincode_next::Decode)`
2. **Add constant:** `const MAX_SNAPSHOT_BYTES: usize = N;` near state struct
3. **save_state():** Change `bincode::serialize(&state)` → `snapshot_serde::serialize(&state)`
4. **restore_state():** Change `bincode::deserialize(data)` → `snapshot_serde::deserialize::<_, { MAX_SNAPSHOT_BYTES }>(data)`
5. **Add import:** `use crate::snapshot_serde;` at module level (within `#[cfg(feature = "snapshot")]`)
6. **Preserve validation:** All existing field-level checks remain unchanged

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Migrate Serial 16550

**Verifies:** safe-snapshot-serde.AC1.1, safe-snapshot-serde.AC3.1, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/devices/src/legacy/serial_16550.rs`

**Implementation:**

This device has existing validation (`in_buffer.len() > LOOP_SIZE` check) and 5 existing snapshot tests. It serves as the template for all subsequent migrations.

At `src/devices/src/legacy/serial_16550.rs`:

1. **Add MAX_SNAPSHOT_BYTES** near the state struct (~line 56):
   ```rust
   const MAX_SNAPSHOT_BYTES: usize = 128;
   ```

2. **Change derive** on `Serial16550State` (line 57):
   ```rust
   // FROM:
   #[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
   // TO:
   #[cfg_attr(feature = "snapshot", derive(bincode_next::Encode, bincode_next::Decode))]
   ```

3. **Change save_state()** (~line 290-314): Replace `bincode::serialize(&state).map_err(|e| SnapshotError::Serialize(e.to_string()))` with `snapshot_serde::serialize(&state)` inside the `#[cfg(feature = "snapshot")]` block.

4. **Change restore_state()** (~line 316-346): Replace `bincode::deserialize(data).map_err(|e| SnapshotError::Deserialize(e.to_string()))` with `snapshot_serde::deserialize::<_, { MAX_SNAPSHOT_BYTES }>(data)` inside the `#[cfg(feature = "snapshot")]` block. Keep the `in_buffer.len() > LOOP_SIZE` validation unchanged.

5. **Update test imports** if any tests directly call `bincode::serialize`/`bincode::deserialize` — replace with `snapshot_serde::serialize`/`snapshot_serde::deserialize::<_, { MAX_SNAPSHOT_BYTES }>`. The tests at lines 389-523 (`test_serial_snapshot_register_roundtrip`, `test_serial_snapshot_buffer_roundtrip`, `test_serial_snapshot_corrupted_state`, `test_serial_snapshot_rejects_huge_length_prefix`, `test_serial_snapshot_rejects_oversized_buffer`) exercise save_state/restore_state so they should pass with the new backend. If any test constructs raw bincode 1.x bytes, update the byte format.

**Verification:**
```bash
cargo test --features snapshot -p devices -- serial_16550::snapshot_tests
```
Expected: All 5 tests pass.

**Commit:** `refactor(devices): migrate serial_16550 snapshot to bincode-next`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Migrate i8042 and CMOS

**Verifies:** safe-snapshot-serde.AC1.1, safe-snapshot-serde.AC3.1, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/devices/src/legacy/i8042.rs`
- Modify: `src/devices/src/legacy/x86_64/cmos.rs`

**Implementation:**

Both devices have existing validation and tests. Apply the same pattern as Task 1.

**i8042** (`src/devices/src/legacy/i8042.rs`):

1. Add `const MAX_SNAPSHOT_BYTES: usize = 128;` near state struct (~line 72)
2. Change derive on `I8042State` (line 73): `serde::Serialize, serde::Deserialize` → `bincode_next::Encode, bincode_next::Decode`
3. save_state() (~line 338): `bincode::serialize` → `snapshot_serde::serialize`
4. restore_state() (~line 362): `bincode::deserialize` → `snapshot_serde::deserialize::<_, { MAX_SNAPSHOT_BYTES }>`. Keep `buf.len() != BUF_SIZE` validation unchanged.
5. Update any direct bincode calls in tests (lines 392-498: `test_i8042_snapshot_registers`, `test_i8042_snapshot_buffer`, `test_i8042_snapshot_corrupted_state`, `test_i8042_snapshot_invalid_buf_length`)

**CMOS** (`src/devices/src/legacy/x86_64/cmos.rs`):

1. Add `const MAX_SNAPSHOT_BYTES: usize = 512;` near state struct (~line 16)
2. Change derive on `CmosState` (line 18): `serde::Serialize, serde::Deserialize` → `bincode_next::Encode, bincode_next::Decode`
3. save_state() (~line 105): `bincode::serialize` → `snapshot_serde::serialize`
4. restore_state() (~line 124): `bincode::deserialize` → `snapshot_serde::deserialize::<_, { MAX_SNAPSHOT_BYTES }>`. Keep `data.len() != DATA_LEN` validation unchanged.
5. Update test at lines 150-195 (`test_cmos_snapshot_preserves_index_and_data`)

**Verification:**
```bash
cargo test --features snapshot -p devices -- i8042::snapshot_tests
cargo test --features snapshot -p devices -- cmos::snapshot_tests
```
Expected: All tests pass.

**Commit:** `refactor(devices): migrate i8042 and CMOS snapshots to bincode-next`

<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-4) -->

<!-- START_TASK_3 -->
### Task 3: Migrate PL011 with new FIFO validation

**Verifies:** safe-snapshot-serde.AC1.1, safe-snapshot-serde.AC3.2, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/devices/src/legacy/aarch64/serial.rs`

**Implementation:**

PL011 currently has NO post-deserialization validation for `read_fifo`. The design requires adding a length check bounded by the FIFO size, matching the serial 16550 pattern.

The PL011 hardware FIFO depth is 16 characters. Look for a FIFO size constant in the file (e.g., `PL011_FIFO_SIZE`, `FIFO_DEPTH`, or similar). If none exists, define `const PL011_FIFO_SIZE: usize = 16;` near the top of the file.

At `src/devices/src/legacy/aarch64/serial.rs`:

1. Add `const MAX_SNAPSHOT_BYTES: usize = 512;` near the state struct (~line 97)
2. Add FIFO size constant if not already present: `const PL011_FIFO_SIZE: usize = 16;`
3. Change derive on `SerialState` (line 98): `serde::Serialize, serde::Deserialize` → `bincode_next::Encode, bincode_next::Decode`
4. save_state() (~line 425): `bincode::serialize` → `snapshot_serde::serialize`
5. restore_state() (~line 457): `bincode::deserialize` → `snapshot_serde::deserialize::<_, { MAX_SNAPSHOT_BYTES }>`. **Add new validation** after deserialization, before field assignment:
   ```rust
   if state.read_fifo.len() > PL011_FIFO_SIZE {
       return Err(SnapshotError::Deserialize(format!(
           "PL011 read_fifo length {} exceeds FIFO size {}",
           state.read_fifo.len(),
           PL011_FIFO_SIZE,
       )));
   }
   ```

**Testing:**

PL011 currently has NO snapshot tests. Add a `#[cfg(all(test, feature = "snapshot"))] mod snapshot_tests` module with tests that verify:
- safe-snapshot-serde.AC3.2: Round-trip succeeds with valid read_fifo, fails when read_fifo exceeds PL011_FIFO_SIZE
- Basic register round-trip: save state, restore to fresh device, verify fields match

Follow the exact pattern from serial_16550 snapshot tests (save_state → restore_state → assert).

**Verification:**

PL011 is gated by `#[cfg(target_arch = "aarch64")]` in `src/devices/src/legacy/mod.rs` (line 38-43). On an x86_64 host, the module is not compiled at all. To verify:

- **On aarch64 host (or CI):** `cargo test --features snapshot -p devices -- serial::snapshot_tests`
- **On x86_64 host:** `cargo check --features snapshot -p devices --target aarch64-unknown-linux-gnu` (requires `rustup target add aarch64-unknown-linux-gnu`). This verifies compilation but cannot run tests. Alternatively, verify the code changes are correct by inspection — the mechanical transformation is identical to serial_16550 (Task 1) which IS testable on x86_64.
- **Minimum verification on x86_64:** Run `cargo check --features snapshot -p devices` to confirm no x86_64 compilation regressions, then visually confirm the PL011 changes match the serial_16550 pattern.

**Commit:** `refactor(devices): migrate PL011 snapshot to bincode-next with FIFO validation`

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Migrate RTC PL031 and GPIO

**Verifies:** safe-snapshot-serde.AC1.1, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/devices/src/legacy/rtc_pl031.rs`
- Modify: `src/devices/src/legacy/aarch64/gpio.rs`

**Implementation:**

Both devices have only scalar fields (no Vec/String) so no field-level validation is needed. RTC has 1 existing test; GPIO has none.

**RTC PL031** (`src/devices/src/legacy/rtc_pl031.rs`):

1. Add `const MAX_SNAPSHOT_BYTES: usize = 128;` near state struct (~line 57)
2. Change derive on `RtcState` (line 58): `serde::Serialize, serde::Deserialize` → `bincode_next::Encode, bincode_next::Decode`
3. save_state() (~line 208): `bincode::serialize` → `snapshot_serde::serialize`
4. restore_state() (~line 232): `bincode::deserialize` → `snapshot_serde::deserialize::<_, { MAX_SNAPSHOT_BYTES }>`
5. Update test at lines 315-376 (`test_rtc_snapshot_preserves_registers`)

**GPIO** (`src/devices/src/legacy/aarch64/gpio.rs`):

1. Add `const MAX_SNAPSHOT_BYTES: usize = 128;` near state struct (~line 81)
2. Change derive on `GpioState` (line 82): `serde::Serialize, serde::Deserialize` → `bincode_next::Encode, bincode_next::Decode`
3. save_state() (~line 265): `bincode::serialize` → `snapshot_serde::serialize`
4. restore_state() (~line 290): `bincode::deserialize` → `snapshot_serde::deserialize::<_, { MAX_SNAPSHOT_BYTES }>`

**Verification:**
```bash
cargo test --features snapshot -p devices -- rtc_pl031::snapshot_tests
```
Expected: RTC test passes. GPIO has no tests (acceptable — all scalar fields, no validation needed).

**Commit:** `refactor(devices): migrate RTC PL031 and GPIO snapshots to bincode-next`

<!-- END_TASK_4 -->

<!-- END_SUBCOMPONENT_B -->

<!-- START_SUBCOMPONENT_C (tasks 5-6) -->

<!-- START_TASK_5 -->
### Task 5: Migrate GICv3 devices

**Verifies:** safe-snapshot-serde.AC1.1, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/devices/src/legacy/gicv3.rs`
- Modify: `src/devices/src/legacy/kvmgicv3.rs`

**Implementation:**

Both GIC devices use the `IrqChipT` trait (`save_snapshot_state`/`restore_snapshot_state`) instead of `Snapshottable`. The methods have different signatures:
- `save_snapshot_state(&self) -> Option<Vec<u8>>` (returns Option, not Result)
- `restore_snapshot_state(&mut self, data: &[u8])` (returns (), not Result)

**gicv3.rs** (userspace GIC emulation, `src/devices/src/legacy/gicv3.rs`):

1. Add `const MAX_SNAPSHOT_BYTES: usize = 4096;` near state struct (~line 391)
2. Change derive on `GicV3SnapshotState` (line 392):
   ```rust
   // FROM:
   #[cfg(feature = "snapshot")]
   #[derive(serde::Serialize, serde::Deserialize)]
   // TO:
   #[cfg(feature = "snapshot")]
   #[derive(bincode_next::Encode, bincode_next::Decode)]
   ```
3. save_snapshot_state() (line 429): Change `bincode::serialize(&state).ok()` → `snapshot_serde::serialize(&state).ok()`
4. restore_snapshot_state() (line 440): Change `bincode::deserialize::<GicV3SnapshotState>(data)` → `snapshot_serde::deserialize::<GicV3SnapshotState, { MAX_SNAPSHOT_BYTES }>(data)`. Keep the `edge_trigger.len()` and `gicd_irouter.len()` validation in the `if let Ok(state)` block unchanged.

**kvmgicv3.rs** (KVM in-kernel GIC, `src/devices/src/legacy/kvmgicv3.rs`):

1. Add `const MAX_SNAPSHOT_BYTES: usize = 4096;` near state struct (~line 74)
2. Change derive on `GicV3State` (line 75-76):
   ```rust
   // FROM:
   #[cfg(feature = "snapshot")]
   #[derive(serde::Serialize, serde::Deserialize)]
   // TO:
   #[cfg(feature = "snapshot")]
   #[derive(bincode_next::Encode, bincode_next::Decode)]
   ```
3. In `save_snapshot_state()` on IrqChipT: change `bincode::serialize` → `snapshot_serde::serialize`
4. In `restore_snapshot_state()` on IrqChipT: change `bincode::deserialize` → `snapshot_serde::deserialize::<GicV3State, { MAX_SNAPSHOT_BYTES }>`. Keep existing validation unchanged.

**Verification:**
```bash
cargo check --features snapshot -p devices
```
Expected: Compiles (GICv3 tests require aarch64 + KVM, compile check is sufficient on x86_64).

**Commit:** `refactor(devices): migrate GICv3 snapshots to bincode-next`

<!-- END_TASK_5 -->

<!-- START_TASK_6 -->
### Task 6: Verify all legacy device tests pass

**Verifies:** safe-snapshot-serde.AC1.2

**Files:** None (verification only)

**Step 1: Run all device snapshot tests**

```bash
cargo test --features snapshot -p devices
```

Expected: All existing and new snapshot tests pass.

**Step 2: Run full check**

```bash
just check
```

Expected: Format + clippy pass.

**Commit:** None (verification only). If any fixes needed, commit as `fix(devices): ...`.

<!-- END_TASK_6 -->

<!-- END_SUBCOMPONENT_C -->
