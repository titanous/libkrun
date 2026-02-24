# Snapshot Completeness Implementation Plan — Phase 2

**Goal:** All legacy devices implement `Snapshottable` with state structs and bincode serialization.

**Architecture:** Each device gets a state struct containing only serializable fields, following the PL011/GPIO pattern (cfg_attr serde derives, bincode serialize/deserialize, cfg(feature="snapshot") blocks). Non-serializable fields (EventFd, Instant, trait objects) are excluded and preserved across restore.

**Tech Stack:** Rust (devices crate, serde + bincode behind `snapshot` feature flag)

**Scope:** 7 phases from original design (this is phase 2 of 7)

**Codebase verified:** 2026-02-24

---

## Acceptance Criteria Coverage

This phase implements and tests:

### snapshot-completeness.AC1: Legacy device state survives snapshot/restore
- **snapshot-completeness.AC1.1 Success:** 16550 Serial save/restore round-trip preserves all register values (interrupt_enable, line_control, modem_control, scratch, baud_divisor)
- **snapshot-completeness.AC1.2 Success:** 16550 Serial save/restore preserves in_buffer FIFO contents (non-empty buffer)
- **snapshot-completeness.AC1.3 Success:** i8042 save/restore preserves status, control, output port, command, and buffer contents
- **snapshot-completeness.AC1.4 Success:** CMOS save/restore preserves index register and all 128 data bytes
- **snapshot-completeness.AC1.5 Success:** PL031 RTC save/restore preserves tick_offset, load, match_value, imsc, ris
- **snapshot-completeness.AC1.7 Failure:** Restoring a snapshot with corrupted device state bytes returns SnapshotError, does not panic
- **snapshot-completeness.AC1.8 Edge:** Restoring a snapshot missing a device entry (e.g. old snapshot without CMOS) leaves that device at construction defaults

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->
<!-- START_TASK_1 -->
### Task 1: Serial16550 Snapshottable implementation

**Verifies:** snapshot-completeness.AC1.1, snapshot-completeness.AC1.2

**Files:**
- Modify: `src/devices/src/legacy/serial_16550.rs` (the shared module created in Phase 1)

**Implementation:**

Add the following to `serial_16550.rs`:

1. Add a `Serial16550State` struct after the `Serial` struct definition. Follow the PL011 pattern at `src/devices/src/legacy/aarch64/serial.rs:98-116`:

```rust
#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
struct Serial16550State {
    interrupt_enable: u8,
    interrupt_identification: u8,
    line_control: u8,
    line_status: u8,
    modem_control: u8,
    modem_status: u8,
    scratch: u8,
    baud_divisor: u16,
    in_buffer: Vec<u8>,
}
```

Note: `in_buffer` is `Vec<u8>` (not `VecDeque<u8>`) because VecDeque does not implement serde traits. Convert during save/restore, same as PL011's `read_fifo` pattern.

2. Add `use crate::snapshot::{SnapshotError, Snapshottable};` to imports.

3. Override `as_snapshottable` and `as_snapshottable_mut` in the existing `impl BusDevice for Serial` block:

```rust
fn as_snapshottable(&self) -> Option<&dyn Snapshottable> {
    Some(self)
}

fn as_snapshottable_mut(&mut self) -> Option<&mut dyn Snapshottable> {
    Some(self)
}
```

4. Implement `Snapshottable for Serial` following the GPIO pattern at `src/devices/src/legacy/aarch64/gpio.rs:260-313`:

```rust
impl Snapshottable for Serial {
    fn snapshot_id(&self) -> &str {
        "serial-16550"
    }

    fn save_state(&self) -> std::result::Result<Vec<u8>, SnapshotError> {
        let state = Serial16550State {
            interrupt_enable: self.interrupt_enable,
            interrupt_identification: self.interrupt_identification,
            line_control: self.line_control,
            line_status: self.line_status,
            modem_control: self.modem_control,
            modem_status: self.modem_status,
            scratch: self.scratch,
            baud_divisor: self.baud_divisor,
            in_buffer: self.in_buffer.iter().copied().collect(),
        };

        #[cfg(feature = "snapshot")]
        {
            bincode::serialize(&state).map_err(|e| SnapshotError::Serialize(e.to_string()))
        }
        #[cfg(not(feature = "snapshot"))]
        {
            let _ = state;
            Err(SnapshotError::Serialize(
                "snapshot feature not enabled".to_string(),
            ))
        }
    }

    fn restore_state(&mut self, data: &[u8]) -> std::result::Result<(), SnapshotError> {
        #[cfg(feature = "snapshot")]
        {
            let state: Serial16550State = bincode::deserialize(data)
                .map_err(|e| SnapshotError::Deserialize(e.to_string()))?;
            self.interrupt_enable = state.interrupt_enable;
            self.interrupt_identification = state.interrupt_identification;
            self.line_control = state.line_control;
            self.line_status = state.line_status;
            self.modem_control = state.modem_control;
            self.modem_status = state.modem_status;
            self.scratch = state.scratch;
            self.baud_divisor = state.baud_divisor;
            self.in_buffer = state.in_buffer.into_iter().collect();
            Ok(())
        }
        #[cfg(not(feature = "snapshot"))]
        {
            let _ = data;
            Err(SnapshotError::Deserialize(
                "snapshot feature not enabled".to_string(),
            ))
        }
    }
}
```

Fields excluded from state (preserved across restore): `interrupt_evt: EventFd`, `out: Option<Box<dyn io::Write + Send>>`, `input: Option<Box<dyn ReadableFd + Send>>`.

**Testing:**

Tests must verify each AC listed above:
- snapshot-completeness.AC1.1: Create Serial with non-default register values (set scratch, baud_divisor, line_control, modem_control, interrupt_enable), save_state, create fresh Serial, restore_state, read each register and assert match.
- snapshot-completeness.AC1.2: Push data into in_buffer (via loopback mode writes), save_state, restore to fresh Serial, read DATA register multiple times and verify all buffered bytes are present in order.

Add a `#[cfg(feature = "snapshot")]` test module.

Run: `cargo test -p devices --features net,snapshot`

Expected: All tests pass.

**Commit:** `feat: add Snapshottable implementation for 16550 serial`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: i8042 Snapshottable implementation

**Verifies:** snapshot-completeness.AC1.3

**Files:**
- Modify: `src/devices/src/legacy/i8042.rs`

**Implementation:**

1. Add `I8042State` struct near the top of the file (after the constants, before the `I8042Device` struct):

```rust
#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
struct I8042State {
    status: u8,
    control: u8,
    outp: u8,
    cmd: u8,
    buf: Vec<u8>,
    bhead: usize,
    btail: usize,
}
```

Note: `buf` is `Vec<u8>` (not `[u8; BUF_SIZE]`) because fixed-size arrays >32 don't implement serde by default in older serde versions. `bhead`/`btail` are plain `usize` (not `Wrapping<usize>`) since `Wrapping` doesn't derive serde — extract inner value on save, wrap on restore.

2. Add imports: `use crate::snapshot::{SnapshotError, Snapshottable};`

3. Override `as_snapshottable` and `as_snapshottable_mut` in the existing `impl BusDevice for I8042Device` block.

4. Implement `Snapshottable for I8042Device`:
   - `snapshot_id()` → `"i8042"`
   - `save_state()` — collect buf as `self.buf[..].to_vec()`, extract `self.bhead.0` and `self.btail.0`
   - `restore_state()` — copy Vec into fixed array `self.buf`, wrap bhead/btail in `Wrapping()`

Fields excluded: `reset_evt: EventFd`, `kbd_interrupt_evt: EventFd`.

**Testing:**

Tests must verify:
- snapshot-completeness.AC1.3: Set non-default status/control/outp/cmd values (e.g., write CMD_WRITE_CTR then write control value, write CMD_WRITE_OUTP then write outp value), push bytes into buffer. Save state, restore to fresh I8042Device, verify all register reads match and buffer contents are preserved.

Add a `#[cfg(feature = "snapshot")]` test module.

Run: `cargo test -p devices --features net,snapshot`

Expected: All tests pass.

**Commit:** `feat: add Snapshottable implementation for i8042`
<!-- END_TASK_2 -->
<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-4) -->
<!-- START_TASK_3 -->
### Task 3: CMOS Snapshottable implementation

**Verifies:** snapshot-completeness.AC1.4

**Files:**
- Modify: `src/devices/src/legacy/x86_64/cmos.rs`

**Implementation:**

1. Add `CmosState` struct:

```rust
#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
struct CmosState {
    index: u8,
    data: Vec<u8>,
}
```

