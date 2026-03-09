# Safe Snapshot Serialization Implementation Plan — Phase 4

**Goal:** Migrate all VMM-level snapshot serialization from bincode 1.x to `snapshot_serde`.

**Architecture:** The VMM crate has ~47 bincode operations across 5 files. This phase adds bincode-next as a dependency to the vmm crate, migrates all call sites, and handles the KVM type compatibility challenge via bincode-next's serde compat mode.

**Tech Stack:** Rust, bincode-next 3.0.0-rc.5, snapshot_serde module from Phase 1

**Scope:** 5 phases from original design (phase 4 of 5)

**Codebase verified:** 2026-03-09

---

## Acceptance Criteria Coverage

This phase implements and tests:

### safe-snapshot-serde.AC1: All bincode call sites migrated to bincode-next
- **safe-snapshot-serde.AC1.5 Success:** VMM-level snapshot structs (VmSnapshot, IncrementalSnapshot, SnapshotHeader, vCPU state, IC state) serialize/deserialize via `snapshot_serde`

### safe-snapshot-serde.AC4: State struct derives use bincode-next native Encode/Decode
- **safe-snapshot-serde.AC4.1 Success:** All `*State` structs use `#[cfg_attr(feature = "snapshot", derive(bincode_next::Encode, bincode_next::Decode))]` instead of serde derives

---

## VMM Architecture

The VMM snapshot path has these layers:
1. **snapshot.rs** — Defines `VmSnapshot`, `IncrementalSnapshot`, `SnapshotHeader`, `DirtyPage`, `InterruptControllerSnapshot`. Provides `save_vmstate`/`load_vmstate` and incremental variants. **10 bincode calls** (4 production + 6 test).
2. **lib.rs** — Orchestrates snapshot: collects per-vCPU state, interrupt controller state, per-device state → builds `VmSnapshot` → writes via snapshot.rs. **19 bincode calls** (15 production + 4 test).
3. **snapshot_store.rs** — Manages snapshot storage: merged snapshots, excluded page indices. **14 bincode calls** (3 production + 11 test).
4. **builder.rs** — Restore path: reads vmstate, deserializes. **1 bincode call**.
5. **linux/vstate.rs** — Per-vCPU state serialization: `VcpuState` (x86_64), `VmState` (x86_64), `Aarch64VcpuState`. **3 bincode calls**.

Many calls are platform-specific (`#[cfg(target_os)]`, `#[cfg(target_arch)]`).

---

## KVM Type Compatibility Strategy

**Problem:** `VcpuState` (x86_64) and `VmState` (x86_64) contain types from `kvm-bindings` (`kvm_regs`, `kvm_sregs`, `kvm_debugregs`, `kvm_lapic_state`, `kvm_mp_state`, `kvm_vcpu_events`, `kvm_xcrs`, `kvm_xsave`, `CpuId`, `Msrs`, `kvm_pit_state2`, `kvm_clock_data`, `kvm_irqchip`). These types have `serde::Serialize`/`serde::Deserialize` derives but NOT `bincode_next::Encode`/`bincode_next::Decode`.

**Chosen approach:** Keep serde derives on `VcpuState` and `VmState`, and serialize them using bincode-next's serde compatibility layer. Specifically:

1. **Keep both derives** on VcpuState and VmState:
   ```rust
   #[cfg(all(target_arch = "x86_64", feature = "snapshot"))]
   #[derive(serde::Serialize, serde::Deserialize)]
   pub struct VcpuState { ... }
   ```
   These structs keep their serde derives (they need them for the serde compat layer).

2. **Use `bincode_next::serde::encode_to_vec` / `bincode_next::serde::decode_from_slice`** for these two structs instead of the standard `encode_to_vec`/`decode_from_slice`. These functions use the serde compatibility layer to encode/decode serde types through bincode-next's wire format.

3. **All other VMM structs** (`VmSnapshot`, `IncrementalSnapshot`, `SnapshotHeader`, `DirtyPage`, `InterruptControllerSnapshot`, `Aarch64VcpuState`) switch to native `bincode_next::Encode`/`bincode_next::Decode` derives.

**Why this approach:**
- Avoids writing manual Encode/Decode impls for 13+ KVM FFI types
- Achieves the same wire format (bincode-next standard config) for all data
- serde compat layer is a first-class bincode-next feature, not a hack
- Only 2 structs (VcpuState, VmState) need serde — all others use native derives

---

## Clean Break: `#[serde(default)]` Fields Intentionally Dropped

