# VMGENID Implementation Plan — Phase 1: ACPI Table Generation Infrastructure (x86_64)

**Goal:** Add minimal HW-reduced ACPI table generation so the Linux kernel initializes its ACPI subsystem on x86_64 boot.

**Architecture:** Use the `acpi_tables` crate (0.2.0, from crates.io) to generate RSDP, XSDT, FADT, and an empty DSDT. Tables are written to guest memory in the EBDA/ROM scan region (0xE0000–0xFFFFF) which is already outside the E820 RAM entries. The FADT uses HW-reduced ACPI mode (revision 6, bit 20), eliminating the need for GPE/PM register emulation.

**Tech Stack:** Rust, `acpi_tables` 0.2.0 (zerocopy-based, no vm-memory dependency), `vm-memory` 0.18

**Scope:** 6 phases from original design (phase 1 of 6)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vmgenid.AC5: ACPI infrastructure
- **vmgenid.AC5.1 Success:** x86_64 FADT has `HW_REDUCED_ACPI` flag set (bit 20) and revision 6
- **vmgenid.AC5.2 Failure:** GUID page address does not appear in E820 memory map entries

---

## Reference Files

- **Pattern to follow:** `src/arch/src/x86_64/mptable.rs` — binary struct writing to guest memory via `write_obj()`/`write_slice()`
- **Integration point:** `src/arch/src/x86_64/mod.rs:249-336` — `configure_system()` function where ACPI setup will be called
- **Layout constants:** `src/arch/src/x86_64/layout.rs` — address space definitions
- **E820 entries:** `src/arch/src/x86_64/mod.rs:290-328` — current E820 setup (0 to EBDA_START as RAM, HIMEM_START onwards as RAM; region 0xA0000–0xFFFFF is NOT in E820)
- **Arch crate deps:** `src/arch/Cargo.toml` — add `acpi_tables` here
- **`acpi_tables` API reference:** The crate uses `AmlSink` trait for AML bytecode and zerocopy `IntoBytes` for fixed structures. Tables produce `&[u8]` slices written to guest memory via `GuestMemoryMmap::write_slice()`.

---

<!-- START_SUBCOMPONENT_A (tasks 1-4) -->

<!-- START_TASK_1 -->
### Task 1: Add `acpi_tables` dependency

**Files:**
- Modify: `src/arch/Cargo.toml`

**Implementation:**

Add `acpi_tables` as a dependency in the `[dependencies]` section. The crate is `no_std` with only `zerocopy` as a dependency — no vm-memory conflict.

Add after the existing dependencies:

```toml
acpi_tables = "0.2.0"
```

The dependency should be added under `[dependencies]` (not platform-specific), since the crate is `no_std` and platform-agnostic. However, the ACPI module that uses it will be `#[cfg(target_arch = "x86_64")]`.

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo check -p arch`
Expected: Compiles without errors. `acpi_tables` resolves and `zerocopy` is already in the dependency tree.

**Commit:** `chore(arch): add acpi_tables 0.2.0 dependency`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add ACPI and GUID page layout constants

**Files:**
- Modify: `src/arch/src/x86_64/layout.rs` (append after line 66, before the trailing newline)

**Implementation:**

Add constants for the ACPI table region and GUID page address. These addresses are in the 0xA0000–0xFFFFF range, which is already outside E820 RAM entries (E820 covers 0–0x9FC00 and 0x100000+).

```rust
/// Start address for ACPI tables (in EBDA/ROM scan region).
/// The kernel scans 0xE0000–0xFFFFF for the RSDP signature.
pub const ACPI_START: u64 = 0xE0000;
/// Maximum size reserved for ACPI tables (128 KB, to end of scan region).
pub const ACPI_MAX_SIZE: u64 = 0x20000;

/// Guest physical address of the VMGENID GUID page (4 KB).
/// Placed in the ROM expansion region, outside E820 RAM entries.
/// Address space layout (no overlaps):
///   0x9FC00..~0x9FDFF  mptable (a few hundred bytes, scales with vCPU count)
///   0xC0000..0xC0FFF   VMGENID GUID page (4 KB)
///   0xE0000..0xFFFFF   ACPI tables (128 KB max)
pub const VMGENID_GUID_PAGE: u64 = 0xC0000;
/// Offset within the GUID page where the 128-bit GUID is stored.
/// Matches the OVMF SDT Header Probe Suppressor convention (Linux vmgenid
/// driver's ADDR method accounts for this offset).
pub const VMGENID_GUID_OFFSET: u64 = 40;
```

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo check -p arch`
Expected: Compiles without errors.

**Commit:** `feat(arch): add ACPI table and VMGENID GUID page layout constants`

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Create ACPI table generation module

**Files:**
- Create: `src/arch/src/x86_64/acpi.rs`
- Modify: `src/arch/src/x86_64/mod.rs` (add module declaration and import)

**Implementation:**

Create `src/arch/src/x86_64/acpi.rs` that generates the minimal ACPI table chain: RSDP → XSDT → FADT → DSDT (empty). All tables are written contiguously to guest memory starting at `layout::ACPI_START`.

