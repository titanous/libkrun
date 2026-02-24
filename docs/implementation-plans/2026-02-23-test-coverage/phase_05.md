# Test Coverage Implementation Plan — Phase 5

**Goal:** Wire the `libkrun` Rust crate into the integration test workspace so subsequent phases can use `Builder` / `Context` / `VmHandle` directly.

**Architecture:** Add `libkrun` as an optional dependency under the `host` feature in `tests/test_cases/Cargo.toml`. Create `tests/test_cases/src/krun_rust.rs` with helpers that mirror `common.rs` (`setup_fs_and_enter`) but use the Rust API instead of the C FFI. The Rust lib crate name is `krun` (set in `src/libkrun/Cargo.toml` via `[lib] name = "krun"`), so it is imported in code as `use krun::...`.

**Tech Stack:** Rust, Cargo workspace, `libkrun` crate (path dep), host/guest feature split.

**Scope:** Phase 5 of 8 phases

**Codebase verified:** 2026-02-23

---

## Acceptance Criteria Coverage

This phase implements infrastructure only. No feature ACs are tested here.

**Verifies: None** — this is an infrastructure phase. Done when `cargo build --features host -p test_cases` succeeds with the new dependency.

---

## Codebase Findings (Phase 5 Investigation)

### tests/test_cases/Cargo.toml (current state)

```toml
[features]
host = ["krun-sys"]
guest = []

[dependencies]
krun-sys = { path = "../../krun-sys", optional = true }
macros = { path = "../macros" }
nix = { version = "0.29.0", features = ["socket"] }
anyhow = "1.0.95"
tempdir = "0.3.7"
```

### src/libkrun/Cargo.toml

- `name = "libkrun"` (package name) but `[lib] name = "krun"` → imported in Rust as `use krun::...`
- Features: `embedded_init`, `net`, `blk`, `snapshot`
- crate-type: `["cdylib", "lib"]` — can be used as rlib

### common.rs pattern (C API)

```rust
pub fn setup_fs_and_enter(ctx: u32, test_setup: TestSetup) -> anyhow::Result<()> {
    // creates root dir, copies guest-agent, calls krun_start_enter()
    unreachable!()
}
```

### How Rust API works (key difference from C API)

```rust
// In start_vm():
let mut builder = krun::Builder::new();
// ... configure ...
let context = builder.build()?;   // returns Context
let handle = context.vm_handle(); // must call BEFORE run()
context.run()?;                   // blocks until VM exits (or never returns; process exits at OS level)
```

When the guest exits, the VM halts and the host process exits at OS level (same as `krun_start_enter`). The guest's stdout is forwarded to the child process stdout (through the virtio console), so guest `println!("OK")` appears in child process stdout — which `check()` validates.

### Key public Builder methods (confirmed)

- `Builder::new()` — constructor
- `.vm_config(&mut self, num_vcpus: u8, ram_mib: u32) -> &mut Self`
- `.set_root(&mut self, root_path: &str) -> &mut Self` — NOT `set_root_virtiofs`
- `.workdir(&mut self, workdir: String) -> &mut Self`
- `.exec_path(&mut self, exec_path: String) -> &mut Self`
- `.add_vsock_port(&mut self, port: u32, filepath: PathBuf, listen: bool) -> &mut Self`
- `.add_net_device(&mut self, backend: VirtioNetBackend, mac: [u8; 6], features: u32) -> &mut Self`
- `.add_block_cfg(&mut self, block_cfg: BlockDeviceConfig) -> &mut Self`
- `.build(self) -> Result<Context, StartError>`

### Context methods

- `.device_info(&self) -> &vmm::resources::VmDeviceInfo`
- `.vm_handle(&self) -> VmHandle` — must call before `run()`
- `.run(mut self) -> Result<(), StartError>` — consumes self, blocks until VM exits
- `.restore_and_run(mut self, base_path, incremental_paths) -> Result<(), StartError>`

---

## Tasks

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Add libkrun dependency to test_cases/Cargo.toml

**Verifies:** None (infrastructure)

**Files:**
- Modify: `tests/test_cases/Cargo.toml`

**Implementation:**

Add `libkrun` as an optional dependency enabled by the `host` feature. All four features are needed across phases 6–8:
- `embedded_init` — required to boot VMs in tests (embeds init binary)
- `net` — required for Phase 8 net proxy test
- `blk` — required for Phase 7 custom block backend test
- `snapshot` — required for Phase 6 snapshot tests

Change the `[features]` section and `[dependencies]` section:

```toml
[features]
host = ["krun-sys", "dep:libkrun"]
guest = []

[dependencies]
krun-sys = { path = "../../krun-sys", optional = true }
libkrun = { path = "../../src/libkrun", optional = true, features = ["embedded_init", "net", "blk", "snapshot"] }
macros = { path = "../macros" }
nix = { version = "0.29.0", features = ["socket"] }
anyhow = "1.0.95"
tempdir = "0.3.7"
```

**Verification:**

Run: `cargo build --features host -p test_cases`
Expected: Builds without errors. If there are dependency version conflicts, resolve them by checking which versions are compatible with the workspace.

**Commit:** `chore(test_cases): add libkrun rlib dependency under host feature`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Create krun_rust.rs helper module

**Verifies:** None (infrastructure)

**Files:**
- Create: `tests/test_cases/src/krun_rust.rs`
- Modify: `tests/test_cases/src/lib.rs` — add `#[cfg(feature = "host")] mod krun_rust;`

**Implementation:**

Create `krun_rust.rs` with host-side helpers that mirror `common.rs` but use the Rust API. The helper configures the root virtiofs, copies the guest-agent binary, and sets up exec — all the same work as `setup_fs_and_enter` in `common.rs`, but working with a `krun::Builder` instead of a C context ID.

The module is gated on `feature = "host"` since it imports `krun::Builder`.

```rust
//! Host-side helpers for integration tests using the Rust libkrun API.
//!
//! Mirrors `common.rs` which serves C-API tests. Use these helpers to set up
//! a `krun::Builder` with the standard test root filesystem and guest-agent.

use anyhow::Context as AnyhowContext;
use std::fs;
use std::fs::create_dir;
use std::path::Path;

use crate::TestSetup;

fn copy_guest_agent(dir: &Path) -> anyhow::Result<()> {
    let path = std::env::var_os("KRUN_TEST_GUEST_AGENT_PATH")
        .context("KRUN_TEST_GUEST_AGENT_PATH env variable not set")?;
    let output_path = dir.join("guest-agent");
    fs::copy(path, output_path).context("Failed to copy executable into vm")?;
    Ok(())
}

/// Configure `builder` with:
/// - a virtiofs root at `test_setup.tmp_dir/root` containing the guest-agent binary
/// - workdir = "/"
/// - exec_path = "/guest-agent" with the test case name as the argument
///
/// Call this before `builder.build()`. The returned `Context` can then be
/// run with `context.run()` or used with `context.vm_handle()` first.
pub fn setup_fs_builder(
    builder: &mut krun::Builder,
    test_setup: &TestSetup,
) -> anyhow::Result<()> {
    let root_dir = test_setup.tmp_dir.join("root");
    create_dir(&root_dir).context("Failed to create root directory")?;
    copy_guest_agent(&root_dir)?;

    builder.set_root(root_dir.to_str().context("root_dir path is not valid UTF-8")?);
    builder.workdir("/".to_string());
    builder.exec_path("/guest-agent".to_string());
    builder.args(test_setup.test_case.clone());

    Ok(())
}
```

Then add to `lib.rs`:
```rust
#[cfg(feature = "host")]
mod krun_rust;
#[cfg(feature = "host")]
pub use krun_rust::*;
```

**Test case name routing:** The `Builder` has a confirmed `args(String) -> &mut Self` method (`src/libkrun/src/lib.rs` line 2421). Call it with the test case name (as shown above: `builder.args(test_setup.test_case.clone())`). The C API equivalent (`krun_set_exec`) passes the test case name as `argv[0]`; the `args()` method routes it through the kernel command line to the guest init, which passes it to the guest-agent as its first argument. The guest-agent uses this argument to look up which test to run.

**Verification:**

Run: `cargo build --features host -p test_cases`
Expected: Builds without errors. No existing tests should be broken.

**Commit:** `feat(test_cases): add krun_rust helper module for Rust API integration tests`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Verify build and check test runner compiles

**Files:** None (verification only)

**Step 1: Build host and guest binaries**

Run: `cargo build --features host -p test_cases && cargo build --features guest -p test_cases`
Expected: Both compile without errors.

**Step 2: Verify runner still works**

Run: `make test` (or the equivalent test runner command used in this project)
Expected: Existing tests still pass (6 tests: configure-vm-1cpu-256MiB, configure-vm-2cpu-1GiB, vsock-guest-connect, tsi-tcp-guest-connect, tsi-tcp-guest-listen, multiport-console). No regressions.

**Commit:** (no new commit if already clean)
<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->