The following fields use `#[serde(default)]` for backward-compatible deserialization with bincode 1.x. Since this migration is a **clean break** (existing snapshots are invalidated), these attributes are intentionally removed when switching to bincode-next native Encode/Decode:

**VmSnapshot:**
- `gic_state: Option<Vec<u8>>` — `#[cfg_attr(feature = "snapshot", serde(default))]`
- `vm_state: Option<Vec<u8>>` — `#[cfg_attr(feature = "snapshot", serde(default))]`
- `excluded_pages: Vec<u64>` — `#[cfg_attr(feature = "snapshot", serde(default))]`

**IncrementalSnapshot:**
- `gic_state: Option<Vec<u8>>` — `#[cfg_attr(feature = "snapshot", serde(default))]`
- `vm_state: Option<Vec<u8>>` — `#[cfg_attr(feature = "snapshot", serde(default))]`
- `reclaimed_pages: Vec<u64>` — `#[cfg_attr(feature = "snapshot", serde(default))]`

**VcpuState (x86_64):**
- `tsc_khz: Option<u32>` — `#[serde(default)]`

When switching VmSnapshot/IncrementalSnapshot to bincode-next native derives, **remove the `#[cfg_attr(feature = "snapshot", serde(default))]`** attributes. They have no equivalent in bincode-next and are not needed for the clean break.

VcpuState keeps its serde derives (KVM compat), so `#[serde(default)]` on `tsc_khz` remains as-is — it applies only within the serde serialization layer.

---

<!-- START_TASK_1 -->
### Task 1: Add bincode-next dependency to VMM crate

**Files:**
- Modify: `src/vmm/Cargo.toml`

**Implementation:**

Add `bincode-next` as an optional dependency and include it in the `snapshot` feature.

1. In `[dependencies]` section, add:
   ```toml
   bincode-next = { version = "3.0.0-rc.5", optional = true }
   ```

2. Update the `snapshot` feature (line 19):
   ```toml
   # FROM:
   snapshot = ["serde", "serde_json", "bincode", "futures", "tokio", "devices/snapshot", "hvf?/snapshot"]
   # TO:
   snapshot = ["serde", "serde_json", "bincode", "bincode-next", "futures", "tokio", "devices/snapshot", "hvf?/snapshot"]
   ```

   Note: `serde` stays in the feature list because VcpuState/VmState use the serde compat layer (see KVM strategy above).

The VMM uses `bincode_next::encode_to_vec` and `bincode_next::decode_from_slice` directly (not `devices::snapshot_serde`) to avoid error type conversion between the two crates' distinct `SnapshotError` enums.

**Verification:**
```bash
cargo check --features snapshot -p vmm
```
Expected: Compiles.

**Commit:** `build(vmm): add bincode-next dependency for snapshot migration`

<!-- END_TASK_1 -->

<!-- START_SUBCOMPONENT_A (tasks 2-3) -->

<!-- START_TASK_2 -->
### Task 2: Migrate snapshot.rs structs and functions

**Verifies:** safe-snapshot-serde.AC1.5, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/vmm/src/snapshot.rs`

**Implementation:**

This file has 5 structs with serde derives and **10 bincode calls** (4 production + 6 test).

1. **Change derives** on all snapshot structs. **Remove `#[serde(default)]` attributes** (clean break — see above):
   - `SnapshotHeader` (~line 191): switch to bincode-next Encode/Decode
   - `VmSnapshot` (~line 203): switch to bincode-next Encode/Decode, **remove all `#[cfg_attr(feature = "snapshot", serde(default))]`** on `gic_state`, `vm_state`, `excluded_pages`
   - `DirtyPage` (~line 399): switch to bincode-next Encode/Decode
   - `IncrementalSnapshot` (~line 407): switch to bincode-next Encode/Decode, **remove all `#[cfg_attr(feature = "snapshot", serde(default))]`** on `gic_state`, `vm_state`, `reclaimed_pages`
   - `InterruptControllerSnapshot` (~line 427): switch to bincode-next Encode/Decode

   For each:
   ```rust
   // FROM:
   #[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
   // TO:
   #[cfg_attr(feature = "snapshot", derive(bincode_next::Encode, bincode_next::Decode))]
   ```

   All fields in these structs are standard types (`u32`, `u64`, `Vec<u8>`, `String`, `Option<T>`, nested structs with Encode/Decode). No KVM types here.

