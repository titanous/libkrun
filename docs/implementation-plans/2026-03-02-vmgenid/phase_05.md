# VMGENID Implementation Plan — Phase 5: Snapshot Restore GUID Update and Interrupt Injection

**Goal:** On snapshot restore, write a new GUID to the guest page and fire the platform-specific interrupt on both x86_64 and aarch64.

**Architecture:** After devices are restored and re-activated but before vCPUs resume execution, the VMM calls `vmgenid.update_guid(mem)` to write a fresh GUID to the guest page, then `vmgenid.signal_interrupt()` to trigger the EventFd registered with KVM. On x86_64 this fires the GED IOAPIC IRQ; on aarch64 this fires the GIC SPI. The guest kernel's vmgenid driver detects the GUID change and calls `add_vmfork_randomness()`.

**Tech Stack:** Rust, KVM irqfd (EventFd trigger), snapshot feature gate

**Scope:** 6 phases from original design (phase 5 of 6)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vmgenid.AC4: Snapshot restore triggers reseed
- **vmgenid.AC4.1 Success:** After restore, VMM writes a new GUID to the guest page before vCPUs resume
- **vmgenid.AC4.2 Success:** VMM injects the platform-specific interrupt (GED on x86_64, SPI on aarch64) after writing the GUID
- **vmgenid.AC4.3 Success:** Guest kernel logs `"crng reseeded due to virtual machine fork"` after restore

---

## Reference Files

- **Restore flow:** `src/vmm/src/lib.rs:444-494` — `restore_device_and_vcpu_states()`: GIC restore (line 448) → VM state (line 453) → MMIO device restore (line 462) → PortIO restore (line 470) → complete restores (line 481) → resume workers (line 488) → restore vCPUs (line 490)
- **All three restore entry points:** `restore_snapshot()` (line 498), `restore_from_store()` (line 527), `restore_from_store_with_uffd()` (line 605)
- **Vmm struct:** `src/vmm/src/lib.rs:216-257` — has `vmgenid: Option<Vmgenid>` (from Phase 3)
- **Vmgenid device:** `src/devices/src/vmgenid/mod.rs` — `update_guid()`, `signal_interrupt()`
- **EventFd trigger:** `utils::eventfd::EventFd::write(1)` — KVM sees irqfd trigger and injects interrupt

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Add VMGENID update to restore flow

**Files:**
- Modify: `src/vmm/src/lib.rs` — add VMGENID GUID update + interrupt injection after device restore

**Implementation:**

In `restore_device_and_vcpu_states()` (around line 488, after `resume_all_device_workers()` and before `restore_vcpu_states()`), add:

```rust
// Update VMGENID: write new GUID and fire interrupt before vCPUs resume.
if let Some(ref mut vmgenid) = self.vmgenid {
    let (old, new) = vmgenid.update_guid(&self.guest_memory);
    log::info!(
        "vmgenid: updated GUID from {:02x?} to {:02x?}",
        &old[..4], &new[..4]
    );
    vmgenid.signal_interrupt().map_err(|e| {
        snapshot::SnapshotError::RestoreDeviceState(format!(
            "vmgenid interrupt injection failed: {}", e
        ))
    })?;
}
```

This placement ensures:
1. Interrupt controller state is already restored (GIC on aarch64, IOAPIC on x86_64)
2. The irqfd registration from boot is still active (KVM preserves irqfd across snapshot restore)
3. Device workers are resumed (they can process the interrupt)
4. vCPU state is about to be loaded but vCPUs are not yet running

The `signal_interrupt()` writes to the EventFd, which KVM sees via the irqfd registration and injects the appropriate interrupt (GED IRQ on x86_64, SPI on aarch64). The interrupt will be pending when vCPUs resume.

**Important considerations:**

1. **irqfd persistence:** Verify that KVM preserves irqfd registrations across the restore flow. If `restore_from_store()` or `restore_from_store_with_uffd()` recreate the VM or reset KVM state, the irqfd may need to be re-registered. Check if the VM fd is the same object before and after restore — if yes, irqfd persists. If the VM is recreated, the Vmgenid device must re-register its irqfd after the new VM is created. Looking at the restore flow: libkrun restores state into the SAME Vmm/VM instance (not a new one), so KVM irqfd registrations from boot persist. No re-registration needed.

2. **UFFD path and GUID page write safety:** In the `restore_from_store_with_uffd()` path, `update_guid()` calls `GuestMemoryMmap::write_slice()` to write the new GUID. On x86_64, the GUID page at 0xC0000 is within the low memory region (GuestMemoryMmap covers from address 0) but outside E820 RAM — the UFFD handler should only register DRAM pages with userfaultfd, not the reserved I/O region. On aarch64, the GUID page at 0x0800_0000 is a small separate KVM memory slot added by `arch_memory_regions()` (Phase 4). The implementer must verify this small region is NOT registered with userfaultfd in the UFFD restore path — if the UFFD handler registers ALL `GuestMemoryMmap` regions, the GUID page write could trigger a fault before the handler is ready. If this is a concern, populate the GUID page region eagerly (not demand-paged) before the UFFD handler starts.

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo build --features snapshot`
Expected: Builds with snapshot feature.

**Commit:** `feat(vmm): trigger VMGENID GUID update and interrupt on snapshot restore`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Handle Vmgenid recreation on full snapshot restore

**Files:**
- Modify: `src/vmm/src/lib.rs` (if needed)

**Implementation:**

For `restore_snapshot()` (the legacy full-file restore path), verify the Vmgenid is handled correctly:

1. The Vmgenid is created during initial VM construction (boot) and stored in `vmm.vmgenid`.
2. On restore, the same Vmm instance is reused — `vmgenid` field persists.
3. `update_guid()` writes to `self.guest_memory` which has the restored memory contents.
4. The old GUID in guest memory (from the snapshot) is overwritten with a new GUID.
5. The irqfd EventFd is still registered with KVM.

If the `restore_snapshot()` path loads memory from a file (overwriting the guest page with the snapshot's GUID), the `update_guid()` call correctly overwrites it again with a fresh GUID. The sequence is:
- Memory loaded from snapshot → old GUID in page
- `update_guid()` → new GUID written to page
- Interrupt fired → guest detects change

No additional code changes should be needed, but verify the flow works by reading through each restore path.

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo build --features snapshot`
Expected: Builds.

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo build --features uffd`
Expected: Builds with uffd feature (implies snapshot).

**Commit:** No commit if no code changes needed (verification only). If changes are needed, commit with appropriate message.

<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->
