# Memory Balloon Device Implementation Plan — Phase 2: Stats and Free Page Reporting

**Goal:** Complete stats queue, free page hint queue, and enhance existing free page reporting queue with bitmap tracking.

**Architecture:** Add stats queue processing that parses `BalloonStat` entries from guest memory, free page hint (PHQ) processing with command ID protocol (START → page blocks → STOP), and a `signal_config_change()` method on `DeviceState` for config space change notification.

**Tech Stack:** Rust, vm_memory 0.18, virtio balloon spec

**Scope:** 7 phases from original design (phase 2 of 7)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### mem-balloon.AC1: Balloon device processes inflate/deflate and reports stats
- **mem-balloon.AC1.4 Success:** Stats queue returns valid memory counters (at minimum: MemFree, MemTotal, MemAvailable)
- **mem-balloon.AC1.5 Success:** Free page hint queue processes command ID protocol (START → page blocks → STOP) and calls MADV_DONTNEED on reported blocks
- **mem-balloon.AC1.7 Failure:** Stats request before guest driver activates returns None (not panic or stale data)

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->
<!-- START_TASK_1 -->
### Task 1: Add BalloonStat struct, stat tag constants, and BalloonStats type

**Verifies:** None (types prerequisite for AC1.4)

**Files:**
- Modify: `src/devices/src/virtio/balloon/mod.rs` (add stat tag constants to `defs::uapi`, re-export BalloonStats)
- Modify: `src/devices/src/virtio/balloon/device.rs` (add BalloonStat and BalloonStats structs)

**Implementation:**

Add stat tag constants to `defs::uapi` in `mod.rs`, matching the Linux spec at `.reference/linux/include/uapi/linux/virtio_balloon.h:64-80`:

```rust
pub const VIRTIO_BALLOON_S_SWAP_IN: u16 = 0;
pub const VIRTIO_BALLOON_S_SWAP_OUT: u16 = 1;
pub const VIRTIO_BALLOON_S_MAJFLT: u16 = 2;
pub const VIRTIO_BALLOON_S_MINFLT: u16 = 3;
pub const VIRTIO_BALLOON_S_MEMFREE: u16 = 4;
pub const VIRTIO_BALLOON_S_MEMTOT: u16 = 5;
pub const VIRTIO_BALLOON_S_AVAIL: u16 = 6;
pub const VIRTIO_BALLOON_S_CACHES: u16 = 7;
pub const VIRTIO_BALLOON_S_HTLB_PGALLOC: u16 = 8;
pub const VIRTIO_BALLOON_S_HTLB_PGFAIL: u16 = 9;
pub const VIRTIO_BALLOON_S_OOM_KILL: u16 = 10;
pub const VIRTIO_BALLOON_S_ALLOC_STALL: u16 = 11;
pub const VIRTIO_BALLOON_S_ASYNC_SCAN: u16 = 12;
pub const VIRTIO_BALLOON_S_DIRECT_SCAN: u16 = 13;
pub const VIRTIO_BALLOON_S_ASYNC_RECLAIM: u16 = 14;
pub const VIRTIO_BALLOON_S_DIRECT_RECLAIM: u16 = 15;
pub const VIRTIO_BALLOON_S_NR: u16 = 16;
```

Also add command ID constants for free page hinting:
```rust
pub const VIRTIO_BALLOON_CMD_ID_STOP: u32 = 0;
pub const VIRTIO_BALLOON_CMD_ID_DONE: u32 = 1;
```

Add `BalloonStat` struct in `device.rs` — the wire format struct matching the Linux spec (`.reference/linux/include/uapi/linux/virtio_balloon.h:126-129`):
```rust
#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct BalloonStat {
    tag: u16,
    val: u64,
}
// SAFETY: BalloonStat only contains plain data with no padding.
unsafe impl ByteValued for BalloonStat {}
```

Add `BalloonStats` struct — the parsed, public-facing stats container. Use `Option<u64>` fields for each stat to represent presence/absence. Include at minimum: `free_memory`, `total_memory`, `available_memory`. Follow Firecracker's pattern at `.reference/firecracker/src/vmm/src/devices/virtio/balloon/device.rs:141-210`.

Add a `BalloonStats::update_with_stat(&mut self, stat: &BalloonStat)` method that matches the `tag` field against the stat tag constants and updates the corresponding `Option<u64>` field. Unknown tags are silently ignored.

Re-export `BalloonStats` from `mod.rs` for use by the Rust API in Phase 6.

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): add BalloonStat/BalloonStats types and stat tag constants`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add stats fields to Balloon and implement process_stats_queue()

**Verifies:** mem-balloon.AC1.4, mem-balloon.AC1.7

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs:48-55` (add fields to `Balloon` struct)
- Modify: `src/devices/src/virtio/balloon/device.rs` (add `process_stats_queue()`, `request_stats()`, `stats()` methods)

**Implementation:**

Add fields to the `Balloon` struct:
- `stats_desc_index: Option<u16>` — stores the pending stats descriptor index (None until guest sends first buffer)
- `latest_stats: Option<BalloonStats>` — most recently received stats (None until first stats are collected)