2. **Define a bincode config helper** at the top of the file:
   ```rust
   #[cfg(feature = "snapshot")]
   fn bincode_config() -> impl bincode_next::config::Config {
       bincode_next::config::standard()
   }
   ```

3. **Migrate production calls** (4 total):
   - save_vmstate() (~line 325): `bincode::serialize(snapshot)` → `bincode_next::encode_to_vec(snapshot, bincode_config())`
   - load_vmstate() (~line 346): `bincode::deserialize(&data)` → `bincode_next::decode_from_slice(&data, bincode_config().with_limit::<{ VMSTATE_MAX_SIZE as usize }>()).map(|(val, _)| val)`
   - save_incremental_snapshot() (~line 439): same pattern as save_vmstate
   - load_incremental_snapshot() (~line 460): same pattern as load_vmstate

4. **Migrate test calls** (6 total):
   - test_header_roundtrip (~lines 520-521)
   - prop_vm_snapshot_bincode_roundtrip (~lines 1062-1064)
   - Any other test roundtrips in the file

   Replace all `bincode::serialize`/`bincode::deserialize` with `bincode_next::encode_to_vec`/`bincode_next::decode_from_slice` using `bincode_config()`.

**Verification:**
```bash
cargo test --features snapshot -p vmm -- snapshot::tests
```
Expected: Tests pass.

**Commit:** `refactor(vmm): migrate snapshot.rs to bincode-next`

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Migrate vCPU and VM state serialization

**Verifies:** safe-snapshot-serde.AC1.5, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/vmm/src/linux/vstate.rs`

**Implementation:**

This file defines 3 state structs and has 3 bincode calls.

**Struct migrations:**

1. **VcpuState** (~line 1940, x86_64) — **KEEP serde derives** (KVM compat):
   ```rust
   // KEEP AS-IS (serde derives stay for KVM type compat):
   #[cfg(all(target_arch = "x86_64", feature = "snapshot"))]
   #[derive(serde::Serialize, serde::Deserialize)]
   pub struct VcpuState {
       cpuid: CpuId,
       msrs: Msrs,
       debug_regs: kvm_debugregs,
       lapic: kvm_lapic_state,
       mp_state: kvm_mp_state,
       regs: kvm_regs,
       sregs: kvm_sregs,
       vcpu_events: kvm_vcpu_events,
       xcrs: kvm_xcrs,
       xsave: kvm_xsave,
       #[serde(default)]  // <-- KEEP: applies within serde layer
       tsc_khz: Option<u32>,
   }
   ```
   No derive changes. The `#[serde(default)]` on `tsc_khz` remains because VcpuState continues to use serde.

2. **VmState** (~line 977, x86_64) — **KEEP serde derives** (KVM compat):
   ```rust
   // KEEP AS-IS (serde derives stay for KVM type compat):
   #[cfg(all(target_arch = "x86_64", feature = "snapshot"))]
   #[derive(serde::Serialize, serde::Deserialize)]
   pub struct VmState {
       pitstate: kvm_pit_state2,
       clock: kvm_clock_data,
       pic_master: kvm_irqchip,
       pic_slave: kvm_irqchip,
       ioapic: kvm_irqchip,
   }
   ```
   No derive changes.

3. **Aarch64VcpuState** (~line 1959, aarch64) — **SWITCH to bincode-next** (all native types):
   ```rust
   // FROM:
   #[cfg(all(target_arch = "aarch64", feature = "snapshot"))]
   #[derive(serde::Serialize, serde::Deserialize)]
   // TO:
   #[cfg(all(target_arch = "aarch64", feature = "snapshot"))]
   #[derive(bincode_next::Encode, bincode_next::Decode)]
   ```
   Fields are `u32` and `Vec<(u64, Vec<u8>)>` — all have native Encode/Decode.

**Bincode call migrations** (3 total, in `paused()` state handler):

1. **serialize (~line 1833):** Handles both arch via `#[cfg]`. For x86_64 VcpuState (serde compat):
   ```rust
   // FROM:
   bincode::serialize(&state).map_err(|e| Error::VcpuState(e.to_string()))
   // TO (x86_64, serde compat):
   bincode_next::serde::encode_to_vec(&state, bincode_next::config::standard())
       .map_err(|e| Error::VcpuState(e.to_string()))
   // TO (aarch64, native):
   bincode_next::encode_to_vec(&state, bincode_next::config::standard())
       .map_err(|e| Error::VcpuState(e.to_string()))
   ```

