# Generic Virtiofs Implementation Plan — Phase 5

**Goal:** Wire up the Builder API and VMM builder to pass `Box<dyn FileSystem>` through to device creation.

**Architecture:** Introduce `FsMount { tag, fs, shm_size }` to hold a pre-built filesystem backend as a trait object. The Rust Builder API accepts `Box<dyn FileSystem>` directly. C API functions create `PassthroughFs` internally and wrap it. VmResources stores `Vec<FsMount>`. During `build_microvm`, `attach_fs_devices` drains the boxed backends and passes them to `Fs::new()`.

**Tech Stack:** Rust (trait objects, FFI)

**Scope:** 6 phases from original design (phase 5 of 6, covers design phase 6)

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase implements and tests:

### generic-virtiofs.AC7: Builder API supports custom backends
- **generic-virtiofs.AC7.1 Success:** `Builder::add_virtiofs(tag, Box<dyn FileSystem>, Option<usize>)` registers a filesystem mount
- **generic-virtiofs.AC7.2 Success:** DAX window is allocated via ShmManager when `shm_size` is `Some`
- **generic-virtiofs.AC7.3 Success:** No DAX window allocated when `shm_size` is `None`
- **generic-virtiofs.AC7.4 Success:** Multiple `add_virtiofs()` calls create independent devices with separate DAX windows

---

<!-- START_TASK_1 -->
### Task 1: Create FsMount struct and update VmResources

**Verifies:** generic-virtiofs.AC7.1 (partial — storage)

**Files:**
- Modify: `src/vmm/src/vmm_config/fs.rs` (replace `FsDeviceConfig` with `FsMount`)
- Modify: `src/vmm/src/resources.rs` (update `fs` field type and `add_fs_device` method)

**Implementation:**

**1a. Replace FsDeviceConfig with FsMount** in `src/vmm/src/vmm_config/fs.rs`:

```rust
use devices::virtio::fs::FileSystem;

pub struct FsMount {
    pub tag: String,
    pub fs: Box<dyn FileSystem + Send + Sync>,
    pub shm_size: Option<usize>,
}
```

Remove `FsDeviceConfig`. Remove the `#[derive(Clone, Debug)]` — `Box<dyn FileSystem>` is not `Clone` or `Debug`. If other code depends on `FsDeviceConfig`, update those references.

**1b. Update VmResources** in `src/vmm/src/resources.rs`:

Change the field (around line 217):
```rust
// BEFORE:
pub fs: Vec<FsDeviceConfig>,

// AFTER:
pub fs: Vec<FsMount>,
```

Update the `add_fs_device` method (around line 380):
```rust
// BEFORE:
pub fn add_fs_device(&mut self, config: FsDeviceConfig) {
    self.fs.push(config)
}

// AFTER:
pub fn add_fs_mount(&mut self, mount: FsMount) {
    self.fs.push(mount)
}
```

Update imports: replace `FsDeviceConfig` with `FsMount` in the `use` statements.

**Verification:** N/A — verify after all tasks complete.

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Update Builder Rust API

**Verifies:** generic-virtiofs.AC7.1

**Files:**
- Modify: `src/libkrun/src/lib.rs` (update `add_virtiofs` method)

**Implementation:**

Change the `add_virtiofs` method (around line 2549):

```rust
// BEFORE:
pub fn add_virtiofs(&mut self, tag: &str, host_path: &str) -> &mut Self {
    self.config.vmr.add_fs_device(FsDeviceConfig {
        fs_id: tag.to_string(),
        shared_dir: host_path.to_string(),
        shm_size: None,
        allow_root_dir_delete: false,
    });
    self
}

// AFTER:
pub fn add_virtiofs(
    &mut self,
    tag: &str,
    fs: Box<dyn devices::virtio::fs::FileSystem + Send + Sync>,
    shm_size: Option<usize>,
) -> &mut Self {
    self.config.vmr.add_fs_mount(FsMount {
        tag: tag.to_string(),
        fs,
        shm_size,
    });
    self
}
```

