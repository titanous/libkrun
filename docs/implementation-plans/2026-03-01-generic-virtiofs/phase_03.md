# Generic Virtiofs Implementation Plan — Phase 3

**Goal:** Remove all macOS-specific virtiofs code from the `src/devices/src/virtio/fs/` tree.

**Architecture:** Delete the `macos/` directory, remove `map_sender` fields and `#[cfg(target_os = "macos")]` blocks from device, worker, and fuse modules. After Phase 2 removed macOS references from `filesystem.rs` and `server.rs`, this phase completes the cleanup.

**Tech Stack:** Rust (code deletion)

**Scope:** 6 phases from original design (phase 3 of 6, covers design phase 4)

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase is infrastructure cleanup. No specific ACs — it removes dead code remaining after Phase 2's trait changes.

**Verifies: None** (infrastructure cleanup, verified operationally by build succeeding)

---

<!-- START_TASK_1 -->
### Task 1: Delete macos/ directory

**Files:**
- Delete: `src/devices/src/virtio/fs/macos/mod.rs`
- Delete: `src/devices/src/virtio/fs/macos/fs_utils.rs`
- Delete: `src/devices/src/virtio/fs/macos/passthrough.rs`

**Implementation:**

Delete the entire `src/devices/src/virtio/fs/macos/` directory. These files contain the macOS-specific `PassthroughFs` implementation (2,596 lines) and helpers that are no longer referenced after Phase 2.

```bash
rm -r src/devices/src/virtio/fs/macos/
```

**Verification:** N/A — verify with final build.

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Remove macOS references from mod.rs

**Files:**
- Modify: `src/devices/src/virtio/fs/mod.rs:16-21`

**Implementation:**

Delete the three `#[cfg(target_os = "macos")]` blocks (lines 16-21):
```rust
// DELETE these 6 lines:
// #[cfg(target_os = "macos")]
// pub mod macos;
// #[cfg(target_os = "macos")]
// pub use macos::fs_utils;
// #[cfg(target_os = "macos")]
// pub use macos::passthrough;
```

Also remove the `#[cfg(target_os = "linux")]` guards from the linux module declarations (lines 11-15), since Linux is now the only platform:
```rust
// BEFORE:
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux::fs_utils;
#[cfg(target_os = "linux")]
pub use linux::passthrough;

// AFTER:
pub mod linux;
pub use linux::fs_utils;
pub use linux::passthrough;
```

**Verification:** N/A — verify with final build.

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Remove map_sender from device.rs

**Files:**
- Modify: `src/devices/src/virtio/fs/device.rs`

**Implementation:**

Remove macOS imports (lines 1, 10-11):
```rust
// DELETE:
// #[cfg(target_os = "macos")]
// use crossbeam_channel::Sender;
// #[cfg(target_os = "macos")]
// use utils::worker_message::WorkerMessage;
```

Remove `map_sender` field from the `Fs` struct (lines 52-53):
```rust
// DELETE from struct Fs:
// #[cfg(target_os = "macos")]
// map_sender: Option<Sender<WorkerMessage>>,
```

Remove `map_sender: None` from `Fs::new()` initialization (around line 86-87).

Delete the `set_map_sender()` method (lines 108-111):
```rust
// DELETE:
// #[cfg(target_os = "macos")]
// pub fn set_map_sender(&mut self, map_sender: Sender<WorkerMessage>) {
//     self.map_sender = Some(map_sender);
// }
```

Remove `self.map_sender.clone()` from the `activate()` method (around lines 188-189). This was being passed to `FsWorker::new()` — remove the argument.

**Verification:** N/A — verify with final build.

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Remove map_sender from worker.rs

**Files:**
- Modify: `src/devices/src/virtio/fs/worker.rs`

**Implementation:**

Remove macOS imports (lines 1-4):
```rust
// DELETE:
// #[cfg(target_os = "macos")]
// use crossbeam_channel::Sender;
// #[cfg(target_os = "macos")]
// use utils::worker_message::WorkerMessage;
```

Remove `map_sender` field from `FsWorker` struct (lines 31-32):
```rust
// DELETE from struct FsWorker:
// #[cfg(target_os = "macos")]
// map_sender: Option<Sender<WorkerMessage>>,
```

Remove `map_sender` from `FsWorker::new()` parameter list (line 46) and field initialization (lines 57-58).

Remove `&self.map_sender` from the `handle_message` call (around lines 163-164). After Phase 2, `handle_message` no longer accepts this parameter.

**Verification:** N/A — verify with final build.

<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Remove macOS-specific code from fuse.rs

**Files:**
- Modify: `src/devices/src/virtio/fs/fuse.rs`

**Implementation:**

Remove the macOS `From<bindings::statvfs64> for Kstatfs` impl block (lines 634-644). Only the `#[cfg(target_os = "linux")]` version remains — remove its cfg guard.

For the Stat struct conversion fields (around lines 582-592), remove the macOS-specific branches:
```rust
// BEFORE:
#[cfg(target_os = "linux")]
mode: st.st_mode,
#[cfg(target_os = "macos")]
mode: st.st_mode as u32,

// AFTER:
mode: st.st_mode,
```

Same for `nlink` — remove the macOS cast, keep the Linux version, and remove the cfg guard.

Remove the `#[cfg(target_os = "linux")]` guards from the remaining Linux-only code since it's now unconditional.

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles with no `#[cfg(target_os = "macos")]` remaining in `src/devices/src/virtio/fs/`

Run: `grep -r 'target_os = "macos"' src/devices/src/virtio/fs/`
Expected: No matches

**Commit:** `refactor(virtiofs): remove all macOS-specific virtiofs code`

<!-- END_TASK_5 -->