2. **deserialize aarch64 (~line 1849):** Native decode:
   ```rust
   bincode_next::decode_from_slice::<Aarch64VcpuState, _>(
       &data, bincode_next::config::standard().with_limit::<{ 10 * 1024 * 1024 }>()
   ).map(|(val, _)| val)
   ```

3. **deserialize x86_64 (~line 1859):** Serde compat decode:
   ```rust
   bincode_next::serde::decode_from_slice::<VcpuState, _>(
       &data, bincode_next::config::standard().with_limit::<{ 10 * 1024 * 1024 }>()
   ).map(|(val, _)| val)
   ```

**Also in lib.rs** (Phase 4 Task 4): VmState serialization at line 1288 and 1482 uses `bincode::serialize(&self.vm.save_state())`. These need `bincode_next::serde::encode_to_vec` since VmState uses serde compat.

**Verification:**
```bash
cargo check --features snapshot -p vmm
```
Expected: Compiles (vCPU tests require KVM, compile check sufficient).

**Commit:** `refactor(vmm): migrate vCPU and VM state serialization to bincode-next`

<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 4-5) -->

<!-- START_TASK_4 -->
### Task 4: Migrate lib.rs snapshot orchestration

**Verifies:** safe-snapshot-serde.AC1.5

**Files:**
- Modify: `src/vmm/src/lib.rs`

**Implementation:**

This file has **19 bincode calls** (15 production + 4 test). Many are platform-specific. Migrate ALL of them.

Use `bincode_next::encode_to_vec`/`bincode_next::decode_from_slice` with standard config for most calls. **Exception:** Calls involving `VmState` (x86_64) or `VcpuState` (x86_64) must use `bincode_next::serde::encode_to_vec`/`bincode_next::serde::decode_from_slice` (serde compat for KVM types).

**Production calls to migrate (15 total):**

1. **Line 369** (macOS): `bincode::serialize(&ic_snapshot)` — IC save → `encode_to_vec` (native, InterruptControllerSnapshot has Encode)
2. **Line 380** (macOS): `bincode::deserialize(data)` — IC restore → `decode_from_slice` (native)
3. **Line 460** (Linux x86_64): `bincode::deserialize(data)` — restore VmState → **`serde::decode_from_slice`** (VmState is serde compat)
4. **Line 586** (Linux): `bincode::deserialize(&vmstate_bytes)` — eager restore → `decode_from_slice` (native, VmSnapshot has Decode)
5. **Line 672** (Linux uffd): `bincode::deserialize(&vmstate_bytes)` — pre-validate → `decode_from_slice` (native)
6. **Line 910** (macOS aarch64): `bincode::deserialize(data)` — map vCPU states → `decode_from_slice` (native, Aarch64VcpuState has Decode)
7. **Line 1051** (macOS aarch64): `bincode::deserialize(data)` — incremental vCPU states → `decode_from_slice` (native)
8. **Line 1288** (Linux x86_64): `bincode::serialize(&self.vm.save_state())` — save VmState → **`serde::encode_to_vec`** (VmState is serde compat)
9. **Line 1380** (Linux): `bincode::serialize(&vm_snapshot)` — serialize VmSnapshot → `encode_to_vec` (native)
10. **Line 1408** (macOS aarch64): `bincode::serialize(s)` — vCPU states → `encode_to_vec` (native, Aarch64VcpuState has Encode)
11. **Line 1432** (macOS aarch64): `bincode::serialize(&vm_snapshot)` — VmSnapshot → `encode_to_vec` (native)
12. **Line 1482** (Linux x86_64): `bincode::serialize(&self.vm.save_state())` — incremental VmState → **`serde::encode_to_vec`** (VmState is serde compat)
13. **Line 1606** (Linux): `bincode::serialize(&incremental)` — IncrementalSnapshot → `encode_to_vec` (native)
14. **Line 1651** (macOS aarch64): `bincode::serialize(s)` — incremental vCPU → `encode_to_vec` (native)
15. **Line 1716** (macOS aarch64): `bincode::serialize(&incremental)` → `encode_to_vec` (native)

**Test calls to migrate (4 total):**
16. **Line 2367**: roundtrip serialize → `encode_to_vec`
17. **Line 2371**: roundtrip deserialize → `decode_from_slice`
18. **Line 2418**: roundtrip serialize → `encode_to_vec`
19. **Line 2422**: roundtrip deserialize → `decode_from_slice`

