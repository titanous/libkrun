# Memory Balloon Device Implementation Plan — Phase 7: Balloon Snapshottable

**Goal:** Balloon device state survives snapshot/restore cycle, retaining inflation target and actual count.

**Architecture:** Implement `save_backend_state()` and `restore_backend_state()` on the Balloon `VirtioDevice` trait impl to serialize config (num_pages, actual) and hinting command state. The MMIO transport's `Snapshottable` impl already serializes queue states and acked_features and calls `save_backend_state()`, so balloon-specific state flows through this existing mechanism. Reclaimed page bitmaps are NOT serialized — they start empty after restore (no pages are reclaimed in a freshly-restored VM).

**Tech Stack:** Rust, bincode/serde (behind `snapshot` feature)

**Scope:** 7 phases from original design (phase 7 of 7)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### mem-balloon.AC4: Rust API enables inflate->snapshot workflow
- **mem-balloon.AC4.5 Success:** Balloon device state survives snapshot/restore — after restore, balloon retains inflation target and actual count

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->
<!-- START_TASK_1 -->
### Task 1: Create BalloonState struct with serde derives

**Verifies:** None (type prerequisite for AC4.5)

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs` (add `BalloonState` struct)

**Implementation:**

Add a serializable state struct near the top of `device.rs`:

```rust
/// Serializable balloon device state for snapshot/restore.
///
/// Contains device-specific fields not covered by MmioTransportState
/// (which already handles queue states and acked_features).
/// Reclaimed page bitmaps are NOT included — they are transient host state
/// that starts empty after restore.
#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
struct BalloonState {
    /// Config space: num_pages (inflation target set by host)
    num_pages: u32,
    /// Config space: actual (current inflation reported by guest)
    actual: u32,
    /// Config space: free_page_report_cmd_id
    free_page_report_cmd_id: u32,
    /// Config space: poison_val
    poison_val: u32,
    /// Free page hinting command counter (monotonically increasing)
    hinting_cmd_counter: u32,
    /// Current host-requested hinting command
    hinting_host_cmd: u32,
}
```

Note: `VirtioBalloonConfig` is `#[repr(C, packed)]` and cannot directly derive serde. The `BalloonState` struct copies the relevant fields as plain integers for serialization.

The `avail_features` and `acked_features` fields are already serialized by the MMIO transport's `MmioTransportState` (see `src/devices/src/virtio/mmio.rs:730`), so they are NOT duplicated here.

**Verification:**
Run: `cargo check -p devices --features snapshot`
Expected: Compiles without errors

**Commit:** `feat(balloon): add BalloonState struct for snapshot serialization`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Implement save_backend_state and restore_backend_state

**Verifies:** mem-balloon.AC4.5

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs` (override `save_backend_state()` and `restore_backend_state()` in the `impl VirtioDevice for Balloon` block)

**Implementation:**

The MMIO transport's `Snapshottable::save_state()` at `src/devices/src/virtio/mmio.rs:700-748` calls `device.save_backend_state()` after quiescing and serializing queue states. The returned `Option<Vec<u8>>` is stored in `MmioTransportState.backend_state`. On restore, `device.restore_backend_state(data)` is called with those bytes.

Override the default no-op implementations in the `impl VirtioDevice for Balloon` block:

```rust
fn save_backend_state(&self) -> Option<Vec<u8>> {
    let state = BalloonState {
        num_pages: self.config.num_pages,
        actual: self.config.actual,
        free_page_report_cmd_id: self.config.free_page_report_cmd_id,
        poison_val: self.config.poison_val,
        hinting_cmd_counter: self.hinting_cmd_counter,
        hinting_host_cmd: self.hinting_host_cmd,
    };

    #[cfg(feature = "snapshot")]
    {
        match bincode::serialize(&state) {
            Ok(data) => Some(data),
            Err(e) => {
                log::error!("balloon: failed to serialize backend state: {e}");
                None
            }
        }
    }
    #[cfg(not(feature = "snapshot"))]
    {
        let _ = state;
        None
    }
}

