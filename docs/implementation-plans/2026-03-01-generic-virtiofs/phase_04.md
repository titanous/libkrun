# Generic Virtiofs Implementation Plan — Phase 4

**Goal:** Make the `Fs` device and `FsWorker` accept any `FileSystem` via `Box<dyn FileSystem>`, and make `Server` non-generic.

**Architecture:** Replace the `passthrough_cfg: passthrough::Config` field in `Fs` with `fs_backend: Option<Box<dyn FileSystem + Send>>`. The caller constructs the `PassthroughFs` (or any other backend) and passes it to `Fs::new()`. `FsWorker` receives the backend via `activate()` and passes it to a non-generic `Server`. `PassthroughFs::new()` moves from `FsWorker::new()` to the caller.

**Tech Stack:** Rust (trait objects, Box<dyn>, Option::take)

**Scope:** 6 phases from original design (phase 4 of 6, covers design phase 5)

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase implements and tests:

### generic-virtiofs.AC2: Fs device accepts any FileSystem impl
- **generic-virtiofs.AC2.1 Success:** `Fs::new()` accepts `Box<dyn FileSystem>` and stores it
- **generic-virtiofs.AC2.2 Success:** `Fs::activate()` transfers backend ownership to FsWorker via `Option::take()`

### generic-virtiofs.AC6: Server holds Box\<dyn FileSystem\>
- **generic-virtiofs.AC6.1 Success:** `Server` is no longer generic (`Server` not `Server<F>`)
- **generic-virtiofs.AC6.2 Success:** FUSE_SETUPMAPPING dispatch creates `LinuxDaxMapper` from `VirtioShmRegion` and passes to FileSystem (dispatch logic set up in Phase 2 Task 4, verified here with non-generic Server)

---

**NOTE: Tasks 1–3 form an atomic change. Verify with Task 4.**

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->

<!-- START_TASK_1 -->
### Task 1: Make Server non-generic

**Verifies:** generic-virtiofs.AC6.1

**Files:**
- Modify: `src/devices/src/virtio/fs/server.rs:68-79`

**Implementation:**

Change `Server` from generic to holding a trait object.

From (line 68):
```rust
pub struct Server<F: FileSystem + Sync> {
    fs: F,
    options: AtomicU64,
}

impl<F: FileSystem + Sync> Server<F> {
    pub fn new(fs: F) -> Server<F> {
        Server {
            fs,
            options: AtomicU64::new(FsOptions::empty().bits()),
        }
    }
```

To:
```rust
pub struct Server {
    fs: Box<dyn FileSystem + Send + Sync>,
    options: AtomicU64,
}

impl Server {
    pub fn new(fs: Box<dyn FileSystem + Send + Sync>) -> Server {
        Server {
            fs,
            options: AtomicU64::new(FsOptions::empty().bits()),
        }
    }
```

Remove the `impl<F: FileSystem + Sync>` generic parameter from the impl block. All methods now work through `self.fs` as `&dyn FileSystem` via auto-deref on `Box`.

The `.into()` calls on `in_header.nodeid` and `fh` throughout the dispatch methods already return `Inode`/`Handle` types (from Phase 2), so they continue to work.

**Verification:** N/A — verify with Task 4.

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Update Fs device to accept Box<dyn FileSystem>

**Verifies:** generic-virtiofs.AC6.1 (partial — device side)

**Files:**
- Modify: `src/devices/src/virtio/fs/device.rs`

**Implementation:**

**2a. Change the `Fs` struct** (line 42):

Replace `passthrough_cfg: passthrough::Config` with:
```rust
fs_backend: Option<Box<dyn FileSystem + Send + Sync>>,
```

Add import: `use super::filesystem::FileSystem;`