**Key rule:** Lines 460, 1288, 1482 deal with `VmState` (x86_64 KVM types) → use `bincode_next::serde::*` variants. All other calls use `bincode_next::encode_to_vec`/`bincode_next::decode_from_slice` (native).

**Transformation pattern:**

For serialize (native):
```rust
bincode_next::encode_to_vec(&value, bincode_next::config::standard())
    .map_err(|e| SnapshotError::Serialize(e.to_string()))
```

For serialize (serde compat, VmState only):
```rust
bincode_next::serde::encode_to_vec(&value, bincode_next::config::standard())
    .map_err(|e| SnapshotError::Serialize(e.to_string()))
```

For deserialize (native):
```rust
bincode_next::decode_from_slice(&data, bincode_next::config::standard().with_limit::<{ VMSTATE_MAX_SIZE as usize }>())
    .map(|(val, _)| val)
    .map_err(|e| SnapshotError::Deserialize(e.to_string()))
```

For deserialize (serde compat, VmState only):
```rust
bincode_next::serde::decode_from_slice(&data, bincode_next::config::standard().with_limit::<{ VMSTATE_MAX_SIZE as usize }>())
    .map(|(val, _)| val)
    .map_err(|e| SnapshotError::Deserialize(e.to_string()))
```

**Verification:**
```bash
cargo test --features snapshot -p vmm -- test_
```
Expected: All roundtrip tests pass.

**Commit:** `refactor(vmm): migrate lib.rs snapshot orchestration to bincode-next`

<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Migrate snapshot_store.rs and builder.rs

**Verifies:** safe-snapshot-serde.AC1.5

**Files:**
- Modify: `src/vmm/src/snapshot_store.rs`
- Modify: `src/vmm/src/builder.rs`

**Implementation:**

**snapshot_store.rs** has **14 bincode calls** (3 production + 11 test):

Production:
1. **Line 213**: `bincode::serialize(&merged_vmstate)` — merged snapshot serialization
2. **Line 452**: `bincode::serialize(&excluded_vec)` — excluded page indices (`Vec<u64>`)
3. **Line 552**: `bincode::deserialize::<Vec<u64>>(&data)` — read excluded page indices

Test calls (11 total):
4. **Line 796**: `bincode::serialize(&base_vmstate)`
5. **Line 824**: `bincode::serialize(&inc1)`
6. **Line 848**: `bincode::serialize(&inc2)`
7. **Line 925**: `bincode::serialize(&base_vmstate)`
8. **Line 947**: `bincode::serialize(&inc)`
9. **Line 1033**: `bincode::serialize(&base_vmstate)`
10. **Line 1055**: `bincode::serialize(&inc1)`
11. **Line 1072**: `bincode::serialize(&inc2)`
12. **Line 1086**: `bincode::deserialize(&merged_vmstate_bytes)`
13. **Line 1140**: `bincode::serialize(&base_vmstate)`
14. **Line 1159**: `bincode::deserialize(&read_vmstate_bytes)`

Apply `bincode_next::encode_to_vec`/`bincode_next::decode_from_slice` transformation to all 14 calls.

For the `Vec<u64>` deserialization at line 552, use VMSTATE_MAX_SIZE limit:
```rust
bincode_next::decode_from_slice::<Vec<u64>, _>(&data, bincode_next::config::standard().with_limit::<{ VMSTATE_MAX_SIZE as usize }>())
    .map(|(val, _)| val)
```

**builder.rs** has 1 bincode call:

1. **Line 823**: `bincode::deserialize(&vmstate_bytes)` — pre-validate vmstate in UFFD restore path. Same transformation pattern.

**Verification:**
```bash
cargo test --features snapshot -p vmm -- snapshot_store
```
Expected: All tests pass.

**Commit:** `refactor(vmm): migrate snapshot_store and builder to bincode-next`

<!-- END_TASK_5 -->

<!-- END_SUBCOMPONENT_B -->

<!-- START_TASK_6 -->
### Task 6: Verify all VMM tests pass

**Verifies:** safe-snapshot-serde.AC1.5

**Files:** None (verification only)

**Step 1: Run all VMM tests**

```bash
cargo test --features snapshot -p vmm
```

Expected: All tests pass.

**Step 2: Run full check**

```bash
just check
```

Expected: Format + clippy pass.

**Step 3: Run full test suite**

```bash
just test
```

Expected: All unit tests pass across all crates.

**Commit:** None (verification only). If fixes needed, commit as `fix(vmm): ...`.

<!-- END_TASK_6 -->