fn restore_backend_state(&mut self, data: &[u8]) {
    #[cfg(feature = "snapshot")]
    {
        match bincode::deserialize::<BalloonState>(data) {
            Ok(state) => {
                self.config.num_pages = state.num_pages;
                self.config.actual = state.actual;
                self.config.free_page_report_cmd_id = state.free_page_report_cmd_id;
                self.config.poison_val = state.poison_val;
                self.hinting_cmd_counter = state.hinting_cmd_counter;
                self.hinting_host_cmd = state.hinting_host_cmd;
                // stats_desc_index is intentionally NOT restored — it refers to a
                // descriptor index in the stats queue which is re-initialized by
                // MmioTransport queue restore. The guest will re-push a stats buffer
                // after resume, providing a fresh descriptor index.
                self.stats_desc_index = None;

                log::debug!(
                    "balloon: restored state: num_pages={}, actual={}",
                    state.num_pages,
                    state.actual
                );
            }
            Err(e) => {
                log::error!("balloon: failed to deserialize backend state: {e}");
            }
        }
    }
    #[cfg(not(feature = "snapshot"))]
    {
        let _ = data;
    }
}
```

After restore:
- `num_pages` retains the inflation target — the guest will continue inflating/deflating toward this target
- `actual` retains the guest's last reported inflation count
- Hinting state is preserved — a future hinting request will use correct command IDs
- `stats_desc_index` is reset to `None` on restore — the old descriptor index referred to the pre-snapshot stats queue and is invalid after MmioTransport queue re-initialization. The guest will re-push a stats buffer after resume.
- Reclaimed bitmaps (`inflated_bitmap`, `reported_free_bitmap`) are `None` after construction and only created during `activate()` — they start empty on restore, which is correct

**Testing:**
Tests must verify:
- mem-balloon.AC4.5: After `save_backend_state()`, deserializing the result produces a `BalloonState` with matching field values. After `restore_backend_state(data)`, the device's config fields match the original.

Include unit tests in the `#[cfg(test)]` module (or create one if not present):
- Test save/restore roundtrip: create Balloon, set config fields, save state, create fresh Balloon, restore state, verify fields match
- Test restore with empty data: `restore_backend_state(&[])` logs error but doesn't panic

**Verification:**
Run: `cargo check -p devices --features snapshot`
Expected: Compiles without errors

**Commit:** `feat(balloon): implement save/restore backend state for snapshots`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Verify snapshot round-trip with unit tests

**Verifies:** mem-balloon.AC4.5

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs` (add snapshot unit tests)

**Implementation:**

Add a `#[cfg(test)]` module (or extend the existing one) with snapshot tests:

Test `balloon_snapshot_roundtrip`:
1. Create a `Balloon::new().unwrap()`
2. Set distinctive config values:
   - `balloon.config.num_pages = 12345`
   - `balloon.config.actual = 6789`
   - `balloon.config.free_page_report_cmd_id = 42`
   - `balloon.hinting_cmd_counter = 10`
   - `balloon.hinting_host_cmd = 5`
   - `balloon.stats_desc_index = Some(3)`
3. Call `save_backend_state()` — assert returns `Some(data)`
4. Create a second `Balloon::new().unwrap()`
5. Call `restore_backend_state(&data)` on the second balloon
6. Assert config and hinting fields match:
   - `balloon2.config.num_pages == 12345`
   - `balloon2.config.actual == 6789`
   - `balloon2.config.free_page_report_cmd_id == 42`
   - `balloon2.hinting_cmd_counter == 10`
   - `balloon2.hinting_host_cmd == 5`
7. Assert `balloon2.stats_desc_index == None` (intentionally reset — old descriptor index is invalid after queue re-initialization)

Test `balloon_snapshot_empty_data_no_panic`:
1. Create a `Balloon::new().unwrap()`
2. Call `restore_backend_state(&[])` — should not panic
3. Config values should remain at defaults

Test `balloon_snapshot_no_bitmaps`:
1. Create a `Balloon::new().unwrap()`
2. Verify `inflated_bitmap` is `None` and `reported_free_bitmap` is `None`
3. Save state, restore to new balloon
4. Verify bitmaps are still `None` (they are not serialized)

**Verification:**
Run: `cargo test -p devices --features net,snapshot -- balloon`
Expected: New snapshot tests pass

**Commit:** `test(balloon): add snapshot round-trip tests`
<!-- END_TASK_3 -->
<!-- END_SUBCOMPONENT_A -->

<!-- START_TASK_4 -->
### Task 4: Verify full phase builds and tests

**Verifies:** None (verification)

**Files:** None

**Verification:**
Run: `cargo build -p devices --features snapshot`
Expected: Builds without errors

Run: `cargo test -p devices --features net,snapshot`
Expected: All tests pass including new balloon snapshot tests

Run: `cargo build -p vmm --features snapshot`
Expected: Builds without errors (VMM uses devices with snapshot support)

**Commit:** Not needed if previous tasks committed individually.
<!-- END_TASK_4 -->
