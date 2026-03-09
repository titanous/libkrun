# Safe Snapshot Serialization Implementation Plan — Phase 5

**Goal:** Remove bincode 1.x entirely from the dependency tree and update fuzz/test workspaces.

**Architecture:** After Phases 2-4 migrated all call sites, bincode 1.x is unused. Remove it from all Cargo.toml files, remove `serde` from snapshot feature gates (no longer needed for snapshot serialization), update the fuzz workspace, and verify clean builds.

**Tech Stack:** Rust, Cargo dependency management

**Scope:** 5 phases from original design (phase 5 of 5)

**Codebase verified:** 2026-03-09

---

## Acceptance Criteria Coverage

This phase implements and tests:

### safe-snapshot-serde.AC5: bincode 1.x fully removed
- **safe-snapshot-serde.AC5.1 Success:** No `bincode` (1.x) in `Cargo.lock` after migration
- **safe-snapshot-serde.AC5.2 Success:** `fuzz/Cargo.toml` uses bincode-next for snapshot-related fuzz targets

---

## Files Requiring Changes

From investigation, 4 Cargo.toml files reference bincode 1.x:
1. `src/devices/Cargo.toml` — line 28: `bincode = { version = "1.3", optional = true }`
2. `src/vmm/Cargo.toml` — line 44: `bincode = { version = "1.3", optional = true }`
3. `fuzz/Cargo.toml` — line 18: `bincode = "1.3"`
4. `tests/test_vsock_proxy/Cargo.toml` — line 14: `bincode = "1"`

### Serde Removal Analysis

**Devices crate (`src/devices/`):** All serde usage is for snapshot derives (`#[derive(serde::Serialize, serde::Deserialize)]` or `#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]`). After Phases 2-3 replace these with `bincode_next::Encode`/`bincode_next::Decode`, serde is completely unused. **Safe to remove both `serde` dependency and `serde` from the snapshot feature.**