The module should:
1. Define OEM constants (e.g., `OEM_ID: [u8; 6] = *b"LIBKRN"`, `OEM_TABLE_ID: [u8; 8] = *b"KRUNVMGN"`)
2. Implement `setup_acpi_tables(guest_mem: &GuestMemoryMmap) -> Result<(), Error>` that:
   - Creates an empty DSDT (no AML yet — Phase 3 adds device definitions)
   - Creates FADT with `HwReducedAcpi` flag (bit 20), `PwrButton`, `SlpButton` flags, pointing to DSDT via `dsdt_64()`
   - Creates XSDT with FADT address
   - Creates RSDP pointing to XSDT
   - Writes each table to guest memory at sequential addresses starting from `ACPI_START`
   - Uses `GuestMemoryMmap::write_slice()` with bytes from `Rsdp::as_bytes()` (via zerocopy `IntoBytes`) and `Sdt::as_slice()` / `FADT::to_aml_bytes()` (via `AmlSink`)

The `acpi_tables` crate API:
- `Rsdp::new(oem_id, xsdt_addr)` → struct with `IntoBytes`, use `zerocopy::IntoBytes::as_bytes()`
- `XSDT::new(oem_id, oem_table_id, oem_revision)` then `.add_entry(fadt_addr)` → has inner `Sdt` with `.as_slice()`
- `FADTBuilder::new(oem_id, oem_table_id, oem_revision).flag(Flags::HwReducedAcpi).flag(Flags::PwrButton).flag(Flags::SlpButton).dsdt_64(dsdt_addr).finalize()` → `FADT` implements `Aml` trait, serialize via `AmlSink` into a `Vec<u8>`
- For DSDT: use `acpi_tables::sdt::Sdt::new(*b"DSDT", 36, 2, oem_id, oem_table_id, oem_revision)` for an empty DSDT (just the SDT header, no AML body)

Address layout (tables written bottom-up, addresses computed top-down):
1. Compute sizes: DSDT (36 bytes header only), FADT (~276 bytes), XSDT (36 + 8 bytes), RSDP (36 bytes)
2. Assign addresses sequentially from `ACPI_START`: RSDP at `ACPI_START`, then XSDT, then FADT, then DSDT
3. Write in reverse order: DSDT first (address known), then FADT (points to DSDT), then XSDT (points to FADT), then RSDP (points to XSDT)

For converting `FADT` to bytes: implement a simple `Vec<u8>`-based `AmlSink`:
```rust
struct AmlBytes(Vec<u8>);
impl acpi_tables::AmlSink for AmlBytes {
    fn byte(&mut self, byte: u8) {
        self.0.push(byte);
    }
}
```

Add to `src/arch/src/x86_64/mod.rs`:
- Module declaration: `#[cfg(not(feature = "tee"))] mod acpi;` (ACPI is not needed for TEE/SEV/TDX modes, same gate as mptable)
- In the `Error` enum, add: `#[cfg(not(feature = "tee"))] AcpiSetup` variant

In `configure_system()`, add the ACPI setup call right after the mptable setup (line 268):
```rust
#[cfg(not(feature = "tee"))]
acpi::setup_acpi_tables(guest_mem).map_err(|_| Error::AcpiSetup)?;
```

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo check -p arch`
Expected: Compiles without errors. The `acpi_tables` types are used correctly.

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo build`
Expected: Full workspace builds. No link errors.

**Commit:** `feat(arch): add minimal HW-reduced ACPI table generation for x86_64`

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Verify ACPI initialization in guest kernel

**Verifies:** vmgenid.AC5.1, vmgenid.AC5.2

**Files:** None (verification only)

**Testing:**

This is an infrastructure phase. Verification is operational rather than unit-test-based:

1. **Build the full project:**
   Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && make`
   Expected: Builds successfully.

2. **Verify E820 exclusion (vmgenid.AC5.2):** The ACPI table region (0xE0000–0xFFFFF) and GUID page (0xC0000) are already outside E820 RAM entries. The E820 setup in `configure_system()` only adds entries for 0–EBDA_START (0x9FC00) and HIMEM_START (0x100000) onwards. No code changes needed — verify by reading the E820 setup code.

3. **Verify HW-reduced ACPI flag (vmgenid.AC5.1):** The `FADTBuilder` is constructed with `.flag(Flags::HwReducedAcpi)` which sets bit 20. The FADT revision is hardcoded to 6 by the `acpi_tables` crate. This is verified at code review time since it's a constant in the builder chain.

4. **Runtime verification** (manual, if VM is bootable): Boot an x86_64 VM and check:
   - `dmesg | grep -i acpi` should show ACPI initialization
   - `dmesg | grep "HW-reduced"` or `dmesg | grep "Hardware-reduced"` should confirm HW-reduced mode
   - The kernel should NOT print ACPI errors about missing GPE blocks (since HW-reduced eliminates them)

**Commit:** No commit for this task (verification only).

<!-- END_TASK_4 -->

<!-- END_SUBCOMPONENT_A -->
