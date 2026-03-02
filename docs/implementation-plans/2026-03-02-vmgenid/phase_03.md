# VMGENID Implementation Plan — Phase 3: VMGENID ACPI Device and GED (x86_64)

**Goal:** Add VMGENID and GED device definitions to the DSDT AML so the Linux vmgenid driver discovers and binds to the device on x86_64.

**Architecture:** The DSDT (currently empty from Phase 1) gets AML bytecode defining two devices: `\_SB.VGEN` (VMGENID, matched by Linux driver via `_CID = "VMGENCTR"`) and `\_SB.GED` (Generic Event Device, `ACPI0013`, delivers interrupt notifications). A dedicated GED IRQ (GSI 16, above the virtio IRQ range of 5-15) is registered via `register_irqfd` with an EventFd stored on the `Vmgenid` struct. The EventFd is triggered during snapshot restore (Phase 5).

**Tech Stack:** Rust, `acpi_tables` 0.2.0 AML generation, KVM irqfd

**Scope:** 6 phases from original design (phase 3 of 6)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vmgenid.AC2: x86_64 ACPI discovery
- **vmgenid.AC2.1 Success:** Kernel finds RSDP in EBDA scan range, parses XSDT → FADT, enters HW-reduced ACPI mode
- **vmgenid.AC2.2 Success:** vmgenid driver binds to `\_SB.VGEN` device via CID `"VMGENCTR"`
- **vmgenid.AC2.3 Success:** vmgenid driver evaluates ADDR method and reads initial GUID from guest memory

---

## Reference Files

- **ACPI module:** `src/arch/src/x86_64/acpi.rs` (Phase 1) — add DSDT AML content
- **Layout constants:** `src/arch/src/x86_64/layout.rs` — `VMGENID_GUID_PAGE`, `VMGENID_GUID_OFFSET`, add `GED_IRQ`
- **acpi_tables AML API:** `acpi_tables::aml::*` — `Device`, `Name`, `Method`, `Return`, `Package`, `Interrupt`, `ResourceTemplate`, `Notify`, `If`, `Arg`, `Path`, `EISAName`
- **IRQ registration:** `src/vmm/src/device_manager/kvm/mmio.rs:153` — `vm.register_irqfd(event_fd, irq)` pattern
- **Builder:** `src/vmm/src/builder.rs` — VM construction, device setup
- **Vmgenid struct:** `src/devices/src/vmgenid/mod.rs` (Phase 2) — add EventFd field, IRQ
- **Linux vmgenid driver reference:** `.reference/linux/` — driver matches on `_CID = "VMGENCTR"`, evaluates `ADDR` method for GUID address

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->

<!-- START_TASK_1 -->
### Task 1: Add GED IRQ constant and EventFd to Vmgenid

**Files:**
- Modify: `src/arch/src/x86_64/layout.rs` (add GED_IRQ constant)
- Modify: `src/devices/src/vmgenid/mod.rs` (add EventFd field, irq field, and accessor)

**Implementation:**

In `src/arch/src/x86_64/layout.rs`, add:
```rust
/// IRQ number for the ACPI Generic Event Device (GED).
/// Allocated above the virtio IRQ range (5-15) on IOAPIC pin 16.
pub const GED_IRQ: u32 = 16;
```

Update the `Vmgenid` struct to hold an EventFd for interrupt injection and the IRQ number:
```rust
use utils::eventfd::EventFd;

pub struct Vmgenid {
    guid_page_addr: u64,
    guid_offset: u64,
    guid: [u8; GUID_SIZE],
    /// EventFd used to inject the GED interrupt via KVM irqfd.
    interrupt_evt: EventFd,
    /// IRQ number registered with KVM for the GED.
    irq: u32,
}
```

Update `Vmgenid::new()` to accept `irq: u32` parameter and create an `EventFd::new(0)`. Add `pub fn interrupt_evt(&self) -> &EventFd` and `pub fn irq(&self) -> u32` accessors.

Add a method to trigger the interrupt:
```rust
pub fn signal_interrupt(&self) -> std::io::Result<()> {
    self.interrupt_evt.write(1)
}
```

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo check -p devices`
Expected: Compiles.

**Commit:** `feat(devices): add EventFd and IRQ to Vmgenid for GED interrupt injection`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Generate VMGENID and GED AML in DSDT

**Files:**
- Modify: `src/arch/src/x86_64/acpi.rs` — update `setup_acpi_tables()` to generate non-empty DSDT with device AML

**Implementation:**

Update the DSDT generation in `setup_acpi_tables()` to include AML bytecode for two devices. The function signature should accept the GUID address and GED IRQ:

```rust
pub fn setup_acpi_tables(guest_mem: &GuestMemoryMmap, guid_addr: u64, ged_irq: u32) -> Result<(), Error>
```

Generate AML for `\_SB.VGEN` (VMGENID device):
```rust
use acpi_tables::aml::*;

let guid_addr_qword = guid_addr; // guid_page_addr + guid_offset

