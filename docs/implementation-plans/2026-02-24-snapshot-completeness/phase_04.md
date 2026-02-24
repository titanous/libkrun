# Snapshot Completeness Implementation Plan — Phase 4

**Goal:** Used ring pages are marked dirty after snapshot save for incremental snapshot correctness.

**Architecture:** Add a method to `MMIODeviceManager` that collects page-aligned GPA ranges of all active virtio queue used rings. On Linux, merge those pages into the dirty page set after `collect_dirty_pages()` since KVM's dirty log misses host-side writes. On macOS, mark them in the userspace `DirtyBitmap` before `drain_dirty_pages()`. Full snapshots dump all memory so this only matters for incremental, but calling unconditionally is simpler per design.

**Tech Stack:** Rust (vmm crate, devices crate, KVM dirty log API)

**Scope:** 7 phases from original design (this is phase 4 of 7)

**Codebase verified:** 2026-02-24

---

## Acceptance Criteria Coverage

This phase implements and tests:

### snapshot-completeness.AC2: Virtio queue memory marked dirty
- **snapshot-completeness.AC2.1 Success:** After snapshot save, used ring pages for all active virtio queues are marked dirty in KVM dirty bitmap
- **snapshot-completeness.AC2.2 Success:** Incremental snapshot after virtio I/O includes used ring pages in dirty page set
- **snapshot-completeness.AC2.3 Edge:** Inactive/unactivated queues are not marked (no crash on queues with zero addresses)

---

<!-- START_TASK_1 -->
### Task 1: Add method to collect virtio used ring page ranges

**Verifies:** snapshot-completeness.AC2.1, snapshot-completeness.AC2.3

**Files:**
- Modify: `src/devices/src/virtio/mmio.rs` (add public method to `impl MmioTransport`)
- Modify: `src/vmm/src/device_manager/kvm/mmio.rs` (add method to `impl MMIODeviceManager`)

**Implementation:**

**Important context:** After device activation, `MmioTransport.queues` is `None` — queues are moved to the device backend via `self.queues.take()` in `activate()` (mmio.rs:360). Queue GPAs are accessible after activation via the `VirtioDevice::queues()` trait method on the inner device (`self.device`). This is the same mechanism used by `MmioTransport::save_state()` to capture `queue_states`.

**Step 1: Add accessor on MmioTransport** (`src/devices/src/virtio/mmio.rs`):

Add a `#[cfg(feature = "snapshot")]` public method to `MmioTransport` that retrieves used ring page ranges by querying the inner `VirtioDevice`:

```rust
#[cfg(feature = "snapshot")]
pub fn get_used_ring_ranges(&self) -> Vec<(u64, u64)> {
    let page_size = 4096u64;
    let mut ranges = Vec::new();

    // Access queues via VirtioDevice::queues() on the inner device.
    // After activation, transport.queues is None (moved to backend),
    // but device.queues() returns the live queue state.
    let Ok(device) = self.device.lock() else {
        return ranges;
    };

    for queue in device.queues() {
        if !queue.ready || queue.used_ring.raw_value() == 0 {
            continue; // AC2.3: skip inactive queues
        }

        let used_ring_addr = queue.used_ring.raw_value();
        let used_ring_size = 6 + 8 * queue.size as u64;

        // Page-align the range
        let start_page = used_ring_addr & !(page_size - 1);
        let end = used_ring_addr + used_ring_size;
        let end_page = (end + page_size - 1) & !(page_size - 1);

        let mut page = start_page;
        while page < end_page {
            ranges.push((page, page_size));
            page += page_size;
        }
    }

    ranges
}
```

