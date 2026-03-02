# VMGENID Implementation Plan — Phase 4: VMGENID Device Tree Node (aarch64)

**Goal:** Add a VMGENID device tree node so the Linux vmgenid driver discovers the device on aarch64.

**Architecture:** A new `vmgenid@{addr}` FDT node is added with `compatible = "microsoft,vmgenid"`, a `reg` property pointing to the GUID page, and an `interrupts` property declaring a GIC SPI. The aarch64 builder allocates a fixed SPI IRQ, registers an irqfd EventFd with KVM, and adds device info to the FDT generation map. A `Vmgenid` DeviceType variant routes the FDT dispatch to a new `create_vmgenid_node()` function.

**Tech Stack:** Rust, `vm-fdt::FdtWriter`, KVM irqfd, GIC SPI

**Scope:** 6 phases from original design (phase 4 of 6)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vmgenid.AC3: aarch64 device tree discovery
- **vmgenid.AC3.1 Success:** FDT contains vmgenid node with `compatible = "microsoft,vmgenid"`, `reg`, and `interrupts` properties
- **vmgenid.AC3.2 Success:** vmgenid driver binds via device tree compatible match
- **vmgenid.AC3.3 Success:** vmgenid driver reads initial GUID from `reg` address and registers IRQ handler

---

## Reference Files

- **FDT generation:** `src/devices/src/fdt/aarch64.rs` — `create_fdt()` (line 72), `create_devices_node()` (line 419), existing device node patterns (Serial line 331, RTC line 357, GPIO line 378)
- **DeviceType enum:** `src/devices/src/lib.rs:41-53` — add `Vmgenid` variant
- **DeviceInfoForFDT trait:** `src/devices/src/fdt/aarch64.rs:44-51` — `addr()`, `irq()`, `length()`
- **MMIODeviceInfo:** `src/vmm/src/device_manager/kvm/mmio.rs:480-497` — implements DeviceInfoForFDT
- **aarch64 layout:** `src/arch/src/aarch64/layout.rs` — `IRQ_BASE = 32`, `IRQ_MAX = 159`
- **Interrupt constants:** `src/devices/src/fdt/aarch64.rs:33-41` — `GIC_FDT_IRQ_TYPE_SPI`, `IRQ_TYPE_EDGE_RISING`
- **FDT interrupt platform split:** `src/devices/src/fdt/aarch64.rs:312-319` — existing nodes use `#[cfg(target_os)]` split: Linux uses `dev_info.irq()` directly, macOS uses `dev_info.irq() - 32`
- **GIC configuration:** `src/devices/src/legacy/kvmgicv2.rs:60`, `src/devices/src/legacy/kvmgicv3.rs:115` — `nr_irqs = IRQ_MAX - IRQ_BASE + 1`
- **Builder:** `src/vmm/src/builder.rs` — aarch64 device setup and FDT generation
- **MMIODeviceManager:** `src/vmm/src/device_manager/kvm/mmio.rs:480-497` — `id_to_dev_info` is private, `MMIODeviceInfo` fields are private
- **aarch64 memory regions:** `src/arch/src/aarch64/mod.rs:46-90` — `arch_memory_regions()` returns `Vec<(GuestAddress, usize)>`. For kernel boot, only creates one region at `DRAM_MEM_START_KERNEL = 0x8000_0000`. The GUID page at `0x0800_0000` is BELOW this — must be added as a separate memory region.
- **GIC snapshot:** `src/devices/src/legacy/kvmgicv3.rs:216-220` (save) and lines 317-319 (restore) — use `IRQ_MAX - IRQ_BASE + 1` for nr_irqs independently of GIC init

---

<!-- START_SUBCOMPONENT_A (tasks 1-4) -->

<!-- START_TASK_1 -->
### Task 1: Add Vmgenid variant to DeviceType and aarch64 layout constant

**Files:**
- Modify: `src/devices/src/lib.rs` — add `Vmgenid` variant to `DeviceType` enum
- Modify: `src/arch/src/aarch64/layout.rs` — add GUID page address constant and GIC_NR_IRQS
- Modify: `src/arch/src/aarch64/mod.rs` — add GUID page memory region to `arch_memory_regions()`
- Modify: `src/devices/src/legacy/kvmgicv2.rs` — use GIC_NR_IRQS for nr_irqs
- Modify: `src/devices/src/legacy/kvmgicv3.rs` — use GIC_NR_IRQS for nr_irqs (init, save, and restore)

**Implementation:**

In `src/devices/src/lib.rs`, add to the `DeviceType` enum:
```rust
pub enum DeviceType {
    Virtio(u32),
    #[cfg(target_arch = "aarch64")]
    Gpio,
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    Serial,
    #[cfg(target_arch = "aarch64")]
    RTC,
    /// VMGENID platform device (not a virtio device).
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    Vmgenid,
}
```

