# Safe Snapshot Serialization Implementation Plan — Phase 1

**Goal:** Create the centralized `snapshot_serde` module with bincode-next backend and byte-level deserialization limits.

**Architecture:** Thin `snapshot_serde` module wrapping bincode-next's `encode_to_vec`/`decode_from_slice` with `config::standard()` and per-device const-generic byte limits via `with_limit::<MAX>()`.

**Tech Stack:** Rust, bincode-next 3.0.0-rc.5 (Encode/Decode derives, config builder, with_limit)

**Scope:** 5 phases from original design (phase 1 of 5)

**Codebase verified:** 2026-03-09

---

## Design Deviations

**bincode-next version:** The design document references "bincode-next v3.x". This plan uses `bincode-next = "3.0.0-rc.5"` (the latest release candidate). The API (`encode_to_vec`, `decode_from_slice`, `config::standard()`, `with_limit::<N>()`) is stable and identical to the 2.x series.

**Const generic byte limits:** The design specifies `deserialize(data, max_bytes)` with a runtime `max_bytes: usize` parameter. The actual implementation uses a const generic `deserialize::<T, const MAX_BYTES: usize>(data)` instead. This is because bincode-next's `with_limit::<N>()` requires a const generic parameter, not a runtime value. The const generic approach is strictly better: limits are verified at compile time, and the compiler can optimize the check. All per-device `MAX_SNAPSHOT_BYTES` constants are defined as `const` values near their respective state structs across Phases 2-4.

---

## Acceptance Criteria Coverage

This phase implements and tests:

### safe-snapshot-serde.AC2: Safe wrapper enforces byte-level deserialization limits
- **safe-snapshot-serde.AC2.1 Success:** `snapshot_serde::deserialize` with valid data under limit succeeds
- **safe-snapshot-serde.AC2.2 Failure:** `snapshot_serde::deserialize` with payload exceeding `max_bytes` returns `SnapshotError::Deserialize` without allocating the claimed size
- **safe-snapshot-serde.AC2.3 Failure:** Crafted payload with Vec length prefix claiming 1GB+ is rejected before allocation

---

## Prerequisites

The worktree branch `safe-snapshot-serde` was created from `origin/main`. The snapshot infrastructure (`Snapshottable` trait, `SnapshotError`, bincode dependency, all device snapshot impls) exists only on the `platform` branch. Task 1 merges `platform` into the worktree.

---

<!-- START_TASK_1 -->
### Task 1: Merge platform branch into worktree

This is an infrastructure task. The snapshot infrastructure (trait definitions, device impls, bincode dependency) lives on the `platform` branch. The worktree was created from `origin/main` and needs this content.

**Step 1: Merge platform**

```bash
cd /home/titanous/vm-platform/libkrun/.worktrees/safe-snapshot-serde
git merge platform --no-edit
```

**Step 2: Verify merge succeeded**

```bash
test -f src/devices/src/snapshot.rs && echo "snapshot.rs exists"
grep 'pub mod snapshot' src/devices/src/lib.rs
grep 'bincode' src/devices/Cargo.toml
```

Expected: `snapshot.rs exists`, module declaration found, bincode dependency found.

**Step 3: Verify build**

```bash
cargo check --features snapshot
```

Expected: Compiles without errors.

**Commit:** Merge commit created automatically by `git merge`.

<!-- END_TASK_1 -->

<!-- START_SUBCOMPONENT_A (tasks 2-4) -->

<!-- START_TASK_2 -->
### Task 2: Add bincode-next dependency and create snapshot_serde module

**Verifies:** safe-snapshot-serde.AC2.1

**Files:**
- Modify: `src/devices/Cargo.toml` — add `bincode-next` optional dependency
- Create: `src/devices/src/snapshot_serde.rs` — new module
- Modify: `src/devices/src/lib.rs` — declare `snapshot_serde` module

**Implementation:**

**Step 1: Add bincode-next to `src/devices/Cargo.toml`**

Add `bincode-next` as an optional dependency alongside the existing `bincode`:

```toml
# In [dependencies] section, add:
bincode-next = { version = "3.0.0-rc.5", optional = true }
```

