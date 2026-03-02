# Memory Balloon Device Implementation Plan — Phase 1: Balloon Device Core

**Goal:** Complete inflate/deflate queue processing with MADV_DONTNEED and config space handling.

**Architecture:** Extend the existing balloon device skeleton at `src/devices/src/virtio/balloon/` to process inflate and deflate queues. Inflate reads u32 PFN arrays from guest memory, converts to host addresses, and calls `madvise(MADV_DONTNEED)`. Deflate acknowledges returned pages. Config space `write_config` is updated to accept guest writes to the `actual` field.

**Tech Stack:** Rust, vm_memory 0.18, libc (madvise), virtio balloon spec

**Scope:** 7 phases from original design (phase 1 of 7)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### mem-balloon.AC1: Balloon device processes inflate/deflate and reports stats
- **mem-balloon.AC1.1 Success:** Inflate queue processes PFN array and releases host memory via MADV_DONTNEED for each page
- **mem-balloon.AC1.2 Success:** Deflate queue processes PFN array and guest regains access to pages
- **mem-balloon.AC1.3 Success:** Guest writes to `actual` config field (offset 4) and device stores the updated value
- **mem-balloon.AC1.6 Failure:** Inflate with invalid PFN (outside guest memory) is silently skipped without crashing
- **mem-balloon.AC1.8 Edge:** Guest sends duplicate PFN in inflate queue — idempotent (atomic OR on already-set bit, MADV_DONTNEED on already-released page is no-op)

---

<!-- START_SUBCOMPONENT_A (tasks 1-1) -->
<!-- START_TASK_1 -->
### Task 1: Add missing feature flag constants and update AVAIL_FEATURES

**Verifies:** None (infrastructure — feature negotiation prerequisite for AC1.1, AC1.2)

**Files:**
- Modify: `src/devices/src/virtio/balloon/mod.rs:15-21` (add constants to `defs::uapi`)
- Modify: `src/devices/src/virtio/balloon/device.rs:27-30` (update `AVAIL_FEATURES`)

**Implementation:**

The design requires advertising all modern balloon features. Currently `mod.rs` defines only 3 feature flags (STATS_VQ=1, FREE_PAGE_HINT=3, REPORTING=5). Add the 3 missing flags per the Linux header at `.reference/linux/include/uapi/linux/virtio_balloon.h:34-39`:

Add to `defs::uapi` in `mod.rs`:
- `VIRTIO_BALLOON_F_MUST_TELL_HOST: u32 = 0`
- `VIRTIO_BALLOON_F_DEFLATE_ON_OOM: u32 = 2`
- `VIRTIO_BALLOON_F_PAGE_POISON: u32 = 4`

Also add a PFN shift constant needed by inflate/deflate processing:
- `VIRTIO_BALLOON_PFN_SHIFT: u32 = 12`

Update `AVAIL_FEATURES` in `device.rs` to include all 6 feature bits (MUST_TELL_HOST, STATS_VQ, DEFLATE_ON_OOM, FREE_PAGE_HINT, PAGE_POISON, REPORTING) plus VERSION_1.

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): add missing feature flag constants`
<!-- END_TASK_1 -->
<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 2-4) -->
<!-- START_TASK_2 -->
### Task 2: Implement process_inflate() with PFN processing and MADV_DONTNEED

**Verifies:** mem-balloon.AC1.1, mem-balloon.AC1.6, mem-balloon.AC1.8

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs:1-6` (add imports: `GuestAddress`, `Bytes` from `vm_memory`)
- Modify: `src/devices/src/virtio/balloon/device.rs` (add `process_inflate()` method to `impl Balloon` block after `process_frq()` at line 112)

**Implementation:**

Add `process_inflate(&mut self) -> bool` method to the `Balloon` impl block. This method follows the exact same structure as the existing `process_frq()` (line 74-112) but with different descriptor processing:

1. Extract `mem` from `DeviceState::Activated` (same pattern as `process_frq()` line 76-80)
2. Get mutable reference to queues (same pattern as line 82-85)
3. Loop `while let Some(head) = queues[IFQ_INDEX].queue.pop(mem)` (same as line 88)
4. For each descriptor in `head.into_iter()`:
   - The descriptor contains a buffer of `u32` PFN values in guest memory at `desc.addr` with length `desc.len`
   - Iterate through PFNs: `for offset in (0..desc.len).step_by(4)`
   - Read each PFN from guest memory: `mem.read_obj::<u32>(desc.addr.checked_add(offset as u64).unwrap())`
   - Convert PFN to guest physical address: `GuestAddress(u64::from(pfn) << VIRTIO_BALLOON_PFN_SHIFT)`
   - Get host address: `mem.get_host_address(guest_addr)` — if this fails, the PFN is invalid; log a warning and skip (AC1.6)
   - Call `libc::madvise(host_addr as *mut libc::c_void, 4096, libc::MADV_DONTNEED)` — this is idempotent on already-released pages (AC1.8)
5. Mark descriptor as used: `queues[IFQ_INDEX].queue.add_used(mem, index, 0)`
6. Return `true` if any descriptors were processed

Key differences from `process_frq()`:
- FRQ descriptors point directly at memory ranges to release (scatter-gather). Inflate descriptors contain *buffers of u32 PFN values* that must be read from guest memory and converted.
- FRQ uses `desc.addr`/`desc.len` as-is. Inflate reads u32s from the buffer at `desc.addr`.
- Invalid PFNs (outside guest memory) must be silently skipped, not unwrap-panicked.

Reference: Firecracker's `process_inflate()` at `.reference/firecracker/src/vmm/src/devices/virtio/balloon/device.rs:368-459` uses the same PFN-reading pattern but with a compaction buffer for batching madvise calls. For Phase 1, per-PFN madvise is acceptable; contiguous range merging can be added later as an optimization.

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): implement inflate queue PFN processing with MADV_DONTNEED`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Implement process_deflate()

**Verifies:** mem-balloon.AC1.2

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs` (add `process_deflate()` method to `impl Balloon` block after `process_inflate()`)

**Implementation:**

Add `process_deflate(&mut self) -> bool` method. Deflate is simpler than inflate — the host does not need to do anything special when the guest reclaims pages. The guest simply re-accesses the pages and they get faulted back in (the MADV_DONTNEED from inflate makes the kernel allocate fresh zero pages on next access).

Following Firecracker's pattern at `.reference/firecracker/src/vmm/src/devices/virtio/balloon/device.rs:461-478`, deflate just needs to:

1. Extract `mem` from `DeviceState::Activated`
2. Get mutable reference to queues
3. Pop all descriptor chains from `queues[DFQ_INDEX]`
4. Mark each as used via `queue.add_used(mem, index, 0)`
5. Return `true` if any descriptors were processed

No madvise or PFN processing needed — just acknowledge receipt. The guest will fault the pages back in when it accesses them.

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): implement deflate queue processing`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Update event handlers to call inflate/deflate processors

**Verifies:** mem-balloon.AC1.1, mem-balloon.AC1.2

**Files:**
- Modify: `src/devices/src/virtio/balloon/event_handler.rs:14-26` (replace `handle_ifq_event` stub)
- Modify: `src/devices/src/virtio/balloon/event_handler.rs:28-40` (replace `handle_dfq_event` stub)

**Implementation:**

Replace the inflate and deflate event handler stubs to match the working `handle_frq_event` pattern at line 70-84.

For `handle_ifq_event`:
1. Remove the `error!("balloon: unsupported inflate queue event")` line
2. Add `debug!("balloon: inflate queue event")` instead
3. Keep the existing event_set check and queue_event read
4. After successful read, call `self.process_inflate()` and if true, call `self.device_state.signal_used_queue()`

For `handle_dfq_event`:
1. Remove the `error!("balloon: unsupported deflate queue event")` line
2. Add `debug!("balloon: deflate queue event")` instead
3. Keep the existing event_set check and queue_event read
4. After successful read, call `self.process_deflate()` and if true, call `self.device_state.signal_used_queue()`

Follow the exact pattern of `handle_frq_event` (line 70-84):
```
debug log → event_set check → queue_event read → process method → signal_used_queue
```

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): wire up inflate/deflate event handlers`
<!-- END_TASK_4 -->
<!-- END_SUBCOMPONENT_B -->