Add a convenience method for backward compatibility (PassthroughFs from path):

```rust
/// Add a virtiofs device backed by a host directory (passthrough).
pub fn add_virtiofs_path(
    &mut self,
    tag: &str,
    host_path: &str,
    shm_size: Option<usize>,
    allow_root_dir_delete: bool,
) -> &mut Self {
    let cfg = devices::virtio::fs::passthrough::Config {
        root_dir: host_path.to_string(),
        allow_root_dir_delete,
        ..Default::default()
    };
    let pt = devices::virtio::fs::passthrough::PassthroughFs::new(cfg)
        .expect("failed to create PassthroughFs");
    self.add_virtiofs(tag, Box::new(pt), shm_size)
}
```

Update imports to include `FsMount` from vmm_config::fs.

**Verification:** N/A — verify after all tasks complete.

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Update C API functions

**Files:**
- Modify: `src/libkrun/src/lib.rs` (update `krun_add_virtiofs`, `krun_add_virtiofs2`, and `krun_mount_virtiofs`)

**Implementation:**

The C API functions cannot accept `Box<dyn FileSystem>` over FFI, so they create `PassthroughFs` internally.

**3a. Update `krun_add_virtiofs`** (around line 548):
```rust
pub unsafe extern "C" fn krun_add_virtiofs(
    ctx_id: u32,
    c_tag: *const c_char,
    c_path: *const c_char,
) -> i32 {
    let tag = match CStr::from_ptr(c_tag).to_str() {
        Ok(tag) => tag,
        Err(_) => return -libc::EINVAL,
    };
    let path = match CStr::from_ptr(c_path).to_str() {
        Ok(path) => path,
        Err(_) => return -libc::EINVAL,
    };

    with_builder(ctx_id, |cfg| {
        cfg.add_virtiofs_path(tag, path, None, false);
        KRUN_SUCCESS
    })
}
```

**3b. Update `krun_add_virtiofs2`** (around line 576):
```rust
pub unsafe extern "C" fn krun_add_virtiofs2(
    ctx_id: u32,
    c_tag: *const c_char,
    c_path: *const c_char,
    shm_size: u64,
) -> i32 {
    // ... same tag/path parsing ...
    with_builder(ctx_id, |cfg| {
        cfg.add_virtiofs_path(tag, path, Some(shm_size.try_into().unwrap()), false);
        KRUN_SUCCESS
    })
}
```

**3c. Update `krun_set_root_disk_remount`** (around line 2162) to use `add_virtiofs_path`:
```rust
cfg.add_virtiofs_path("/dev/root", &empty_root.to_string_lossy(), Some(1 << 29), true);
```

Also update the guard condition at line 2140 that checks for existing root fs — change `fs.fs_id` to `fs.tag`:
```rust
// BEFORE:
if cfg.config.vmr.fs.iter().any(|fs| fs.fs_id == "/dev/root") {
// AFTER:
if cfg.config.vmr.fs.iter().any(|fs| fs.tag == "/dev/root") {
```

**Verification:** N/A — verify after all tasks complete.

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Update attach_fs_devices and create_guest_memory

**Verifies:** generic-virtiofs.AC7.2, generic-virtiofs.AC7.3, generic-virtiofs.AC7.4

**Files:**
- Modify: `src/vmm/src/builder.rs` (update `attach_fs_devices` and SHM allocation in `create_guest_memory`)

**Implementation:**

**4a. Update SHM region allocation** in `create_guest_memory` (around line 1891):

```rust
// BEFORE:
for (index, fs) in vm_resources.fs.iter().enumerate() {
    if let Some(shm_size) = fs.shm_size {
        shm_manager.create_fs_region(index, shm_size)
            .map_err(StartMicrovmError::ShmCreate)?;
    }
}

// AFTER (same logic, just field access changes):
for (index, mount) in vm_resources.fs.iter().enumerate() {
    if let Some(shm_size) = mount.shm_size {
        shm_manager.create_fs_region(index, shm_size)
            .map_err(StartMicrovmError::ShmCreate)?;
    }
}
```