let vgen = Device::new(
    Path::new("\\_SB.VGEN"),
    vec![
        &Name::new(Path::new("_HID"), &"LNRO0003"),     // vendor HID
        &Name::new(Path::new("_CID"), &"VMGENCTR"),      // compatible ID (Linux driver match)
        &Name::new(Path::new("_DDN"), &"VM Generation ID"),
        &Method::new(Path::new("_STA"), 0, false, vec![
            &Return::new(&0xfu8),                         // present, enabled, functioning
        ]),
        &Method::new(Path::new("ADDR"), 0, false, vec![
            &Return::new(&Package::new(vec![
                &guid_addr_qword,                         // physical address of GUID
                &0u64,                                    // high 32 bits (0)
            ])),
        ]),
    ],
);
```

Generate AML for `\_SB.GED` (Generic Event Device):
```rust
let ged_irq_dword = ged_irq;

let ged = Device::new(
    Path::new("\\_SB.GED"),
    vec![
        &Name::new(Path::new("_HID"), &"ACPI0013"),
        &Name::new(Path::new("_CRS"), &ResourceTemplate::new(vec![
            &Interrupt::new(true, true, false, vec![ged_irq]),  // shared, active-high, edge-triggered
        ])),
        &Method::new(Path::new("_EVT"), 1, true, vec![
            // If (Arg0 == ged_irq) { Notify(\_SB.VGEN, 0x80) }
            &If::new(&Equal::new(&Arg(0), &ged_irq_dword), vec![
                &Notify::new(&Path::new("\\_SB.VGEN"), &0x80u8),  // 0x80 = Status Change
            ]),
        ]),
    ],
);
```

Wrap both in a `\_SB` scope:
```rust
let sb_scope = Scope::new(Path::new("\\_SB"), vec![&vgen, &ged]);
```

Serialize the scope to bytes using an `AmlSink` implementation, then pass these bytes as the DSDT definition block. The DSDT is created with this AML data instead of being empty.

Update the caller in `configure_system()` (in `src/arch/src/x86_64/mod.rs`) to pass the GUID address and GED IRQ.

**Important:** The AML code examples above are illustrative pseudocode showing the intended ACPI device topology and structure. The exact `acpi_tables` 0.2.0 API may differ — the implementer MUST verify the actual type signatures, trait bounds, and constructor patterns against the `acpi_tables` crate source (in `~/.cargo/registry/src/` after building). Key areas to verify:
- String handling for `_CID`/`_HID` values: check if `&str` implements `Aml` or if a wrapper type is needed
- Integer types for `Return`, `Arg`, `Equal`: check what types implement `Aml`
- `Package` construction: verify the exact parameter types accepted
- `Scope` vs top-level device placement: verify the correct way to place devices under `\_SB`
The crate's `src/aml.rs` file contains all type definitions and `Aml` trait implementations.

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo check -p arch`
Expected: Compiles. AML types used correctly.

**Commit:** `feat(arch): add VMGENID and GED device definitions to DSDT AML`

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Wire Vmgenid creation and GED IRQ registration in VM builder

**Files:**
- Modify: `src/vmm/src/builder.rs` — create Vmgenid device, register irqfd for GED
- Modify: `src/vmm/src/lib.rs` — add `vmgenid` field to `Vmm` struct
- Modify: `src/arch/src/x86_64/mod.rs` — pass GUID address and GED IRQ to `setup_acpi_tables()`

**Implementation:**

Add `vmgenid: Option<Vmgenid>` field to the `Vmm` struct (in `src/vmm/src/lib.rs`, after the `balloon` field around line 256). Initialize to `None` in the constructor (in `src/vmm/src/builder.rs`, around line 1277).

In the x86_64 VM builder (`src/vmm/src/builder.rs`), after creating the Vmm struct:

1. Create the Vmgenid device:
```rust
#[cfg(target_arch = "x86_64")]
{
    use arch::x86_64::layout::{VMGENID_GUID_PAGE, VMGENID_GUID_OFFSET, GED_IRQ};
    let vmgenid = devices::vmgenid::Vmgenid::new(
        VMGENID_GUID_PAGE, VMGENID_GUID_OFFSET, GED_IRQ, &vmm.guest_memory,
    );
    // Register the GED EventFd with KVM irqchip
    vmm.vm.fd().register_irqfd(vmgenid.interrupt_evt(), GED_IRQ)
        .map_err(StartMicrovmError::RegisterIrqFd)?;
    vmm.vmgenid = Some(vmgenid);
}
```

2. Update `configure_system()` call to pass the GUID address and GED IRQ:
```rust
#[cfg(target_arch = "x86_64")]
arch::x86_64::configure_system(
    &vmm.guest_memory,
    &vmm.arch_memory_info,
    /* ... existing params ... */,
)?;
```

The `configure_system()` function needs to accept and pass through the GUID address and GED IRQ to `acpi::setup_acpi_tables()`. Update its signature or have the ACPI setup use the layout constants directly.

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo build`
Expected: Full workspace builds. Vmgenid is created and GED IRQ registered during x86_64 VM startup.

**Commit:** `feat(vmm): wire Vmgenid creation and GED IRQ registration in x86_64 builder`

<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->