In `src/arch/src/aarch64/layout.rs`, add:
```rust
/// Guest physical address of the VMGENID GUID page (4 KB).
/// Placed well below the GIC redistributor region (which grows downward
/// from 0x09FF_0000) to avoid conflicts at any vCPU count.
/// GICv3 redists reach 0x09FF_0000 - (0x20000 * vcpu_count); at 256 vCPUs
/// they'd reach 0x07FF_0000. Address 0x0800_0000 is safe for up to ~255 vCPUs.
/// This address is below DRAM start (0x8000_0000 for kernel boot, 0x4000_0000
/// for EFI boot), so it is NOT in guest RAM and not registered with UFFD.
pub const VMGENID_GUID_PAGE: u64 = 0x0800_0000;
/// Offset within the GUID page where the 128-bit GUID is stored.
pub const VMGENID_GUID_OFFSET: u64 = 40;
/// Fixed GIC SPI number for the VMGENID interrupt.
/// Allocated above the dynamic virtio SPI range (IRQ_BASE..IRQ_MAX = 32..159)
/// to avoid conflicts with virtio device allocations.
pub const VMGENID_SPI: u32 = 160;

/// Total number of interrupts to configure on the KVM GIC.
/// Must be a multiple of 32 (KVM requirement). Covers the dynamic virtio
/// range (SPIs 0-127, INTID 32-159) plus platform-reserved SPIs like
/// VMGENID_SPI (INTID 160). Value 192 supports INTIDs 0-191.
pub const GIC_NR_IRQS: u32 = 192;
```

Note: The dynamic virtio allocator range (IRQ_BASE..IRQ_MAX = 32..159) is unchanged. VMGENID_SPI=160 is a fixed platform SPI above this range. The GIC `nr_irqs` must be increased from the current `IRQ_MAX - IRQ_BASE + 1 = 128` to `GIC_NR_IRQS = 192` to cover INTID 160.

Additionally, update the GIC configuration in both GIC implementations to use the new `GIC_NR_IRQS` constant instead of computing `IRQ_MAX - IRQ_BASE + 1`:

In `src/devices/src/legacy/kvmgicv2.rs` (line 60), change:
```rust
// Before:
let nr_irqs: u32 = arch::aarch64::layout::IRQ_MAX - arch::aarch64::layout::IRQ_BASE + 1;
// After:
let nr_irqs: u32 = arch::aarch64::layout::GIC_NR_IRQS;
```

In `src/devices/src/legacy/kvmgicv3.rs` (line 115), change:
```rust
// Before:
let nr_irqs: u32 = arch::aarch64::layout::IRQ_MAX - arch::aarch64::layout::IRQ_BASE + 1;
// After:
let nr_irqs: u32 = arch::aarch64::layout::GIC_NR_IRQS;
```

This increases the GIC interrupt capacity from 128 to 192, covering VMGENID_SPI at INTID 160.

Also update `save_gic_state()` (line ~216-220) and `restore_gic_state()` (line ~317-319) in `kvmgicv3.rs` to use `GIC_NR_IRQS` instead of computing `IRQ_MAX - IRQ_BASE + 1`. This ensures GIC snapshot/restore covers the full interrupt range including platform-reserved SPIs.

**GUID page memory region:** On aarch64, `GuestMemoryMmap` is created from `arch_memory_regions()` which starts at `DRAM_MEM_START_KERNEL = 0x8000_0000` for kernel boot. The GUID page at `0x0800_0000` is below DRAM start and NOT in any guest memory region — `GuestMemoryMmap::write_slice()` would fail.

Add the GUID page as a separate small memory region in `src/arch/src/aarch64/mod.rs`, in `arch_memory_regions()`, before the DRAM region:

```rust
// VMGENID GUID page: 4KB region below DRAM, used by the VMGENID device to
// store the 128-bit VM Generation ID. This is a separate KVM memory slot
// from DRAM — the guest kernel does not see it as usable RAM (it's not in
// the FDT /memory node). Not registered with UFFD for demand-paging.
regions.push((GuestAddress(layout::VMGENID_GUID_PAGE), 0x1000));
```