Remove import of `passthrough` if it was only used for `Config` (check whether `passthrough` is still needed for anything else in device.rs — it shouldn't be after this change).

**2b. Change `Fs::new()` constructor** (line 57):

From:
```rust
pub fn new(
    fs_id: String,
    shared_dir: String,
    exit_code: Arc<AtomicI32>,
    allow_root_dir_delete: bool,
) -> super::Result<Fs>
```

To:
```rust
pub fn new(
    fs_id: String,
    fs_backend: Box<dyn FileSystem + Send + Sync>,
    exit_code: Arc<AtomicI32>,
) -> super::Result<Fs>
```

The caller now constructs `PassthroughFs` and passes it in. Remove `passthrough::Config` construction from the body.

In the struct initialization:
```rust
// BEFORE:
passthrough_cfg: fs_cfg,

// AFTER:
fs_backend: Some(fs_backend),
```

**2c. Update `set_export_table`** (line 99):

Replace the method that wrote to `self.passthrough_cfg` with delegation to the backend:
```rust
// BEFORE:
pub fn set_export_table(&mut self, export_table: ExportTable) -> u64 {
    static FS_UNIQUE_ID: AtomicU64 = AtomicU64::new(0);
    self.passthrough_cfg.export_fsid = FS_UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
    self.passthrough_cfg.export_table = Some(export_table);
    self.passthrough_cfg.export_fsid
}

// AFTER:
pub fn set_export_table(&mut self, export_table: ExportTable) -> u64 {
    self.fs_backend
        .as_mut()
        .expect("fs_backend already taken")
        .set_export_table(export_table)
}
```

This delegates to the `FileSystem::set_export_table` method added in Phase 2. `PassthroughFs` implements it with the original logic; other backends get the default no-op.

**2d. Update `activate()` method** (line 179):

Instead of passing `self.passthrough_cfg.clone()` to `FsWorker::new()`, use `Option::take()`:

```rust
let fs_backend = self.fs_backend.take().expect("fs_backend already taken");

let worker = FsWorker::new(
    worker_queues,
    queue_evts,
    interrupt.clone(),
    mem.clone(),
    self.shm_region.clone(),
    fs_backend,       // was: self.passthrough_cfg.clone()
    self.worker_stopfd.try_clone().unwrap(),
    self.exit_code.clone(),
);
```

**Verification:** N/A — verify with Task 4.

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Update FsWorker to accept Box<dyn FileSystem>

**Files:**
- Modify: `src/devices/src/virtio/fs/worker.rs`

**Implementation:**

**3a. Change the `FsWorker` struct** (line 22):

Replace `server: Server<PassthroughFs>` with:
```rust
server: Server,
```

Remove import of `PassthroughFs` (e.g., `use super::linux::passthrough::PassthroughFs;` or similar).

Add import: `use super::filesystem::FileSystem;`

**3b. Change `FsWorker::new()` constructor** (line 36):

Replace `passthrough_cfg: passthrough::Config` parameter with:
```rust
fs_backend: Box<dyn FileSystem + Send + Sync>,
```

In the body, change:
```rust
// BEFORE:
server: Server::new(PassthroughFs::new(passthrough_cfg).unwrap()),

// AFTER:
server: Server::new(fs_backend),
```

`PassthroughFs::new()` no longer happens here — it moved to the caller (the Builder API, Phase 5).

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles. `Server` is non-generic. `Fs` accepts `Box<dyn FileSystem>`.

**Commit:** `feat(virtiofs): make Fs device and Server accept any FileSystem backend`

<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_TASK_4 -->
### Task 4: Verify the full generic path works

**Files:**
- No file changes (verification only)

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles cleanly

Confirm the construction pattern works:
```rust
// This pattern should now be valid:
// let pt = PassthroughFs::new(cfg).unwrap();
// let fs = Fs::new("myfs".to_string(), Box::new(pt), exit_code).unwrap();
```

Run: `cargo test -p devices`
Expected: All existing tests pass

**Commit:** If fixups needed, `fix(virtiofs): resolve generic Fs device issues`

<!-- END_TASK_4 -->