Note: `VirtioDevice::queues()` returns `&[Queue]` (default `&[]` for devices that don't override it). Devices that support snapshots (Console, Block, Net, Vsock) override this method to return their queue state. Unactivated devices return empty — the `ready` and `used_ring == 0` checks handle any remaining edge cases.

**Step 2: Add collector on MMIODeviceManager** (`src/vmm/src/device_manager/kvm/mmio.rs`):

Add a `#[cfg(feature = "snapshot")]` method that iterates all registered devices, downcasts to `MmioTransport`, and delegates to the accessor:

```rust
#[cfg(feature = "snapshot")]
pub fn get_virtio_used_ring_ranges(&self) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();

    for ((_device_type, _device_id), dev_info) in &self.id_to_dev_info {
        let Some((_, device)) = self.bus.get_device(dev_info.addr) else {
            continue;
        };
        let Ok(device) = device.lock() else {
            continue;
        };

        // Downcast BusDevice to MmioTransport
        let Some(transport) = device.as_any().downcast_ref::<MmioTransport>() else {
            continue;
        };

        ranges.extend(transport.get_used_ring_ranges());
    }

    ranges
}
```

The `AsAny` trait is already required by `BusDevice` (`pub trait BusDevice: AsAny + Send`). `MmioTransport` implements `BusDevice` and thus `AsAny`.

**Verification:**

Build: `cargo build -p vmm --features snapshot`

**Commit:** Do not commit yet — continue to Task 2.
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Integrate used ring dirty marking into incremental snapshot (Linux)

**Verifies:** snapshot-completeness.AC2.2

**Files:**
- Modify: `src/vmm/src/lib.rs:958-1002` (Linux `collect_dirty_pages`)
- Or modify: `src/vmm/src/lib.rs:1040-1057` (Linux `create_incremental_snapshot`, after `collect_dirty_pages`)

**Implementation:**

On Linux, KVM's `get_dirty_log` only tracks guest writes. Host-side writes to used ring pages (from `add_used()`, `set_notification()`) are invisible to KVM. After collecting the KVM dirty log, merge the used ring pages into the dirty set.

In `create_incremental_snapshot()`, after `let dirty_pages = self.collect_dirty_pages()?;`:

```rust
// Mark virtio used ring pages dirty (host writes not tracked by KVM)
let used_ring_ranges = self.mmio_device_manager.get_virtio_used_ring_ranges();
for (page_addr, page_size) in &used_ring_ranges {
    // Check if this page is already in the dirty set
    if !dirty_pages.iter().any(|p| p.guest_addr == *page_addr) {
        let host_ptr = self
            .guest_memory
            .get_host_address(vm_memory::GuestAddress(*page_addr))
            .map_err(|e| {
                snapshot::SnapshotError::Serialize(format!(
                    "Invalid guest address for used ring page 0x{page_addr:x}: {e}"
                ))
            })?;
        let data = unsafe {
            std::slice::from_raw_parts(host_ptr, *page_size as usize)
        };
        dirty_pages.push(snapshot::DirtyPage {
            guest_addr: *page_addr,
            data: data.to_vec(),
        });
    }
}
```

Note: `dirty_pages` needs to be `let mut dirty_pages`.

For macOS incremental snapshots (`src/vmm/src/lib.rs:785-865`), mark the pages in the userspace DirtyBitmap before `drain_dirty_pages()`:

```rust
// Mark virtio used ring pages dirty (host writes not tracked by memory protection)
let used_ring_ranges = self.mmio_device_manager.get_virtio_used_ring_ranges();
for (page_addr, _) in &used_ring_ranges {
    for bitmap in &self.dirty_bitmaps {
        bitmap.mark_dirty(*page_addr);
    }
}
```

For full snapshots (both platforms): No changes needed — full snapshots dump all memory.

**Verification:**

Build: `cargo build -p vmm --features snapshot`

Run: `cargo test -p vmm --features snapshot`

**Commit:** `feat: mark virtio used ring pages dirty for incremental snapshots`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Unit tests for virtio queue dirty marking

**Verifies:** snapshot-completeness.AC2.1, snapshot-completeness.AC2.3

**Files:**
- Modify: `src/vmm/src/device_manager/kvm/mmio.rs` (add to test module)

**Implementation:**

Add `#[cfg(feature = "snapshot")]` tests:

**Testing:**

Tests must verify:
- snapshot-completeness.AC2.1: Create MMIODeviceManager with a mock virtio device that has activated queues with known used_ring addresses. Call `get_virtio_used_ring_ranges()`. Verify returned ranges include the correct page-aligned addresses for each active queue's used ring.
- snapshot-completeness.AC2.3: Include a device with unactivated queues (ready=false, used_ring=0). Verify `get_virtio_used_ring_ranges()` returns no entries for those queues and does not crash.

Run: `cargo test -p vmm --features snapshot`

Expected: All tests pass.

**Commit:** `test: verify virtio used ring dirty marking`
<!-- END_TASK_3 -->