Initialize both as `None` in `Balloon::new()`.

**`process_stats_queue(&mut self) -> bool`:**

The stats queue protocol is host-initiated (reference: Firecracker `.reference/firecracker/src/vmm/src/devices/virtio/balloon/device.rs:480-511`):
1. Guest pushes one buffer during init containing current stats
2. Host returns the buffer (via `add_used`) to request new stats
3. Guest fills buffer with fresh stats and re-pushes

Implementation:
1. Extract `mem` from `DeviceState::Activated`
2. Pop descriptor from `queues[STQ_INDEX]`
3. If there's a previous `stats_desc_index`, return it via `add_used` (driver sent extra buffer, per Firecracker's compliance warning)
4. Read `BalloonStat` entries from the descriptor buffer: iterate in steps of `size_of::<BalloonStat>()` (10 bytes), use `mem.read_obj::<BalloonStat>(addr)` for each
5. Update a fresh `BalloonStats` with each stat via `update_with_stat()`
6. Store the result in `self.latest_stats = Some(stats)`
7. Store `self.stats_desc_index = Some(head.index)` — do NOT add_used yet (hold the descriptor for next request)
8. Return `true` if a descriptor was processed

**`request_stats(&mut self)`:**

Used by the Rust API (Phase 6) to trigger a stats collection. If `stats_desc_index` is `Some`, return the buffer to the guest:
1. Get `mem` from device state (return early if inactive)
2. Take `stats_desc_index` via `.take()`
3. Call `queues[STQ_INDEX].queue.add_used(mem, index, 0)`
4. Signal the used queue

If `stats_desc_index` is `None`, log a warning and do nothing (guest hasn't sent a buffer yet, or request already pending).

**`stats(&self) -> Option<&BalloonStats>`:**

Simple getter returning `self.latest_stats.as_ref()`. Returns `None` before activation or before first stats collection (AC1.7).

**Testing:**
Tests must verify each AC listed above:
- mem-balloon.AC1.4: After processing a stats descriptor containing MEMFREE, MEMTOT, AVAIL tags, `stats()` returns `Some` with those fields populated
- mem-balloon.AC1.7: Before activation or before any stats descriptor arrives, `stats()` returns `None`

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): implement stats queue processing`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Update stats event handler

**Verifies:** mem-balloon.AC1.4

**Files:**
- Modify: `src/devices/src/virtio/balloon/event_handler.rs:42-54` (replace `handle_stq_event` stub)

**Implementation:**

Replace the stats queue event handler stub to match the working `handle_frq_event` pattern:
1. Remove `debug!("balloon: stats queue event (ignored)")`
2. Add `debug!("balloon: stats queue event")`
3. After successful queue event read, call `self.process_stats_queue()` and if true, do NOT signal used queue (the stats protocol holds the descriptor — signaling happens in `request_stats` when the host wants new stats)

Note: Unlike inflate/deflate/FRQ, the stats handler does NOT signal the used queue after processing. The stats protocol is host-driven: the host signals when it wants new stats by calling `request_stats()`.

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): wire up stats event handler`
<!-- END_TASK_3 -->
<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 4-6) -->
<!-- START_TASK_4 -->
### Task 4: Add signal_config_change to DeviceState and hinting state to Balloon

**Verifies:** None (infrastructure prerequisite for AC1.5)

**Files:**
- Modify: `src/devices/src/virtio/device.rs:53-61` (add `signal_config_change()` method to `DeviceState`)
- Modify: `src/devices/src/virtio/balloon/device.rs:48-55` (add hinting fields to `Balloon` struct)

**Implementation:**

Add `signal_config_change(&self)` to `DeviceState`, following the exact pattern of the existing `signal_used_queue()` at `src/devices/src/virtio/device.rs:54-61`:
```rust
pub fn signal_config_change(&self) {
    match self {
        Self::Inactive => {
            warn!("DeviceState::signal_config_change() called, but device is not activated")
        }
        Self::Activated(_, ref interrupt) => interrupt.signal_config_change(),
    }
}
```

`InterruptTransport::signal_config_change()` already exists at `src/devices/src/virtio/mmio.rs:181`.

Add hinting fields to the `Balloon` struct:
- `hinting_cmd_counter: u32` — monotonically increasing counter for generating command IDs (starts at 2, since 0=STOP and 1=DONE are reserved)
- `hinting_host_cmd: u32` — the current host-requested command (0=STOP initially)
- `hinting_guest_cmd: Option<u32>` — the last command ID received from the guest

Initialize in `Balloon::new()`: `hinting_cmd_counter: 2`, `hinting_host_cmd: 0`, `hinting_guest_cmd: None`.

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): add signal_config_change to DeviceState and hinting state`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Implement process_phq() with command ID protocol

**Verifies:** mem-balloon.AC1.5

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs` (add `process_phq()` and `start_free_page_hinting()` methods)

**Implementation:**

**`process_phq(&mut self) -> bool`:**

