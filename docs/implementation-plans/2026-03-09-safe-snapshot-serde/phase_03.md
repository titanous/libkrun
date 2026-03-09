# Safe Snapshot Serialization Implementation Plan — Phase 3

**Goal:** Migrate MMIO transport, balloon, and vhost-user device serialization to `snapshot_serde`.

**Architecture:** Same mechanical transformation as Phase 2. Virtio devices use two snapshot patterns: `Snapshottable` trait (MMIO transport) and `VirtioDevice::save_backend_state`/`restore_backend_state` (balloon, vhost-user vsock/fs). Both patterns get the same treatment.

**Tech Stack:** Rust, bincode-next 3.0.0-rc.5, snapshot_serde module from Phase 1

**Scope:** 5 phases from original design (phase 3 of 5)

**Codebase verified:** 2026-03-09

---

## Acceptance Criteria Coverage

This phase implements and tests:

### safe-snapshot-serde.AC1: All bincode call sites migrated to bincode-next
- **safe-snapshot-serde.AC1.3 Success:** Virtio device snapshot paths (MMIO transport, balloon, vhost-user vsock, vhost-user fs) use `snapshot_serde`
- **safe-snapshot-serde.AC1.4 Success:** Vhost-user `save_backend_state`/`restore_backend_state` methods use `snapshot_serde`

### safe-snapshot-serde.AC4: State struct derives use bincode-next native Encode/Decode
- **safe-snapshot-serde.AC4.1 Success:** All `*State` structs use `#[cfg_attr(feature = "snapshot", derive(bincode_next::Encode, bincode_next::Decode))]` instead of serde derives

---

## Device Snapshot Patterns

Two patterns exist in virtio devices:

**Pattern 1: Snapshottable trait** (MMIO transport)
- `save_state() -> Result<Vec<u8>, SnapshotError>`
- `restore_state(&mut self, data: &[u8]) -> Result<(), SnapshotError>`
- Orchestrates: calls `device.save_backend_state()` inside its own save_state

**Pattern 2: VirtioDevice trait methods** (balloon, vhost-user vsock/fs)
- `save_backend_state(&self) -> Option<Vec<u8>>`
- `restore_backend_state(&mut self, data: &[u8])` (returns `()`)
- Called BY MmioTransport during its save/restore

The `Option`/`()` return types on Pattern 2 mean errors are logged and swallowed (`.ok()` on serialize, `match` with `log::error` on deserialize). This pattern is preserved.

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Migrate MMIO Transport

**Verifies:** safe-snapshot-serde.AC1.3, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/devices/src/virtio/mmio.rs`

**Implementation:**

MMIO transport has TWO state structs with serde derives:
- `MmioTransportState` (~line 658) — contains `Vec<QueueState>`, `Option<Vec<u8>>` (backend_state)
- `QueueState` (~line 681) — contains scalar fields (u16, bool, u64)

Both structs are `pub` (used cross-crate).

1. Add `const MAX_SNAPSHOT_BYTES: usize = 4096;` near `MmioTransportState`

2. Change derives on BOTH structs:
   ```rust
   // MmioTransportState (~line 658):
   // FROM:
   #[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
   // TO:
   #[cfg_attr(feature = "snapshot", derive(bincode_next::Encode, bincode_next::Decode))]

   // QueueState (~line 681):
   // FROM:
   #[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
   // TO:
   #[cfg_attr(feature = "snapshot", derive(bincode_next::Encode, bincode_next::Decode))]
   ```

3. save_state() (~line 736): Change `bincode::serialize(&state).map_err(|e| SnapshotError::Serialize(e.to_string()))` → `snapshot_serde::serialize(&state)`

4. restore_state() (~line 757): Change `bincode::deserialize(data).map_err(|e| SnapshotError::Deserialize(e.to_string()))` → `snapshot_serde::deserialize::<_, { MAX_SNAPSHOT_BYTES }>(data)`

**Verification:**
```bash
cargo check --features snapshot -p devices
```
Expected: Compiles without errors.

**Commit:** `refactor(devices): migrate MMIO transport snapshot to bincode-next`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Migrate Balloon

**Verifies:** safe-snapshot-serde.AC1.3, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs`

**Implementation:**

Balloon uses `save_backend_state`/`restore_backend_state` (Pattern 2). State has only scalar fields (num_pages: u32, actual: u32).

1. Add `const MAX_SNAPSHOT_BYTES: usize = 128;` near `BalloonState` (~line 124)

2. Change derive on `BalloonState` (~line 125):
   ```rust
   // FROM:
   #[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
   // TO:
   #[cfg_attr(feature = "snapshot", derive(bincode_next::Encode, bincode_next::Decode))]
   ```

3. save_backend_state() (~line 835): Change `bincode::serialize(&state)` → `snapshot_serde::serialize(&state)`. Keep the `.map_err(|e| log::error!(...)).ok()` pattern.

4. restore_backend_state() (~line 853): Change `bincode::deserialize::<BalloonState>(data)` → `snapshot_serde::deserialize::<BalloonState, { MAX_SNAPSHOT_BYTES }>(data)`. Keep the `match` error handling pattern.

