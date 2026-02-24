# Snapshot Completeness Implementation Plan — Phase 3

**Goal:** x86_64 PortIO devices participate in the snapshot save/restore flow.

**Architecture:** Add `save_all_device_states()` and `restore_all_device_states()` to `PortIODeviceManager`, mirroring the `MMIODeviceManager` pattern. Merge PortIO states into `VmSnapshot.device_states` in the orchestration code in `lib.rs`. PortIO is x86_64-only so only Linux x86_64 snapshot paths need changes.

**Tech Stack:** Rust (vmm crate, snapshot feature)

**Scope:** 7 phases from original design (this is phase 3 of 7)

**Codebase verified:** 2026-02-24

---

## Acceptance Criteria Coverage

This phase implements and tests:

### snapshot-completeness.AC1: Legacy device state survives snapshot/restore
- **snapshot-completeness.AC1.6 Success:** PortIO device states appear in VmSnapshot.device_states after full snapshot
- **snapshot-completeness.AC1.8 Edge:** Restoring a snapshot missing a device entry (e.g. old snapshot without CMOS) leaves that device at construction defaults

---

<!-- START_TASK_1 -->
### Task 1: Add save_all_device_states to PortIODeviceManager

**Verifies:** snapshot-completeness.AC1.6

**Files:**
- Modify: `src/vmm/src/device_manager/legacy.rs:53-146` (add method to `impl PortIODeviceManager`)

**Implementation:**

First, add a `SnapshotState(String)` variant to the `Error` enum in this file (after line 21), matching the pattern in `src/vmm/src/device_manager/kvm/mmio.rs`:

```rust
SnapshotState(String),
```

Add to the `Display` match (after the EventFd arm):
```rust
SnapshotState(ref msg) => write!(f, "Snapshot state error: {msg}"),
```

Then add a `#[cfg(feature = "snapshot")]` method to `PortIODeviceManager` after `register_devices()` (after line 146). Unlike `MMIODeviceManager` which iterates a HashMap of dynamic devices, `PortIODeviceManager` has named fields so iterate them explicitly:

```rust
#[cfg(feature = "snapshot")]
pub fn save_all_device_states(&self) -> std::result::Result<Vec<(String, Vec<u8>)>, Error> {
    let mut states = Vec::new();

    // Save CMOS state
    {
        let device = self.cmos.lock().unwrap();
        if let Some(snapshottable) = device.as_snapshottable() {
            let state = snapshottable.save_state().map_err(|e| {
                Error::SnapshotState(format!("Failed to save CMOS state: {e}"))
            })?;
            states.push((snapshottable.snapshot_id().to_string(), state));
        }
    }

    // Save serial states
    for (i, serial) in self.stdio_serial.iter().enumerate() {
        let device = serial.lock().unwrap();
        if let Some(snapshottable) = device.as_snapshottable() {
            let id = format!("{}:{}", snapshottable.snapshot_id(), i);
            let state = snapshottable.save_state().map_err(|e| {
                Error::SnapshotState(format!("Failed to save serial {i} state: {e}"))
            })?;
            states.push((id, state));
        }
    }

    // Save i8042 state
    {
        let device = self.i8042.lock().unwrap();
        if let Some(snapshottable) = device.as_snapshottable() {
            let state = snapshottable.save_state().map_err(|e| {
                Error::SnapshotState(format!("Failed to save i8042 state: {e}"))
            })?;
            states.push((snapshottable.snapshot_id().to_string(), state));
        }
    }

    Ok(states)
}
```

**Verification:**

Not verifiable on its own — continue to Task 2.

**Commit:** Do not commit yet — continue to Task 2.
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add restore_all_device_states to PortIODeviceManager

**Verifies:** snapshot-completeness.AC1.8

**Files:**
- Modify: `src/vmm/src/device_manager/legacy.rs` (add method after save_all_device_states)

**Implementation:**

Add restore method. Unlike MMIO which searches by ID across a HashMap, PortIO can match by snapshot_id directly. States missing from the snapshot are silently skipped (AC1.8: old snapshots without new device entries leave defaults).