Note: `data` is `Vec<u8>` rather than `[u8; 128]` for serde compatibility.

2. Add imports: `use crate::snapshot::{SnapshotError, Snapshottable};`

3. Override `as_snapshottable` and `as_snapshottable_mut` in `impl BusDevice for Cmos`.

4. Implement `Snapshottable for Cmos`:
   - `snapshot_id()` → `"cmos"`
   - `save_state()` — `data: self.data[..].to_vec()`
   - `restore_state()` — copy Vec into `self.data` array, verify length matches DATA_LEN

No non-serializable fields to exclude.

**Testing:**

Tests must verify:
- snapshot-completeness.AC1.4: Create Cmos with known memory values, write to index register (port 0x70), save state, restore to fresh Cmos, verify index register and all 128 data bytes match.

Add a `#[cfg(feature = "snapshot")]` test module.

Run: `cargo test -p devices --features net,snapshot`

Expected: All tests pass.

**Commit:** `feat: add Snapshottable implementation for CMOS`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: PL031 RTC Snapshottable implementation

**Verifies:** snapshot-completeness.AC1.5

**Files:**
- Modify: `src/devices/src/legacy/rtc_pl031.rs`

**Implementation:**

1. Add `RtcState` struct:

```rust
#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
struct RtcState {
    tick_offset: i64,
    previous_now_elapsed_nanos: u64,
    load: u32,
    match_value: u32,
    imsc: u32,
    ris: u32,
}
```

`previous_now: Instant` cannot be serialized. Instead, save elapsed time since `previous_now` as `previous_now_elapsed_nanos`. On restore, reconstruct `previous_now` as `Instant::now()` and adjust `tick_offset` by the difference in elapsed time.

2. Add imports: `use crate::snapshot::{SnapshotError, Snapshottable};`

3. Override `as_snapshottable` and `as_snapshottable_mut` in `impl BusDevice for RTC`.

4. Implement `Snapshottable for RTC`:
   - `snapshot_id()` → `"pl031"`
   - `save_state()`:
     ```rust
     let elapsed = Instant::now().duration_since(self.previous_now).as_nanos() as u64;
     RtcState {
         tick_offset: self.tick_offset,
         previous_now_elapsed_nanos: elapsed,
         load: self.load,
         match_value: self.match_value,
         imsc: self.imsc,
         ris: self.ris,
     }
     ```
   - `restore_state()`:
     ```rust
     self.previous_now = Instant::now() - Duration::from_nanos(state.previous_now_elapsed_nanos);
     self.tick_offset = state.tick_offset;
     self.load = state.load;
     self.match_value = state.match_value;
     self.imsc = state.imsc;
     self.ris = state.ris;
     ```
     The `previous_now` is reconstructed so that `Instant::now().duration_since(previous_now)` returns approximately the same elapsed time, preserving the guest's time continuity via `get_time()`.

Fields excluded: `interrupt_evt: EventFd`.

**Testing:**

Tests must verify:
- snapshot-completeness.AC1.5: Set RTC load register, match_value, imsc, and ris via writes. Save state, restore to fresh RTC, verify all register reads match and get_time() returns a consistent value (within a small tolerance for wall-clock elapsed time during the test).

Add a `#[cfg(feature = "snapshot")]` test module.

Run: `cargo test -p devices --features net,snapshot`

Expected: All tests pass.

**Commit:** `feat: add Snapshottable implementation for PL031 RTC`
<!-- END_TASK_4 -->
<!-- END_SUBCOMPONENT_B -->

<!-- START_TASK_5 -->
### Task 5: Corrupted state deserialization test

**Verifies:** snapshot-completeness.AC1.7

**Files:**
- Modify: `src/devices/src/legacy/serial_16550.rs` (add test to existing snapshot test module)

**Implementation:**

Add a test to the `#[cfg(feature = "snapshot")]` test module in serial_16550.rs:
- Call `restore_state(&[0xFF, 0xFF, 0xFF])` with garbage bytes
- Assert it returns `Err(SnapshotError::Deserialize(_))`
- Assert no panic occurred

This validates AC1.7 for the general pattern. The same bincode deserialization error path applies to all four devices.

**Verification:**

Run: `cargo test -p devices --features net,snapshot`

Expected: All tests pass.

**Commit:** `test: verify corrupted snapshot state returns error without panic`
<!-- END_TASK_5 -->
