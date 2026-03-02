# VMGENID Implementation Plan — Phase 2: Vmgenid Device Model

**Goal:** Shared device model that manages the 128-bit GUID and writes it to guest memory.

**Architecture:** A `Vmgenid` struct in `src/devices/src/vmgenid/` owns a GUID and the guest physical address of the GUID page. On construction it generates a random GUID and writes it to guest memory at the configured offset. `update_guid()` generates a new GUID, writes it, and returns old/new values. The struct is NOT a virtio device — it's a platform device that bypasses MmioTransport.

**Tech Stack:** Rust, `getrandom` (already transitive dep), `vm-memory` 0.18

**Scope:** 6 phases from original design (phase 2 of 6)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vmgenid.AC1: GUID lifecycle
- **vmgenid.AC1.1 Success:** Fresh random 128-bit GUID is generated and written to guest page at offset 40 on VM creation
- **vmgenid.AC1.2 Success:** `update_guid()` produces a different GUID each invocation and writes it to the correct guest physical address

---

## Reference Files

- **Device module pattern:** `src/devices/src/lib.rs` — module declarations (add `pub mod vmgenid;`)
- **Guest memory API:** `vm_memory::{Bytes, GuestAddress, GuestMemoryMmap}` — `write_slice()` for byte writes
- **Layout constants (reference only):** `src/arch/src/x86_64/layout.rs` — `VMGENID_GUID_PAGE`, `VMGENID_GUID_OFFSET` (from Phase 1); aarch64 equivalents added in Phase 4. The Vmgenid struct is arch-agnostic — it accepts address parameters, so these constants are only used by arch-specific callers (builder.rs).
- **Existing device pattern:** `src/devices/src/virtio/balloon/device.rs` — struct organization, ByteValued, imports
- **Randomness:** `getrandom::getrandom()` — already in Cargo.lock as transitive dependency

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->

<!-- START_TASK_1 -->
### Task 1: Create vmgenid device module

**Files:**
- Create: `src/devices/src/vmgenid/mod.rs`
- Modify: `src/devices/src/lib.rs` (add module declaration)

**Implementation:**

Create `src/devices/src/vmgenid/mod.rs` with the `Vmgenid` struct:

```rust
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

/// Size of a GUID in bytes (128-bit).
const GUID_SIZE: usize = 16;

pub struct Vmgenid {
    /// Guest physical address of the 4KB GUID page.
    guid_page_addr: u64,
    /// Byte offset within the page where the GUID is stored.
    guid_offset: u64,
    /// Current GUID value.
    guid: [u8; GUID_SIZE],
}
```

Implement:

- `Vmgenid::new(guid_page_addr: u64, guid_offset: u64, mem: &GuestMemoryMmap) -> Self` — generates a random GUID via `getrandom::getrandom()`, writes it to `mem` at `GuestAddress(guid_page_addr + guid_offset)`, returns the struct.
- `Vmgenid::update_guid(&mut self, mem: &GuestMemoryMmap) -> ([u8; 16], [u8; 16])` — generates a new GUID, writes it to guest memory at the same address, returns `(old_guid, new_guid)`.
- `Vmgenid::guid(&self) -> &[u8; 16]` — getter for current GUID.
- `Vmgenid::guest_addr(&self) -> u64` — returns `guid_page_addr + guid_offset` (useful for ACPI ADDR method).

For random GUID generation, `rand` 0.9.2 is already a direct dependency of the `devices` crate (check `src/devices/Cargo.toml`). Use `rand::random::<[u8; 16]>()` or `rand::Rng::fill()` for generating random bytes — no new dependency needed. Alternatively, add `getrandom = "0.3"` as a direct dependency (it's already a transitive dep via `rand` 0.9), but `rand` is simpler since it's already imported.

In `src/devices/src/lib.rs`, add the module declaration alongside existing modules:
```rust
pub mod vmgenid;
```

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo check -p devices`
Expected: Compiles without errors.

**Commit:** `feat(devices): add vmgenid device model with GUID lifecycle`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Unit tests for Vmgenid

**Verifies:** vmgenid.AC1.1, vmgenid.AC1.2

**Files:**
- Modify: `src/devices/src/vmgenid/mod.rs` (add `#[cfg(test)] mod tests`)

**Testing:**

Add a `#[cfg(test)]` module with tests that verify:

- **vmgenid.AC1.1**: `new()` generates a non-zero GUID and writes it to guest memory at the correct offset (guid_page_addr + guid_offset). Create a `GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)])`, construct Vmgenid, read back bytes from guest memory and verify they match `vmgenid.guid()`.

- **vmgenid.AC1.2**: `update_guid()` produces a different GUID than the initial one. Call `update_guid()` and verify `old != new`. Also verify the new GUID is written to guest memory at the correct address.

Additional test: calling `update_guid()` multiple times produces different GUIDs each time.

Follow the existing unit test pattern: inline `#[cfg(test)]` module, `GuestMemoryMmap::from_ranges()` for mock memory.

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo test -p devices -- vmgenid`
Expected: All vmgenid tests pass.

**Commit:** `test(devices): add unit tests for vmgenid GUID lifecycle`

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Verify full build

**Files:** None (verification only)

**Verification:**

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo build`
Expected: Full workspace builds without errors.

Run: `cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo test -p devices -- vmgenid`
Expected: All vmgenid tests pass.

**Commit:** No commit (verification only).

<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->