**4b. Update `attach_fs_devices`** (around line 2374):

Change the function signature:
```rust
fn attach_fs_devices(
    vmm: &mut Vmm,
    fs_mounts: &mut Vec<FsMount>,  // mutable to drain backends
    shm_manager: &mut ShmManager,
    #[cfg(not(feature = "tee"))] export_table: Option<ExportTable>,
    intc: IrqChip,
    exit_code: Arc<AtomicI32>,
) -> std::result::Result<(), StartMicrovmError> {
```

Remove the `map_sender` parameter (macOS gone after Phase 3). Keep `export_table` — it is set on the `Fs` device after construction, which delegates to the `FileSystem` backend (via `set_export_table` added in Phase 2).

Update the body to drain backends from FsMount:
```rust
for (i, mount) in fs_mounts.drain(..).enumerate() {
    let fs = Arc::new(Mutex::new(
        devices::virtio::Fs::new(
            mount.tag,
            mount.fs,  // Box<dyn FileSystem> from FsMount
            exit_code.clone(),
        )
        .unwrap(),
    ));

    let id = format!("{}{}", String::from(fs.lock().unwrap().id()), i);

    if let Some(shm_region) = shm_manager.fs_region(i) {
        fs.lock().unwrap().set_shm_region(VirtioShmRegion {
            host_addr: vmm
                .guest_memory
                .get_host_address(shm_region.guest_addr)
                .map_err(StartMicrovmError::ShmHostAddr)? as u64,
            guest_addr: shm_region.guest_addr.raw_value(),
            size: shm_region.size,
        });
    }

    #[cfg(not(feature = "tee"))]
    if let Some(export_table) = export_table.as_ref() {
        fs.lock().unwrap().set_export_table(export_table.clone());
    }

    attach_mmio_device(vmm, id, intc.clone(), fs).map_err(RegisterFsDevice)?;
}
```

The `set_export_table` call on `Fs` now delegates to the `FileSystem::set_export_table` method on the boxed backend (Phase 4 Task 2c). `PassthroughFs` stores the export table in its config; other backends get the default no-op.

**4c. Update the `build_microvm` call** (around line 1346):

```rust
// BEFORE:
attach_fs_devices(
    &mut vmm,
    &vm_resources.fs,
    &mut _shm_manager,
    export_table,
    intc.clone(),
    exit_code,
)?;

// AFTER:
attach_fs_devices(
    &mut vmm,
    &mut vm_resources.fs,  // mutable for drain
    &mut _shm_manager,
    export_table,           // kept for GPU feature
    intc.clone(),
    exit_code,
)?;
```

Keep the `export_table` variable creation — it is still used by `attach_fs_devices`.

Update imports throughout builder.rs to use `FsMount` instead of `FsDeviceConfig`.

**Verification:**
Run: `cargo check -p vmm`
Run: `cargo check -p libkrun`
Expected: Both compile cleanly

**Commit:** `feat(virtiofs): wire Builder API to accept generic FileSystem backends`

<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Verify end-to-end construction path

**Files:**
- No file changes (verification only)

**Verification:**
Run: `cargo check -p devices && cargo check -p vmm && cargo check -p libkrun`
Expected: Full workspace compiles

The construction pattern should work:
```rust
// Rust API:
let cfg = passthrough::Config { root_dir: "/tmp".into(), ..Default::default() };
let pt = PassthroughFs::new(cfg).unwrap();
builder.add_virtiofs("myfs", Box::new(pt), Some(256 * 1024 * 1024));

// C API (backward compatible):
krun_add_virtiofs(ctx, "myfs\0", "/tmp\0");
krun_add_virtiofs2(ctx, "myfs\0", "/tmp\0", 256 * 1024 * 1024);
```

**Commit:** If fixups needed, `fix: resolve Builder API integration issues`

<!-- END_TASK_5 -->