**VMM crate (`src/vmm/`):** Serde is still needed. Phase 4 uses `bincode_next::serde::encode_to_vec`/`decode_from_slice` for `VcpuState` and `VmState` (which contain kvm-bindings types with serde derives but not bincode-next derives). Additionally, `serde_json` is used for non-snapshot purposes. **Keep `serde` as a dependency, but remove it from the `snapshot` feature list** (serde_json pulls in serde transitively, and the serde compat layer in bincode-next is activated by bincode-next's own `serde` feature, not by having serde in the workspace feature list).

---

<!-- START_TASK_1 -->
### Task 1: Remove bincode 1.x from devices crate

**Verifies:** safe-snapshot-serde.AC5.1

**Files:**
- Modify: `src/devices/Cargo.toml`

**Implementation:**

1. **Remove bincode dependency** (~line 28):
   ```toml
   # DELETE this line:
   bincode = { version = "1.3", optional = true }
   ```

2. **Remove serde dependency** (~line 38):
   ```toml
   # DELETE this line:
   serde = { version = "1.0", features = ["derive"], optional = true }
   ```

3. **Update snapshot feature** (~line 12):
   ```toml
   # FROM:
   snapshot = ["serde", "bincode", "bincode-next"]
   # TO:
   snapshot = ["bincode-next"]
   ```

4. **Verify no remaining bincode/serde references** in device source files. After Phases 2-3, all `bincode::` calls should be replaced. Search to confirm:
   ```bash
   grep -r 'bincode::' src/devices/src/ --include='*.rs'
   grep -r 'serde::Serialize\|serde::Deserialize' src/devices/src/ --include='*.rs'
   ```
   Expected: No matches (all replaced with `bincode_next::Encode`/`bincode_next::Decode` and `snapshot_serde::` calls).

**Verification:**
```bash
cargo check --features snapshot -p devices
```
Expected: Compiles without errors.

**Commit:** `build(devices): remove bincode 1.x and serde from snapshot dependencies`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Remove bincode 1.x from VMM crate

**Verifies:** safe-snapshot-serde.AC5.1

**Files:**
- Modify: `src/vmm/Cargo.toml`

**Implementation:**

1. **Remove bincode dependency** (~line 44):
   ```toml
   # DELETE this line:
   bincode = { version = "1.3", optional = true }
   ```

2. **Update snapshot feature** (~line 19):
   ```toml
   # FROM:
   snapshot = ["serde", "serde_json", "bincode", "bincode-next", "futures", "tokio", "devices/snapshot", "hvf?/snapshot"]
   # TO:
   snapshot = ["serde_json", "bincode-next", "futures", "tokio", "devices/snapshot", "hvf?/snapshot"]
   ```

   **Why keep serde_json but remove serde from feature list:** `serde_json` is used for non-snapshot purposes in VMM. It pulls in `serde` transitively, so `serde` doesn't need to be in the feature list. Phase 4's `VcpuState`/`VmState` serde compat (`bincode_next::serde::*`) works because those types have serde derives unconditionally (from kvm-bindings), and bincode-next's `serde` feature enables the compat layer. The `serde` crate remains available via `serde_json`'s transitive dependency — it just doesn't need to be an explicit feature gate.

3. **Verify no remaining bincode 1.x references:**
   ```bash
   grep -r 'bincode::' src/vmm/src/ --include='*.rs'
   ```
   Expected: No matches (after Phase 4 migration, only `bincode_next::` calls remain).

**Verification:**
```bash
cargo check --features snapshot -p vmm
```
Expected: Compiles without errors.

**Commit:** `build(vmm): remove bincode 1.x from snapshot dependencies`

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Update fuzz workspace

**Verifies:** safe-snapshot-serde.AC5.2

**Files:**
- Modify: `fuzz/Cargo.toml`
- Modify: `fuzz/fuzz_targets/fuzz_snapshot_deser.rs`

**Implementation:**

The fuzz workspace is SEPARATE from the root workspace. `[patch.crates-io]` in root Cargo.toml does NOT apply to fuzz (see MEMORY.md).

1. **Update fuzz/Cargo.toml**:
   ```toml
   # FROM:
   bincode = "1.3"
   # TO:
   bincode-next = "3.0.0-rc.5"
   ```

2. **Update fuzz_snapshot_deser.rs** — this is the only fuzz target using bincode:
   ```rust
   // FROM:
   let _ = bincode::deserialize::<VmSnapshot>(data);
   let _ = bincode::deserialize::<IncrementalSnapshot>(data);
   // TO:
   let _ = bincode_next::decode_from_slice::<VmSnapshot, _>(data, bincode_next::config::standard());
   let _ = bincode_next::decode_from_slice::<IncrementalSnapshot, _>(data, bincode_next::config::standard());
   ```

   Note: Fuzz targets intentionally omit `with_limit()` — the fuzzer should explore all code paths including oversized allocations (the fuzzer's memory limit handles OOM).

3. **Verify fuzz patches**: Ensure `fuzz/Cargo.toml` has the required `[patch.crates-io]` entries for vhost and linux-loader (see MEMORY.md for required patches).

**Verification:**
```bash
cd fuzz && cargo check
```
Expected: Compiles without errors.

**Commit:** `build(fuzz): migrate fuzz_snapshot_deser to bincode-next`

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Update test_vsock_proxy workspace

**Verifies:** safe-snapshot-serde.AC5.1

**Files:**
- Modify: `tests/test_vsock_proxy/Cargo.toml`
- Modify: `tests/test_vsock_proxy/src/main.rs`

**Implementation:**

The test_vsock_proxy uses bincode for `ProxyState` serialization (separate from the snapshot path).

1. **Update Cargo.toml** (~line 14):
   ```toml
   # FROM:
   bincode = "1"
   # TO:
   bincode-next = "3.0.0-rc.5"
   ```

2. **Update main.rs**: Find `bincode::serialize`/`bincode::deserialize` calls and replace with `bincode_next::encode_to_vec`/`bincode_next::decode_from_slice` using standard config.

3. **Update ProxyState struct**: Change serde derives to bincode-next Encode/Decode.

**Verification:**
```bash
cd tests/test_vsock_proxy && cargo check
```
Expected: Compiles without errors.

**Commit:** `build(tests): migrate test_vsock_proxy to bincode-next`

<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Regenerate Cargo.lock and verify clean build

**Verifies:** safe-snapshot-serde.AC5.1

**Files:**
- Regenerated: `Cargo.lock`
- Regenerated: `fuzz/Cargo.lock`
- Regenerated: `tests/Cargo.lock`

**Step 1: Regenerate root Cargo.lock**

```bash
cargo generate-lockfile
```

**Step 2: Verify no bincode 1.x in root Cargo.lock**

```bash
grep -A2 'name = "bincode"' Cargo.lock
```

Expected: Only `bincode-next` entries (version 3.x), no `bincode` 1.x entries.

**Step 3: Regenerate fuzz Cargo.lock**

```bash
cd fuzz && cargo generate-lockfile
```

**Step 4: Verify no bincode 1.x in fuzz Cargo.lock**

```bash
grep -A2 'name = "bincode"' fuzz/Cargo.lock
```

Expected: No bincode 1.x entries.

**Step 5: Full build verification**

```bash
cargo build --all-features
just check
just test
```

Expected: All pass with no bincode 1.x in the dependency tree.

**Commit:** `chore: regenerate lockfiles without bincode 1.x`

<!-- END_TASK_5 -->
