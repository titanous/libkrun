# Generic Virtiofs Implementation Plan — Phase 6

**Goal:** Fix compilation issues from the refactored API, re-export filesystem types for external consumers, write an integration test exercising the generic `Box<dyn FileSystem>` path, and verify the full test suite passes.

**Architecture:** Update `set_root` to use the new `add_virtiofs_path` convenience method. Re-export `FileSystem`, `passthrough`, and `DaxMapper` from the `krun` (libkrun) crate so integration tests and external consumers can construct custom backends. Write a new integration test that constructs `PassthroughFs` manually, boxes it, and passes it through `Builder::add_virtiofs()` to prove the entire generic pipeline works end-to-end.

**Tech Stack:** Rust (integration tests, host/guest proc macros)

**Scope:** 6 phases from original design (phase 6 of 6, covers design phase 7)

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase implements and tests:

### generic-virtiofs.AC3: FileSystem trait is publicly exported
- **generic-virtiofs.AC3.1 Success:** External crate can import `FileSystem` trait from `devices` crate
- **generic-virtiofs.AC3.2 Success:** All types referenced in FileSystem method signatures are also exported

### generic-virtiofs.AC8: Existing integration tests pass
- **generic-virtiofs.AC8.1 Success:** `make test FEATURE_FLAGS="--features embedded_init"` passes with virtiofs tests exercising the new generic path

---

**NOTE: Tasks 1–2 fix compilation. Task 3–4 add the new test. Task 5 runs the full suite.**

<!-- START_TASK_1 -->
### Task 1: Update `set_root` and `krun_set_root_disk_remount` to compile after Phase 5 changes

**Verifies:** generic-virtiofs.AC8.1 (partial — compilation)

**Files:**
- Modify: `src/libkrun/src/lib.rs:2657-2670` (`set_root` method)
- Modify: `src/libkrun/src/lib.rs:2140-2168` (`krun_set_root_disk_remount` function)

**Implementation:**

After Phase 5 removes `FsDeviceConfig` and renames `add_fs_device` to `add_fs_mount`, both `set_root` and `krun_set_root_disk_remount` will no longer compile because they construct `FsDeviceConfig` directly.

**1a. Update `set_root`** (line 2657):

Replace the method body to delegate to `add_virtiofs_path` (added in Phase 5 Task 2):

```rust
// BEFORE (line 2657):
pub fn set_root(&mut self, root_path: &str) -> &mut Self {
    let fs_id = "/dev/root".to_string();
    let shared_dir = root_path.to_string();

    self.config.vmr.add_fs_device(FsDeviceConfig {
        fs_id,
        shared_dir,
        // Default to a conservative 512 MB window.
        shm_size: Some(1 << 29),
        allow_root_dir_delete: false,
    });

    self
}

// AFTER:
pub fn set_root(&mut self, root_path: &str) -> &mut Self {
    self.add_virtiofs_path("/dev/root", root_path, Some(1 << 29), false)
}
```

**1b. Update `krun_set_root_disk_remount`** (line 2140-2168):

Update the guard condition that checks for existing root fs (line 2140) — `fs.fs_id` becomes `fs.tag` after FsMount:
```rust
// BEFORE:
if cfg.config.vmr.fs.iter().any(|fs| fs.fs_id == "/dev/root") {
// AFTER:
if cfg.config.vmr.fs.iter().any(|fs| fs.tag == "/dev/root") {
```

Update the FsDeviceConfig construction (line 2162) to use `add_virtiofs_path`:
```rust
// BEFORE:
cfg.config.vmr.add_fs_device(FsDeviceConfig {
    fs_id: "/dev/root".into(),
    shared_dir: empty_root.to_string_lossy().into(),
    shm_size: Some(1 << 29),
    allow_root_dir_delete: true,
});

// AFTER:
cfg.add_virtiofs_path("/dev/root", &empty_root.to_string_lossy(), Some(1 << 29), true);
```