This creates a separate 4KB KVM memory slot for the GUID page. It is NOT reported to the guest kernel as RAM (the FDT `/memory` node only covers the DRAM region starting at 0x8000_0000), so the guest cannot use it for general allocation. The `Vmgenid::new()` call in the builder (Task 3) will write the initial GUID to this region via `write_slice()`.

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo check -p devices`
Expected: Compiles.

**Commit:** `feat(devices): add Vmgenid DeviceType variant, aarch64 layout constants, and GIC nr_irqs increase`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Create vmgenid FDT node generation function

**Files:**
- Modify: `src/devices/src/fdt/aarch64.rs` — add `create_vmgenid_node()` and dispatch from `create_devices_node()`

**Implementation:**

Add a new function following the RTC/Serial/GPIO node patterns:

```rust
fn create_vmgenid_node<T: DeviceInfoForFDT + Clone + Debug>(
    fdt: &mut FdtWriter,
    dev_info: &T,
) -> Result<()> {
    let vmgenid_node = fdt.begin_node(&format!("vmgenid@{:x}", dev_info.addr()))?;
    fdt.property_string("compatible", "microsoft,vmgenid")?;

    // reg = <addr 0x1000> (64-bit address, 64-bit size)
    let reg = generate_prop64(&[dev_info.addr(), dev_info.length()]);
    fdt.property("reg", &reg)?;

    // interrupts = <GIC_SPI irq_num IRQ_TYPE_EDGE_RISING>
    // Platform split: Linux uses GSI directly, macOS subtracts 32 (SPI offset).
    // This matches the pattern in create_virtio_node, create_serial_node, etc.
    #[cfg(target_os = "linux")]
    let irq = generate_prop32(&[GIC_FDT_IRQ_TYPE_SPI, dev_info.irq(), IRQ_TYPE_EDGE_RISING]);
    #[cfg(target_os = "macos")]
    let irq = generate_prop32(&[GIC_FDT_IRQ_TYPE_SPI, dev_info.irq() - 32, IRQ_TYPE_EDGE_RISING]);
    fdt.property("interrupts", &irq)?;

    fdt.end_node(vmgenid_node)?;
    Ok(())
}
```

In `create_devices_node()` (around line 427), add dispatch:
```rust
DeviceType::Vmgenid => create_vmgenid_node(fdt, info)?,
```

Check whether `generate_prop64` exists or if you need to use `generate_prop32` with split high/low words for 64-bit values. The existing code uses `fdt.property_u64()` for some nodes — use whichever pattern matches the `reg` property format required by the `#address-cells` and `#size-cells` of the parent node.

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo check -p devices --target aarch64-unknown-linux-gnu`
Expected: Compiles (cross-check; may need target installed).

Fallback: `cargo check -p devices` (will check non-aarch64 gated code only, but validates syntax).

**Commit:** `feat(fdt): add vmgenid device tree node generation for aarch64`

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Wire Vmgenid creation and SPI registration in aarch64 builder

**Files:**
- Modify: `src/vmm/src/builder.rs` — create Vmgenid, register irqfd, add to device info map

**Implementation:**

In the aarch64 VM builder, after creating the Vmm struct and attaching legacy devices:

1. Create the Vmgenid device:
```rust
#[cfg(target_arch = "aarch64")]
{
    use arch::aarch64::layout::{VMGENID_GUID_PAGE, VMGENID_GUID_OFFSET, VMGENID_SPI};
    let vmgenid = devices::vmgenid::Vmgenid::new(
        VMGENID_GUID_PAGE, VMGENID_GUID_OFFSET, VMGENID_SPI, &vmm.guest_memory,
    );
    // Register irqfd: SPI number maps to KVM GSI (SPI_num + 32 offset may apply
    // depending on KVM GIC implementation — verify with existing IRQ registration pattern)
    vmm.vm.fd().register_irqfd(vmgenid.interrupt_evt(), VMGENID_SPI)
        .map_err(StartMicrovmError::RegisterIrqFd)?;
    vmm.vmgenid = Some(vmgenid);
}
```

2. The `MMIODeviceManager::id_to_dev_info` HashMap is private and `MMIODeviceInfo` has private fields with no public constructor. Add a public method to `MMIODeviceManager` for registering platform device info (devices that aren't on the MMIO bus but need FDT entries):

In `src/vmm/src/device_manager/kvm/mmio.rs`, add to `MMIODeviceManager`:
```rust
/// Register a platform device's info for FDT generation.
/// Unlike MMIO bus devices, platform devices have fixed addresses
/// and don't use the MMIO bus or dynamic IRQ allocation.
pub fn register_platform_device_info(
    &mut self,
    type_id: (DeviceType, String),
    addr: u64,
    irq: u32,
    len: u64,
) {
    self.id_to_dev_info.insert(type_id, MMIODeviceInfo { addr, _irq: irq, _len: len });
}
```

If `MMIODeviceInfo` fields use different names (check the struct definition at line 480-497), adjust accordingly. The key requirement is exposing a way to insert device info without going through the MMIO bus allocation path.

Then in the builder, call:
```rust
mmio_device_manager.register_platform_device_info(
    (DeviceType::Vmgenid, "vmgenid".to_string()),
    VMGENID_GUID_PAGE,
    VMGENID_SPI,
    0x1000,
);
```

The device info's `addr` = GUID page address, `irq` = SPI number, `length` = 0x1000 (4KB page). These are used by the FDT generator to produce the `reg` and `interrupts` properties.

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo build`
Expected: Full workspace builds.

**Commit:** `feat(vmm): wire Vmgenid creation and SPI registration in aarch64 builder`

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Verify aarch64 FDT generation (build verification)

**Verifies:** vmgenid.AC3.1, vmgenid.AC3.2, vmgenid.AC3.3

**Files:** None (verification only)

**Verification:**

1. Full build: `cargo build`
2. Runtime verification (manual, if aarch64 VM bootable): boot VM and check `dmesg | grep vmgenid` shows driver binding via device tree compatible match.

These ACs are verified at integration test time (Phase 6) and by code review of the FDT node structure matching the Linux vmgenid driver's expected compatible string and property layout.

**Commit:** No commit (verification only).

<!-- END_TASK_4 -->

<!-- END_SUBCOMPONENT_A -->