Update the `snapshot` feature to include `bincode-next` (keep existing `serde` and `bincode` for now — they're removed in Phase 5):

```toml
# Change from:
snapshot = ["serde", "bincode"]
# To:
snapshot = ["serde", "bincode", "bincode-next"]
```

**Step 2: Create `src/devices/src/snapshot_serde.rs`**

```rust
//! Safe snapshot serialization using bincode-next with byte-level limits.
//!
//! All snapshot serialize/deserialize calls go through this module.
//! `deserialize` enforces a per-device byte limit via const generic
//! `MAX_BYTES`, which uses bincode-next's `with_limit()` to reject
//! oversized allocation requests before they reach the allocator.

use crate::snapshot::SnapshotError;

/// Serialize a snapshot state struct to bytes.
pub fn serialize<T: bincode_next::Encode>(state: &T) -> Result<Vec<u8>, SnapshotError> {
    bincode_next::encode_to_vec(state, bincode_next::config::standard())
        .map_err(|e| SnapshotError::Serialize(e.to_string()))
}

/// Deserialize a snapshot state struct from bytes with a byte limit.
///
/// `MAX_BYTES` is the maximum allowed payload size for this device.
/// Payloads larger than `MAX_BYTES` are rejected immediately.
/// Additionally, `with_limit()` prevents the deserializer from
/// attempting allocations that exceed the byte budget (e.g., from
/// crafted Vec length prefixes).
pub fn deserialize<T: bincode_next::Decode, const MAX_BYTES: usize>(
    data: &[u8],
) -> Result<T, SnapshotError> {
    if data.len() > MAX_BYTES {
        return Err(SnapshotError::Deserialize(format!(
            "snapshot data {} bytes exceeds limit of {} bytes",
            data.len(),
            MAX_BYTES
        )));
    }
    let (val, _) = bincode_next::decode_from_slice(
        data,
        bincode_next::config::standard().with_limit::<MAX_BYTES>(),
    )
    .map_err(|e| SnapshotError::Deserialize(e.to_string()))?;
    Ok(val)
}
```

**Step 3: Declare module in `src/devices/src/lib.rs`**

Add after the existing `pub mod snapshot;` line:

```rust
#[cfg(feature = "snapshot")]
pub mod snapshot_serde;
```

**Step 4: Verify build**

```bash
cargo check --features snapshot
```

Expected: Compiles without errors. If the `bincode_next::Encode` bound on `&T` doesn't work (no blanket impl), change `serialize` to take `state: T` and have callers clone. Check compiler output.

**Commit:** `feat(devices): add snapshot_serde module with bincode-next backend`

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Verify bincode-next API compatibility

This is a verification task. After Task 2, run `cargo check` and fix any API mismatches between the plan and the actual bincode-next 3.0.0-rc.5 API.

**Potential issues to check:**

1. **`Encode` trait bounds on references:** If `bincode_next::encode_to_vec` takes `val: E` by value and `&T` doesn't implement `Encode`, change the serialize signature to `serialize<T: bincode_next::Encode>(state: T)` and have callers pass references (bincode-next likely has `impl<T: Encode> Encode for &T`).

2. **`Decode` trait context parameter:** If `bincode_next::decode_from_slice` requires `D: Decode<()>` and the default isn't `()`, add the explicit bound: `T: bincode_next::Decode<()>`.

3. **`with_limit()` const generic:** Verify `with_limit::<MAX_BYTES>()` compiles when `MAX_BYTES` is a const generic parameter of the enclosing function.

4. **`config::standard()` return type:** Verify it chains with `.with_limit::<N>()` as expected.

**Step 1: Run cargo check**

```bash
cargo check --features snapshot 2>&1
```

**Step 2: Fix any compilation errors** based on the checklist above.

**Step 3: Re-verify**

```bash
cargo check --features snapshot
```

Expected: Clean compilation.

**Commit:** `fix(devices): adjust snapshot_serde API to match bincode-next` (only if changes needed)

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Unit tests for snapshot_serde

**Verifies:** safe-snapshot-serde.AC2.1, safe-snapshot-serde.AC2.2, safe-snapshot-serde.AC2.3

**Files:**
- Modify: `src/devices/src/snapshot_serde.rs` — add `#[cfg(test)]` module

**Testing:**

Add a test module at the bottom of `snapshot_serde.rs`. Tests must verify:

- **safe-snapshot-serde.AC2.1:** Define a test struct `TestState` with `#[derive(bincode_next::Encode, bincode_next::Decode, Debug, PartialEq)]` containing fields `a: u32`, `b: String`, `c: Vec<u8>`. Serialize it with `serialize()`, then deserialize with `deserialize::<TestState, 1024>()`. Assert the round-tripped value equals the original.

- **safe-snapshot-serde.AC2.2:** Serialize a `TestState`, then call `deserialize::<TestState, 1>()` with a limit smaller than the serialized size. Assert it returns `Err` and the error message contains "exceeds limit".

- **safe-snapshot-serde.AC2.3:** Construct a crafted byte payload that starts with valid bincode-next structure but contains a varint-encoded Vec length prefix claiming 1GB+ of elements. Pass it to `deserialize::<TestState, 128>()`. Assert it returns `Err` — the `with_limit()` guard rejects the allocation before it happens. To craft the varint: bincode-next's standard config uses variable-length integer encoding. A large length can be encoded in a few bytes. If crafting the exact varint is difficult, an alternative is to serialize a `TestState` with a large Vec (e.g., 256 bytes), then try to deserialize with a 64-byte limit — the `with_limit()` will reject mid-decode when cumulative bytes exceed the limit.

**Verification:**

```bash
cargo test --features snapshot -p devices -- snapshot_serde
```

Expected: All tests pass.

**Commit:** `test(devices): add snapshot_serde round-trip and limit enforcement tests`

<!-- END_TASK_4 -->

<!-- END_SUBCOMPONENT_A -->