**1c.** Remove `FsDeviceConfig` from the imports in `lib.rs` if no other method references it after Phases 5 and this change. Replace with `FsMount` import if not already present.

**Verification:** N/A — verify with Task 5.

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Re-export filesystem types from libkrun crate

**Verifies:** generic-virtiofs.AC3.1, generic-virtiofs.AC3.2

**Files:**
- Modify: `src/libkrun/src/lib.rs` (add `pub use` re-exports near existing ones, around line 8-62)

**Implementation:**

Add re-exports so that external consumers (including integration tests) can construct custom `FileSystem` implementations and `PassthroughFs` backends through the `krun` crate:

```rust
// Add after existing `pub use devices::` lines (around line 31):
pub use devices::virtio::fs::dax_mapper;
pub use devices::virtio::fs::filesystem::FileSystem;
pub use devices::virtio::fs::passthrough;
```

These re-exports depend on:
- `dax_mapper` module being public in `fs/mod.rs` (Phase 1 creates this module)
- `filesystem` module being public in `fs/mod.rs` (Phase 2 should make this public for AC3)
- `passthrough` module already being public (Phase 3 removes the `#[cfg(target_os = "linux")]` guard)

If `mod dax_mapper;` or `mod filesystem;` in `src/devices/src/virtio/fs/mod.rs` are still private (`mod` instead of `pub mod`), change them to `pub mod` now. After Phase 3, `mod.rs` should have:

```rust
pub mod dax_mapper;    // must be pub (Phase 1 creates, may need pub here)
pub mod filesystem;    // must be pub (Phase 2 AC3 requires this)
pub mod fuse;          // already pub
pub mod linux;         // Phase 3 removes cfg guard
pub use linux::fs_utils;
pub use linux::passthrough;
```

**Verification:** N/A — verify with Task 5.

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Write integration test for generic virtiofs passthrough

**Verifies:** generic-virtiofs.AC8.1

**Files:**
- Create: `tests/test_cases/src/test_virtiofs_generic_passthrough.rs`
- Modify: `tests/test_cases/src/lib.rs` (register new test)

**Implementation:**

**3a. Create the test file** at `tests/test_cases/src/test_virtiofs_generic_passthrough.rs`:

This test directly exercises the generic `Builder::add_virtiofs(tag, Box<dyn FileSystem>, shm_size)` API by manually constructing a `PassthroughFs`, boxing it, and using it as the root filesystem. It validates that file I/O works end-to-end through the new trait-object-based device stack.

```rust
use macros::{guest, host};

pub struct TestVirtiofsGenericPassthrough;

#[host]
mod host_impl {
    use super::*;
    use crate::{Test, TestSetup};

    impl Test for TestVirtiofsGenericPassthrough {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            use anyhow::Context;
            use std::fs::{self, create_dir};

            let root_dir = test_setup.tmp_dir.join("root");
            create_dir(&root_dir).context("create root dir")?;

            // Copy guest-agent into root
            let agent_path = std::env::var_os("KRUN_TEST_GUEST_AGENT_PATH")
                .context("KRUN_TEST_GUEST_AGENT_PATH not set")?;
            fs::copy(&agent_path, root_dir.join("guest-agent"))
                .context("copy guest-agent")?;

            // Create a test data file that the guest will read
            fs::write(root_dir.join("test-data.txt"), b"hello from generic virtiofs")
                .context("write test-data.txt")?;

            // Construct PassthroughFs manually and pass via the generic API
            let cfg = krun::passthrough::Config {
                root_dir: root_dir.to_str().unwrap().to_string(),
                ..Default::default()
            };
            let pt = krun::passthrough::PassthroughFs::new(cfg)
                .context("PassthroughFs::new")?;

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 256)?;

            // This is the key call: generic add_virtiofs with Box<dyn FileSystem>
            builder.add_virtiofs(
                "/dev/root",
                Box::new(pt),
                Some(1 << 29), // 512 MiB DAX window
            );

            builder.workdir("/".to_string());
            builder.exec_path("/guest-agent".to_string());
            builder.args(test_setup.test_case.clone());

            let context = builder.build()?;
            let vm_thread = std::thread::spawn(move || context.run());
            vm_thread.join().ok();

            Ok(())
        }
    }
}

#[guest]
mod guest_impl {
    use super::*;
    use crate::Test;

    impl Test for TestVirtiofsGenericPassthrough {
        fn in_guest(self: Box<Self>) {
            use std::fs;

            // 1. Read the test file created by the host through the generic virtiofs path
            let data = fs::read_to_string("/test-data.txt").unwrap();
            assert_eq!(
                data, "hello from generic virtiofs",
                "read mismatch: got {:?}",
                data
            );

            // 2. Write a new file and read it back
            fs::write("/write-test.txt", b"generic virtiofs write test").unwrap();
            let data = fs::read_to_string("/write-test.txt").unwrap();
            assert_eq!(
                data, "generic virtiofs write test",
                "write readback mismatch: got {:?}",
                data
            );

            println!("OK");
        }
    }
}
```