```rust
#[cfg(feature = "snapshot")]
pub fn restore_all_device_states(&self, states: &[(String, Vec<u8>)]) -> std::result::Result<(), Error> {
    for (id, data) in states {
        // Try CMOS
        {
            let mut device = self.cmos.lock().unwrap();
            if let Some(snapshottable) = device.as_snapshottable() {
                if snapshottable.snapshot_id() == id {
                    device.as_snapshottable_mut().unwrap()
                        .restore_state(data)
                        .map_err(|e| Error::SnapshotState(format!(
                            "Failed to restore {id}: {e}"
                        )))?;
                    continue;
                }
            }
        }

        // Try serials (match "serial-16550:N" pattern)
        let mut matched = false;
        for (i, serial) in self.stdio_serial.iter().enumerate() {
            let mut device = serial.lock().unwrap();
            if let Some(snapshottable) = device.as_snapshottable() {
                let expected_id = format!("{}:{}", snapshottable.snapshot_id(), i);
                if &expected_id == id {
                    device.as_snapshottable_mut().unwrap()
                        .restore_state(data)
                        .map_err(|e| Error::SnapshotState(format!(
                            "Failed to restore {id}: {e}"
                        )))?;
                    matched = true;
                    break;
                }
            }
        }
        if matched { continue; }

        // Try i8042
        {
            let mut device = self.i8042.lock().unwrap();
            if let Some(snapshottable) = device.as_snapshottable() {
                if snapshottable.snapshot_id() == id {
                    device.as_snapshottable_mut().unwrap()
                        .restore_state(data)
                        .map_err(|e| Error::SnapshotState(format!(
                            "Failed to restore {id}: {e}"
                        )))?;
                    continue;
                }
            }
        }

        // Unknown PortIO device state — skip silently (forward compat)
        debug!("Skipping unknown PortIO device state: {id}");
    }

    Ok(())
}
```

**Verification:**

Run: `cargo test -p vmm --features snapshot`

Expected: Build succeeds. Existing tests pass.

**Commit:** `feat: add snapshot save/restore to PortIODeviceManager`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Wire PortIO states into snapshot orchestration

**Files:**
- Modify: `src/vmm/src/lib.rs:404-443` (create_snapshot — Linux x86_64 full)
- Modify: `src/vmm/src/lib.rs:483-489` (restore_snapshot — Linux x86_64 full)
- Modify: `src/vmm/src/lib.rs:1014-1019` (create_incremental_snapshot — Linux x86_64 incremental)
- Modify: `src/vmm/src/lib.rs:1098-1104` (restore_incremental_snapshot — Linux x86_64 incremental)

**Implementation:**

In all four snapshot functions on the Linux x86_64 path, merge PortIO device states alongside MMIO states.

**Save paths** (create_snapshot and create_incremental_snapshot):

After `let device_states = self.mmio_device_manager.save_all_device_states()...`, add:

```rust
#[cfg(target_arch = "x86_64")]
{
    let pio_states = self.pio_device_manager.save_all_device_states()
        .map_err(|e| {
            snapshot::SnapshotError::Serialize(format!("Failed to save PortIO device states: {e}"))
        })?;
    device_states.extend(pio_states);
}
```

Note: `device_states` needs to be `let mut device_states` for the extend call.

**Restore paths** (restore_snapshot and restore_incremental_snapshot):

After `self.mmio_device_manager.restore_all_device_states(...)`, add:

```rust
#[cfg(target_arch = "x86_64")]
{
    self.pio_device_manager
        .restore_all_device_states(&vmstate.device_states) // or &incremental.device_states
        .map_err(|e| {
            snapshot::SnapshotError::Deserialize(format!(
                "Failed to restore PortIO device states: {e}"
            ))
        })?;
}
```

The restore_all_device_states method will only match entries that belong to PortIO devices (by snapshot_id), silently skipping MMIO entries.

**Verification:**

Run: `cargo build -p vmm --features snapshot`

Expected: Build succeeds.

Run: `cargo test -p vmm --features snapshot`

Expected: All existing tests pass.

**Commit:** `feat: wire PortIO device states into snapshot save/restore`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Unit tests for PortIO snapshot integration

**Verifies:** snapshot-completeness.AC1.6, snapshot-completeness.AC1.8

**Files:**
- Modify: `src/vmm/src/device_manager/legacy.rs:149-184` (add tests to existing test module)

**Implementation:**

Add `#[cfg(feature = "snapshot")]` tests to the existing test module:

**Testing:**

Tests must verify:
- snapshot-completeness.AC1.6: Create PortIODeviceManager, call save_all_device_states, verify returned vec contains entries for "cmos", "serial-16550:0", and "i8042". Verify each entry has non-empty serialized bytes.
- snapshot-completeness.AC1.8: Call restore_all_device_states with an empty slice. Verify it succeeds (devices keep construction defaults). Call with states containing an unknown device ID — verify it succeeds (skips unknown entries).

Run: `cargo test -p vmm --features snapshot`

Expected: All tests pass.

**Commit:** `test: verify PortIO device states in snapshot save/restore`
<!-- END_TASK_4 -->