<!-- START_SUBCOMPONENT_C (tasks 5-5) -->
<!-- START_TASK_5 -->
### Task 5: Implement write_config to handle guest writes to the actual field

**Verifies:** mem-balloon.AC1.3

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs:154-160` (replace `write_config` implementation)

**Implementation:**

Replace the current no-op `write_config` with an implementation that accepts guest writes to the `actual` field.

Per the virtio balloon spec (`.reference/linux/include/uapi/linux/virtio_balloon.h:46-62`), the config space layout is:
- Offset 0: `num_pages` (u32, host-only, read-only to guest)
- Offset 4: `actual` (u32, guest-writable)
- Offset 8: `free_page_hint_cmd_id` (u32, host-only, read-only to guest)
- Offset 12: `poison_val` (u32, host-only, read-only to guest)

Only writes that overlap with the `actual` field (bytes 4..8) should be applied. All other writes are silently ignored.

Implementation approach:
1. Get `config_slice` via `self.config.as_mut_slice()` (ByteValued provides this)
2. For each byte in the write range `offset..offset+data.len()`, if the byte index falls within `4..8`, copy it from `data` to `config_slice`
3. If the write touched the actual field, log the new value at debug level

This handles partial writes correctly (e.g., if the guest writes 2 bytes at offset 6, only those 2 bytes of `actual` are updated).

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): handle guest writes to config space actual field`
<!-- END_TASK_5 -->
<!-- END_SUBCOMPONENT_C -->

<!-- START_TASK_6 -->
### Task 6: Unit tests for inflate, deflate, and write_config

**Verifies:** mem-balloon.AC1.1, mem-balloon.AC1.2, mem-balloon.AC1.3, mem-balloon.AC1.6, mem-balloon.AC1.8

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs` (add `#[cfg(test)] mod tests` block or extend existing)

**Implementation:**

Add unit tests to the balloon device module. Testing inflate/deflate requires a mock guest memory setup with virtio queue descriptors. Follow the existing test patterns in the devices crate (e.g., `src/devices/src/virtio/net/` or other virtio devices that test queue processing).

**Testing:**
Tests must verify each AC:
- mem-balloon.AC1.1: `process_inflate` reads PFN values from a descriptor buffer and calls madvise on the corresponding host addresses. Verify by checking that the function returns `true` (descriptors processed) and the queue used ring is updated.
- mem-balloon.AC1.2: `process_deflate` pops descriptors and marks them as used without error. Verify return value is `true`.
- mem-balloon.AC1.3: `write_config` with offset=4 and 4 bytes updates the `actual` field. `write_config` with offset=0 does NOT change `num_pages`.
- mem-balloon.AC1.6: `process_inflate` with a PFN outside guest memory range is silently skipped (no panic, function still returns `true` for valid PFNs in the same batch).
- mem-balloon.AC1.8: Calling `process_inflate` twice with the same PFN does not panic or error (idempotent madvise).

**Verification:**
Run: `cargo test -p devices --features net -- balloon`
Expected: New tests pass

**Commit:** `test(balloon): add unit tests for inflate, deflate, and write_config`
<!-- END_TASK_6 -->

<!-- START_TASK_7 -->
### Task 7: Verify full phase builds and commit

**Verifies:** None (verification)

**Files:** None (verification only)

**Verification:**
Run: `cargo build -p devices`
Expected: Builds without errors or warnings related to balloon code

Run: `cargo test -p devices --features net`
Expected: All existing tests pass (no regressions)

**Commit:** Not needed if previous tasks committed individually. If not, combine: `feat(balloon): complete inflate/deflate queue processing and config space handling`
<!-- END_TASK_7 -->