Implement the free page hint command ID protocol. Reference: Firecracker's `process_free_page_hinting_queue()` at `.reference/firecracker/src/vmm/src/devices/virtio/balloon/device.rs:513-593` and the Linux guest driver.

The protocol:
1. Host writes a new `free_page_hint_cmd_id` to config space and signals config change
2. Guest reads the cmd_id and starts sending descriptors to PHQ
3. A 4-byte descriptor contains a command ID (START=matching cmd_id, or STOP=0)
4. Larger descriptors contain memory ranges to release (guest address + length, scatter-gather style like FRQ)

Implementation:
1. Extract `mem` from `DeviceState::Activated`
2. Get mutable reference to `queues[PHQ_INDEX]`
3. Track whether we completed (guest sent STOP)
4. For each descriptor chain from the queue:
   - For each descriptor in the chain (`head.into_iter()`):
     - If `desc.len == 4`: read `u32` cmd_id from guest memory. Update `self.hinting_guest_cmd = Some(cmd)`. If cmd is `VIRTIO_BALLOON_CMD_ID_STOP` (0) or `VIRTIO_BALLOON_CMD_ID_DONE` (1), mark complete.
     - If `desc.len > 4` AND host has an active command (not STOP/DONE) AND guest cmd matches host cmd: call `madvise(MADV_DONTNEED)` on the range using `mem.get_host_address(desc.addr)` and `desc.len` (same pattern as existing `process_frq()`). If host cmd is STOP or DONE, discard in-flight hints (skip the madvise). If guest cmd doesn't match host cmd, skip (stale hints from previous run).
   - Mark descriptor as used
5. If complete: write `VIRTIO_BALLOON_CMD_ID_DONE` to `config.free_page_report_cmd_id`, update `self.hinting_host_cmd = DONE`, signal config change
6. Return `true` if any descriptors were processed

**`start_free_page_hinting(&mut self)`:**

Called by the Rust API (Phase 6) to initiate a hinting run:
1. Generate new cmd_id: `self.hinting_cmd_counter` (then increment counter, wrapping but skipping 0 and 1)
2. Write cmd_id to `self.config.free_page_report_cmd_id`
3. Store in `self.hinting_host_cmd`
4. Signal config change via `self.device_state.signal_config_change()`

**Testing:**
Tests must verify:
- mem-balloon.AC1.5: After processing a PHQ descriptor chain with START cmd_id, page blocks, and STOP, MADV_DONTNEED is called on the reported blocks and the device transitions to DONE

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): implement free page hint queue with command ID protocol`
<!-- END_TASK_5 -->

<!-- START_TASK_6 -->
### Task 6: Update PHQ event handler

**Verifies:** mem-balloon.AC1.5

**Files:**
- Modify: `src/devices/src/virtio/balloon/event_handler.rs:56-68` (replace `handle_phq_event` stub)

**Implementation:**

Replace the PHQ event handler stub to match the `handle_frq_event` pattern:
1. Remove `error!("balloon: unsupported page-hinting queue event")`
2. Add `debug!("balloon: page-hinting queue event")`
3. After successful queue event read, call `self.process_phq()` and if true, call `self.device_state.signal_used_queue()`

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

Run: `cargo build -p devices`
Expected: Full build succeeds

**Commit:** `feat(balloon): wire up free page hint event handler`
<!-- END_TASK_6 -->
<!-- END_SUBCOMPONENT_B -->

<!-- START_TASK_7 -->
### Task 7: Unit tests for stats queue and PHQ processing

**Verifies:** mem-balloon.AC1.4, mem-balloon.AC1.5, mem-balloon.AC1.7

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs` (add or extend `#[cfg(test)] mod tests`)

**Implementation:**

Add unit tests for stats and PHQ functionality.

**Testing:**
Tests must verify each AC:
- mem-balloon.AC1.4: After `process_stats_queue` processes a descriptor containing BalloonStat entries (MEMFREE, MEMTOT, AVAIL tags), `stats()` returns `Some(BalloonStats)` with those fields populated and non-zero.
- mem-balloon.AC1.5: After `process_phq` processes a descriptor chain containing a 4-byte START command ID matching the host command, followed by memory range descriptors, and a STOP command, the device transitions to DONE state (`hinting_host_cmd` becomes CMD_ID_DONE).
- mem-balloon.AC1.7: Before device activation (or before any stats descriptor arrives), `stats()` returns `None` (not panic or stale data).

**Verification:**
Run: `cargo test -p devices --features net -- balloon`
Expected: New stats and PHQ tests pass

**Commit:** `test(balloon): add unit tests for stats queue and PHQ processing`
<!-- END_TASK_7 -->

<!-- START_TASK_8 -->
### Task 8: Verify full phase builds

**Verifies:** None (verification)

**Files:** None

**Verification:**
Run: `cargo build -p devices`
Expected: Builds without errors

Run: `cargo test -p devices --features net`
Expected: All existing tests pass (no regressions)

**Commit:** Not needed if previous tasks committed individually.
<!-- END_TASK_8 -->