Note: The test uses `krun::passthrough::Config` and `krun::Builder` which require the `krun` (libkrun) crate. The test workspace already has `libkrun` as a dependency (in `tests/test_cases/Cargo.toml`, line 14 — aliased as `krun` via `lib.name = "krun"` in the source crate). The re-exports added in Task 2 make `krun::passthrough` and `krun::FileSystem` available. No Cargo.toml changes needed.

**3b. Register the test** in `tests/test_cases/src/lib.rs`:

Add module declaration (after the existing `mod test_vhost_user_fs;` block, around line 59-62):
```rust
mod test_virtiofs_generic_passthrough;
use test_virtiofs_generic_passthrough::TestVirtiofsGenericPassthrough;
```

Add to the `test_cases()` vector (after the vhost-user-fs entries, around line 148):
```rust
TestCase::new(
    "virtiofs-generic-passthrough",
    Box::new(TestVirtiofsGenericPassthrough),
),
```

**Verification:** N/A — verify with Task 5.

**Commit:** `test(virtiofs): add integration test for generic FileSystem backend`

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Run full workspace check

**Files:**
- No file changes (verification only)

**Verification:**

Run workspace-wide compilation checks to ensure all phases integrate correctly:

```bash
cargo check -p devices
cargo check -p vmm
cargo check -p libkrun
```

Expected: All three compile cleanly with no errors.

If any compilation errors occur (e.g., missing `pub mod` declarations, import mismatches, trait visibility issues), fix them before proceeding to the full test run.

**Commit:** If fixups needed, `fix(virtiofs): resolve compilation issues in generic virtiofs integration`

<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Run full integration test suite

**Verifies:** generic-virtiofs.AC8.1

**Files:**
- No file changes (verification only)

**Verification:**

Run the full integration test suite:
```bash
make test FEATURE_FLAGS="--features embedded_init"
```

Expected: All tests pass, including:
- Existing tests that use `set_root` (which now delegates to `add_virtiofs_path`): `configure-vm-*`, `snapshot-*`, `rust-api-*`, `vm-exit-*`, etc.
- Existing vhost-user-fs tests (unaffected, uses separate vhost-user path): `vhost-user-fs-dax-always`, `vhost-user-fs-dax-inode`, `vhost-user-fs-dax-never`
- New test: `virtiofs-generic-passthrough`

The new test validates that `PassthroughFs` works through the entire generic device stack: `Builder::add_virtiofs(tag, Box<dyn FileSystem>, shm_size)` → `FsMount` → `VmResources` → `attach_fs_devices` → `Fs::new(tag, Box<dyn FileSystem>, exit_code)` → `FsWorker` → `Server` → `PassthroughFs`.

Note: Tests are inherently flaky (VM + network timing). 5-6/6 passing is normal per project conventions. The `virtiofs-generic-passthrough` test itself should pass consistently as it only does basic file I/O.

**Commit:** N/A (verification only; commit any fixups discovered here as `fix(virtiofs): resolve integration test issues`)

<!-- END_TASK_5 -->