**Verification:**
```bash
cargo check --features snapshot -p devices
```
Expected: Compiles without errors.

**Commit:** `refactor(devices): migrate balloon snapshot to bincode-next`

<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-5) -->

<!-- START_TASK_3 -->
### Task 3: Migrate VhostUser Vsock

**Verifies:** safe-snapshot-serde.AC1.3, safe-snapshot-serde.AC1.4, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/vsock.rs`

**Implementation:**

VhostUser Vsock uses `save_backend_state`/`restore_backend_state` (Pattern 2). State contains: guest_cid u64, acked_features u64, acked_protocol_features u64, vring_bases Vec<u16>, daemon_state Vec<u8>, socket_path Option<String>.

1. Add `const MAX_SNAPSHOT_BYTES: usize = 8192;` near `VhostUserVsockState` (~line 43)

2. Change derive on `VhostUserVsockState` (~line 45):
   ```rust
   // FROM:
   #[cfg(feature = "snapshot")]
   #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
   // TO:
   #[cfg(feature = "snapshot")]
   #[derive(Clone, Debug, bincode_next::Encode, bincode_next::Decode)]
   ```

3. save_backend_state() (~line 252): Change `bincode::serialize(&state)` → `snapshot_serde::serialize(&state)`. Keep `.map_err(|e| log::error!(...)).ok()`.

4. restore_backend_state() (~line 260): Change `bincode::deserialize(data)` → `snapshot_serde::deserialize::<VhostUserVsockState, { MAX_SNAPSHOT_BYTES }>(data)`. Keep `match` error handling.

5. **Update tests** (~lines 445-486): Two tests directly call `bincode::serialize`/`bincode::deserialize`:
   - `test_vsock_state_roundtrip` (~line 440): Change `bincode::serialize(&state)` → `snapshot_serde::serialize(&state)` and `bincode::deserialize(&serialized)` → `snapshot_serde::deserialize::<VhostUserVsockState, { MAX_SNAPSHOT_BYTES }>(&serialized)`
   - `test_restore_backend_state_stores_pending` (~line 465): Change `bincode::serialize(&state)` → `snapshot_serde::serialize(&state).expect("serialize")`

**Verification:**
```bash
cargo test --features snapshot,vhost-user -p devices -- vhost_user::vsock
```
Expected: All tests pass.

**Commit:** `refactor(devices): migrate vhost-user vsock snapshot to bincode-next`

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Migrate VhostUser FS

**Verifies:** safe-snapshot-serde.AC1.3, safe-snapshot-serde.AC1.4, safe-snapshot-serde.AC4.1

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/fs.rs`

**Implementation:**

VhostUser FS uses `save_backend_state`/`restore_backend_state` (Pattern 2). State contains: tag String, socket_path String, dax_window_mib Option<u32>, acked_features u64, acked_protocol_features u64, vring_bases Vec<u16>, daemon_state Vec<u8>, config_tag Vec<u8>, config_num_request_queues u32.

1. Add `const MAX_SNAPSHOT_BYTES: usize = 8192;` near `VhostUserFsState` (~line 26)

2. Change derive on `VhostUserFsState` (~line 29):
   ```rust
   // FROM:
   #[cfg(feature = "snapshot")]
   #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
   // TO:
   #[cfg(feature = "snapshot")]
   #[derive(Clone, Debug, bincode_next::Encode, bincode_next::Decode)]
   ```

3. save_backend_state() (~line 261): Change `bincode::serialize(&state)` → `snapshot_serde::serialize(&state)`. Keep `.map_err(|e| log::error!(...)).ok()`.

4. restore_backend_state() (~line 269): Change `bincode::deserialize(data)` → `snapshot_serde::deserialize::<VhostUserFsState, { MAX_SNAPSHOT_BYTES }>(data)`. Keep `match` error handling.

5. **Update tests** (~lines 646-706): Two tests directly call `bincode::serialize`/`bincode::deserialize`:
   - `test_fs_state_roundtrip` (~line 635): Change to `snapshot_serde::serialize`/`snapshot_serde::deserialize::<VhostUserFsState, { MAX_SNAPSHOT_BYTES }>`
   - `test_restore_backend_state_stores_pending` (~line 678): Change to `snapshot_serde::serialize`

**Verification:**
```bash
cargo test --features snapshot,vhost-user -p devices -- vhost_user::fs
```
Expected: All tests pass.

**Commit:** `refactor(devices): migrate vhost-user fs snapshot to bincode-next`

<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Verify all virtio device tests pass

**Verifies:** safe-snapshot-serde.AC1.3, safe-snapshot-serde.AC1.4

**Files:** None (verification only)

**Step 1: Run all device tests**

```bash
cargo test --features snapshot,vhost-user,blk -p devices
```

Expected: All tests pass.

**Step 2: Run check**

```bash
just check
```

Expected: Format + clippy pass.

**Commit:** None (verification only). If any fixes needed, commit as `fix(devices): ...`.

<!-- END_TASK_5 -->

<!-- END_SUBCOMPONENT_B -->
