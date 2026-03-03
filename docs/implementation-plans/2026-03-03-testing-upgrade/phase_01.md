# Testing Upgrade Implementation Plan

**Goal:** Remove the 24 `pub extern "C"` C API functions from libkrun, delete the Makefile, create a justfile as the single build/test entry point, and migrate the 5 integration tests that still use the C API to the Rust Builder API.

**Architecture:** Delete the entire C API section from `src/libkrun/src/lib.rs` (lines 408–2409) including `BUILDER_MAP`, `BUILDER_IDS`, helper functions, and all `#[no_mangle] pub extern "C"` functions. Remove `cdylib` crate type. Migrate `common.rs` + 5 test files from krun-sys to Rust `Builder`. Replace Makefile with justfile. Update CLAUDE.md files.

**Tech Stack:** Rust (stable from rust-toolchain.toml), `just` command runner, `cargo` workspace

**Scope:** Phase 1 of 8

**Codebase verified:** 2026-03-03

---

## Acceptance Criteria Coverage

This phase implements and tests:

### testing-upgrade.AC1: C API removed
- **testing-upgrade.AC1.1 Success:** No `pub extern "C"` functions exist in `src/libkrun/src/lib.rs`
- **testing-upgrade.AC1.2 Success:** `Builder`, `Context`, `VmHandle`, and all trait re-exports compile and are publicly accessible from Rust
- **testing-upgrade.AC1.3 Success:** `just check` passes with no C API symbols in the compiled library
- **testing-upgrade.AC1.4 Edge:** Removing C API does not break integration tests (they use Rust API)

### testing-upgrade.AC5: Justfile as test runner
- **testing-upgrade.AC5.1 Success:** `Makefile` does not exist at project root
- **testing-upgrade.AC5.2 Success:** `justfile` exists at project root with all targets documented in Section 9
- **testing-upgrade.AC5.3 Success:** `just build` produces the release library (replaces `make`)
- **testing-upgrade.AC5.4 Success:** `just integration <name>` runs a single named integration test
- **testing-upgrade.AC5.5 Success:** `just all` runs test + miri + proptest + loom + shuttle as a compound target
- **testing-upgrade.AC5.6 Success:** `just safety` runs asan + miri + fuzz-all + kani as a compound target

---

## Investigation Findings

**Discrepancy from design:** The design says delete `CTX_MAP`/`CTX_IDS` globals. Actual names are `BUILDER_MAP`/`BUILDER_IDS` at `src/libkrun/src/lib.rs:408-409`.

**5 integration tests still use C API** (the rest already use Rust `Builder`):
- `tests/test_cases/src/test_vm_config.rs` — uses `krun_set_vm_config`
- `tests/test_cases/src/test_vsock_guest_connect.rs` — uses `krun_add_vsock_port`
- `tests/test_cases/src/test_tsi_tcp_guest_connect.rs` — uses `krun_set_vm_config`
- `tests/test_cases/src/test_tsi_tcp_guest_listen.rs` — uses `krun_set_port_map`
- `tests/test_cases/src/test_multiport_console.rs` — uses `krun_add_virtio_console_*`
- `tests/test_cases/src/common.rs` — `setup_fs_and_enter()` calls C API

**C API → Rust Builder migration pattern** (from `tests/test_cases/src/test_vm_exit.rs`):
```rust
// OLD (C API):
let ctx = krun_call_u32!(krun_create_ctx())?;
krun_call!(krun_set_vm_config(ctx, 1, 256))?;
setup_fs_and_enter(ctx, test_setup)?;  // calls krun_start_enter, never returns

// NEW (Rust API):
use crate::krun_rust::setup_fs_builder;
let mut builder = krun::Builder::new();
builder.vm_config(1, 256)?;
setup_fs_builder(&mut builder, &test_setup)?;
let context = builder.build()?;
context.run()?;
Ok(())
```

**Key Rust Builder methods:**
- `Builder::new()` — constructor
- `builder.vm_config(num_vcpus: u8, ram_mib: u32) -> Result<&mut Self, StartError>` — set VM config
- `builder.add_vsock_port(port: u32, filepath: PathBuf, listen: bool) -> &mut Self` — add vsock port
- `builder.port_map(HashMap<u16, u16>) -> Result<&mut Self, ()>` — set TSI port mapping
- `builder.disable_implicit_console() -> Result<&mut Self, BuilderError>` — disable auto console
- `builder.add_virtio_console() -> ConsoleDeviceInfo` — add multiport console device
- `builder.add_port_console_fd(info, input_fd, output_fd, cols, rows) -> Option<String>` — add console port
- `builder.add_port_fd(info, name, input_fd, output_fd) -> Option<String>` — add named port
- `builder.build() -> Result<Context, StartError>` — build the VM
- `context.run() -> Result<VmExit, StartError>` — enter and run (returns on exit)

**Crate name:** `libkrun` package maps to crate name `krun` (`[lib] name = "krun"` in `src/libkrun/Cargo.toml:55`)

**lib.rs structure for deletion:**
- Lines 408–409: `BUILDER_MAP`, `BUILDER_IDS` statics (delete)
- Lines 411–420: `log_level_to_filter_str` helper (delete)
- Lines 422 onward: `with_builder`, `take_builder`, `add_net_cfg` helpers + all `#[no_mangle] pub extern "C" fn krun_*` + `krun_start_enter_nitro` — everything through line 2409
- Lines 2411+: `ConsoleDeviceInfo`, `Builder`, `Context`, `VmHandle`, `BalloonHandle` — KEEP

**Imports to remove after deletion** (become unused):
- `use libc::{c_char, c_int, size_t};` (C type aliases for function params) — but keep `libc` in scope as `Builder::vmm_uid`/`vmm_gid` use `libc::uid_t`/`libc::gid_t` in their signatures
- `use once_cell::sync::Lazy;` (only for `BUILDER_MAP`)
- `use std::collections::hash_map::Entry;` (only in `with_builder`)
- `use std::ffi::{c_void, CStr, CString};` (C string handling for C API)
- `use std::slice;` (C pointer to slice conversion)
- `use std::sync::atomic::{AtomicI32, Ordering};` (for `BUILDER_IDS`)
- `const KRUN_SUCCESS: i32 = 0;` (C return code)
- `const MAX_ARGS: usize = 4096;` (C API argument limit)
- Run `cargo check --features embedded_init,snapshot,uffd,blk,vhost-user` and fix any remaining unused import warnings

**Crate type:** Change `crate-type = ["cdylib", "lib"]` to `crate-type = ["lib"]` in `src/libkrun/Cargo.toml:56-57` — no longer produces a C shared library.

**justfile `integration` target:** Does not need to build `test-prefix` (no longer installs C library). Only needs `LD_LIBRARY_PATH` pointing to `test-prefix/lib64/` for `libkrunfw.so` (symlinked there by the nix shellHook). Does NOT need `PKG_CONFIG_PATH` (was only for krun-sys).

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Delete C API section from `src/libkrun/src/lib.rs`

**Verifies:** testing-upgrade.AC1.1, testing-upgrade.AC1.2

**Files:**
- Modify: `src/libkrun/src/lib.rs:408-2409`
- Modify: `src/libkrun/Cargo.toml:54`

**Implementation:**

**Step 1: Delete the C API block from lib.rs**

Delete everything from line 408 through line 2409. This removes:
- `BUILDER_MAP` and `BUILDER_IDS` statics (lines 408–409)
- `log_level_to_filter_str` helper (lines 411–420)
- `with_builder`, `take_builder`, `add_net_cfg` C API helpers
- All `#[no_mangle] pub extern "C" fn krun_*` functions (24 declarations)
- `krun_start_enter_nitro` (feature-gated C API helper for AWS Nitro)

The section to delete starts with:
```rust
static BUILDER_MAP: Lazy<Mutex<HashMap<u32, Builder>>>
```
and ends with the closing brace of `krun_start_enter_nitro` (last `}` before `/// Information about a console device`).

After deletion, `src/libkrun/src/lib.rs` should begin with the existing imports (lines 1–407) and then immediately have:
```rust
/// Information about a console device for computing port paths.
#[derive(Debug, Clone)]
pub struct ConsoleDeviceInfo {
```

**Step 2: Remove now-unused C-API-only imports**

In `src/libkrun/src/lib.rs`, remove these lines:
```rust
use libc::{c_char, c_int, size_t};
use once_cell::sync::Lazy;
```

In the `use std::collections::hash_map::Entry;` import — remove `Entry` (keep `HashMap` as it is used by `Builder::port_map`):
```rust
// BEFORE:
use std::collections::hash_map::Entry;
use std::collections::HashMap;

// AFTER:
use std::collections::HashMap;
```

Remove these lines entirely:
```rust
use std::ffi::{c_void, CStr, CString};
use std::slice;
use std::sync::atomic::{AtomicI32, Ordering};
const KRUN_SUCCESS: i32 = 0;
const MAX_ARGS: usize = 4096;
```

**Step 3: Change crate type in `src/libkrun/Cargo.toml`**

Change:
```toml
crate-type = ["cdylib", "lib"]
```
To:
```toml
crate-type = ["lib"]
```

**Verification:**

Run: `cargo check -p krun --features embedded_init,snapshot,uffd,blk,vhost-user`

Expected: Compiles with no errors. Fix any remaining unused import warnings — the compiler will identify them precisely. Common remaining issue: `use env_logger::{Env, Target}` where `Target` may become unused (remove it, keep `Env` if Builder uses it, or check if both can be removed).

**Commit:** `feat: remove C API from libkrun, change crate-type to lib`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Update `tests/test_cases/Cargo.toml` to remove krun-sys

**Verifies:** testing-upgrade.AC1.4 (partial — integration tests must compile)

**Files:**
- Modify: `tests/test_cases/Cargo.toml:6,13`

**Implementation:**

**Step 1: Remove krun-sys from the host feature and dependencies**

Change:
```toml
[features]
host = ["krun-sys", "dep:libkrun", "dep:futures"]
```
To:
```toml
[features]
host = ["dep:libkrun", "dep:futures"]
```

Remove this line from `[dependencies]`:
```toml
krun-sys = { path = "../../krun-sys", optional = true, features = ["bindgen_clang_runtime"] }
```

**Verification:**

Run from `tests/` directory: `cargo check --features host`

Expected: Compile error or warning about unused krun.rs module (the `krun_call!` / `krun_call_u32!` macros are still referenced by the 5 test files that haven't been migrated yet — those errors will be fixed in Task 3).

**Do not commit yet** — Tasks 3-7 must complete before the tests compile.
<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-7) -->

<!-- START_TASK_3 -->
### Task 3: Rewrite `common.rs` and delete `krun.rs` references from 5 test files

**Verifies:** testing-upgrade.AC1.4 (integration tests compile with Rust API)

**Files:**
- Modify: `tests/test_cases/src/common.rs`
- Modify: `tests/test_cases/src/test_vm_config.rs`
- Modify: `tests/test_cases/src/test_tsi_tcp_guest_connect.rs`
- Modify: `tests/test_cases/src/test_tsi_tcp_guest_listen.rs`

**Implementation:**

**Step 1: Rewrite `tests/test_cases/src/common.rs`**

`common.rs` currently provides `setup_fs_and_enter()` which calls the C API and never returns. Replace the entire file content with a Rust API equivalent:

```rust
//! Host-side helpers used by multiple tests.
//!
//! Deprecated in favor of `krun_rust::setup_fs_builder`. Kept for tests
//! that haven't been migrated, will be deleted once all tests use setup_fs_builder.
//
// This module is intentionally empty — all C API helpers have been removed.
// Tests should use krun_rust::setup_fs_builder instead.
```

(This removes `setup_fs_and_enter`. Tests that call it will fail to compile until migrated.)

**Step 2: Migrate `tests/test_cases/src/test_vm_config.rs`**

Replace the `#[host] mod host` block entirely:

```rust
#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};

    impl Test for TestVmConfig {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();
            builder.vm_config(self.num_cpus, self.ram_mib)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;
            context.run()?;
            Ok(())
        }
    }
}
```

**Step 3: Migrate `tests/test_cases/src/test_tsi_tcp_guest_connect.rs`**

Replace the `#[host] mod host` block. The old code calls `krun_set_vm_config(ctx, 1, 512)`. Rust equivalent:

```rust
#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::thread;

    impl Test for TestTsiTcpGuestConnect {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let listener = self.tcp_tester.create_server_socket();
            thread::spawn(move || self.tcp_tester.run_server(listener));

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;
            context.run()?;
            Ok(())
        }
    }
}
```

(Remove the `unsafe` block, `CString`, `null` imports — no longer needed.)

**Step 4: Migrate `tests/test_cases/src/test_tsi_tcp_guest_listen.rs`**

The old code calls `krun_set_port_map(ctx, port_map.as_ptr())` with a null-terminated C array of `"PORT:PORT"` strings. Rust equivalent uses `builder.port_map(HashMap<u16, u16>)`.

```rust
#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::collections::HashMap;
    use std::thread;

    impl Test for TestTsiTcpGuestListen {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(1));
                self.tcp_tester.run_client();
            });

            let mut builder = krun::Builder::new();
            let mut port_mapping = HashMap::new();
            port_mapping.insert(PORT, PORT);
            builder.port_map(port_mapping).map_err(|_| anyhow::anyhow!("port_map failed"))?;
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;
            context.run()?;
            Ok(())
        }
    }
}
```

**Verification:**

Run from `tests/` directory: `cargo check --features host`

Expected: Compile errors only for the two remaining un-migrated files (`test_vsock_guest_connect.rs` and `test_multiport_console.rs`) — fix those in Tasks 4 and 5.

**Do not commit yet.**
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Migrate `test_vsock_guest_connect.rs` to Rust API

**Verifies:** testing-upgrade.AC1.4

**Files:**
- Modify: `tests/test_cases/src/test_vsock_guest_connect.rs`

**Implementation:**

The old host block calls `krun_add_vsock_port(ctx, VSOCK_PORT, sock_path_cstr.as_ptr())`. The Rust equivalent is `builder.add_vsock_port(port, filepath, listen)`.

Replace the `#[host] mod host` block:

```rust
#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::{mem, thread};

    fn server(listener: UnixListener) {
        let (mut stream, _addr) = listener.accept().unwrap();
        stream_set_timeouts(&mut stream);
        stream.write_all(b"ping!").unwrap();
        stream_expect_msg(&mut stream, b"pong!");
        stream_expect_wouldblock(&mut stream);
        stream.write_all(b"bye!").unwrap();
        // Leak the socket fd to not close it early when we exit the thread
        mem::forget(stream);
    }

    impl Test for TestVsockGuestConnect {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("test.sock");
            let listener = UnixListener::bind(&sock_path).unwrap();
            thread::spawn(move || server(listener));

            let mut builder = krun::Builder::new();
            builder.add_vsock_port(VSOCK_PORT, sock_path, false);
            builder.vm_config(1, 1024)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;
            context.run()?;
            Ok(())
        }
    }
}
```

**Verification:**

Run: `cargo check --features host` from `tests/` directory.

Expected: Only `test_multiport_console.rs` still has compile errors.

**Do not commit yet.**
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Migrate `test_multiport_console.rs` to Rust API

**Verifies:** testing-upgrade.AC1.4

**Files:**
- Modify: `tests/test_cases/src/test_multiport_console.rs`

**Implementation:**

This is the most complex migration. The old code:
1. `krun_disable_implicit_console(ctx)` → `builder.disable_implicit_console()?`
2. `krun_add_virtio_console_default(ctx, -1, stdout_fd, -1)` → adds a "default" console routing output to stdout. Rust equivalent: `builder.add_virtio_console()` + `builder.add_port_console_fd(&info, -1, stdout.as_raw_fd(), 80, 24)`
3. `krun_add_virtio_console_multiport(ctx)` → returns console_id. Rust equivalent: `builder.add_virtio_console()` → returns `ConsoleDeviceInfo`
4. `krun_add_console_port_inout(ctx, console_id, name, in_fd, out_fd)` → `builder.add_port_fd(&console_info, name, in_fd, out_fd)`

Replace the `fn test_port` and `impl Test for TestMultiportConsole` host block:

```rust
#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::{mem, thread};
    use krun::ConsoleDeviceInfo;

    fn spawn_ping_pong_responder(stream: UnixStream) {
        thread::spawn(move || {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok() && !line.is_empty() {
                let response = line.replace("PING", "PONG");
                writer.write_all(response.as_bytes()).unwrap();
                writer.flush().unwrap();
                line.clear();
            }
        });
    }

    fn test_port(
        builder: &mut krun::Builder,
        console_info: &ConsoleDeviceInfo,
        name: &str,
    ) -> anyhow::Result<()> {
        let (guest, host) = UnixStream::pair()?;
        builder.add_port_fd(console_info, name, guest.as_raw_fd(), guest.as_raw_fd());
        mem::forget(guest);
        spawn_ping_pong_responder(host);
        Ok(())
    }

    impl Test for TestMultiportConsole {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();

            builder.disable_implicit_console()?;

            // Add a default console routing output to stdout (replaces krun_add_virtio_console_default)
            let default_console_info = builder.add_virtio_console();
            builder.add_port_console_fd(
                &default_console_info,
                -1,
                std::io::stdout().as_raw_fd(),
                80,
                24,
            );

            // Add the multiport console (replaces krun_add_virtio_console_multiport)
            let multiport_console_info = builder.add_virtio_console();

            test_port(&mut builder, &multiport_console_info, "test-port-alpha")?;
            test_port(&mut builder, &multiport_console_info, "test-port-beta")?;
            test_port(&mut builder, &multiport_console_info, "test-port-gamma")?;

            builder.vm_config(1, 1024)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;
            context.run()?;
            Ok(())
        }
    }
}
```

**Note for implementor:** The C API `krun_add_virtio_console_default` used an internal `Autoconfigure` mode; the Rust API uses `Custom` mode via `add_virtio_console()`. These differ internally — verify the test passes during integration testing. If it fails, check how the `Autoconfigure` vs `Custom` modes differ in `src/vmm/src/builder.rs` for the console device setup path.

**Verification:**

Run from `tests/` directory: `cargo check --features host`

Expected: Clean compile — no more krun_sys references.

**Commit:** `refactor(tests): migrate C API integration tests to Rust Builder API`
<!-- END_TASK_5 -->

<!-- START_TASK_6 -->
### Task 6: Delete `tests/test_cases/src/krun.rs` and clean up `lib.rs`

**Verifies:** testing-upgrade.AC1.4

**Files:**
- Delete: `tests/test_cases/src/krun.rs`
- Modify: `tests/test_cases/src/lib.rs:214-215`

**Implementation:**

**Step 1:** Delete `tests/test_cases/src/krun.rs` — it only contained `krun_call!` and `krun_call_u32!` macros for the C API. No test files use them anymore.

**Step 2:** Remove the `krun` module declaration from `tests/test_cases/src/lib.rs`:

Remove these two lines:
```rust
#[cfg(feature = "host")]
mod krun;
```

**Step 3:** Also delete `tests/test_cases/src/common.rs` (now empty) and remove its declaration from `lib.rs`:

Remove from `lib.rs`:
```rust
#[cfg(feature = "host")]
mod common;
```

And delete `tests/test_cases/src/common.rs`.

**Verification:**

Run from `tests/` directory: `cargo check --features host && cargo check --features guest`

Expected: Clean compile for both features.

**Commit:** `chore(tests): delete krun.rs and common.rs C API helpers`
<!-- END_TASK_6 -->

<!-- START_TASK_7 -->
### Task 7: Compile-check and run unit tests

**Verifies:** testing-upgrade.AC1.2, testing-upgrade.AC1.4 (partial)

**Files:** No changes

**Verification:**

Run: `cargo check --features embedded_init,snapshot,uffd,blk,vhost-user`

Expected: Main workspace compiles cleanly.

Run: `cargo test -p devices --features net,snapshot`

Expected: All devices unit tests pass.

Run: `cargo test -p vmm --features snapshot`

Expected: All vmm unit tests pass.

Run from `tests/`: `cargo test --features guest`

Expected: The `all_testcases_have_unique_names` test passes.

**Commit:** none (verification only)
<!-- END_TASK_7 -->

<!-- END_SUBCOMPONENT_B -->

<!-- START_SUBCOMPONENT_C (tasks 8-10) -->

<!-- START_TASK_8 -->
### Task 8: Create `justfile` at project root

**Verifies:** testing-upgrade.AC5.1 (partial), testing-upgrade.AC5.2, testing-upgrade.AC5.3, testing-upgrade.AC5.4

**Files:**
- Create: `justfile` at project root

**Implementation:**

Create `/home/titanous/vm-platform/libkrun/.worktrees/testing-upgrade/justfile` with this exact content:

```justfile
# Feature set used by all targets.
# AC2.11: All targets use this same variable.
features := "embedded_init,snapshot,uffd,blk,vhost-user"

# Default: check
default: check

# Format check + clippy
check:
    cargo fmt --check
    cargo clippy --features {{features}}

# Build the release library
build:
    cargo build --release --features {{features}}

# Unit tests for all crates
test:
    cargo test -p devices --features net,snapshot
    cargo test -p vmm --features snapshot
    cargo test -p devices --features net,vhost-user

# Run integration tests.
# Requires libkrunfw.so in test-prefix/lib64/ (nix shellHook creates this symlink).
# Usage:
#   just integration         — run all tests
#   just integration <name>  — run single test by name
integration test="all":
    mkdir -p test-prefix/lib64
    cd tests && RUST_LOG=trace LD_LIBRARY_PATH="$(realpath ../test-prefix/lib64/)" ./run.sh test --test-case "{{test}}"

# Compound target: all fast tests (extended in later phases)
# Phase 1: check + unit tests
# Later phases add: miri proptest loom shuttle
all: check test

# Compound target: safety checks (extended in later phases)
# Phase 1: check only
# Later phases add: asan miri fuzz-all kani
safety: check

# ── Stubs for tools added in later phases ────────────────────────────────────
# These targets are extended by later implementation phases.
# Running them before the corresponding phase is complete will exit with an error.

miri:
    @echo "miri: set up in Phase 3 (Miri + proptest + Loom)"
    @exit 1

proptest:
    @echo "proptest: set up in Phase 3 (Miri + proptest + Loom)"
    @exit 1

proptest-long:
    @echo "proptest-long: set up in Phase 3 (Miri + proptest + Loom)"
    @exit 1

loom:
    @echo "loom: set up in Phase 3 (Miri + proptest + Loom)"
    @exit 1

fuzz target:
    @echo "fuzz: set up in Phase 4 (Fuzzing)"
    @exit 1

fuzz-all duration="60":
    @echo "fuzz-all: set up in Phase 4 (Fuzzing)"
    @exit 1

fuzz-list:
    @echo "fuzz-list: set up in Phase 4 (Fuzzing)"
    @exit 1

fuzz-corpus target:
    @echo "fuzz-corpus: set up in Phase 4 (Fuzzing)"
    @exit 1

asan:
    @echo "asan: set up in Phase 5 (ASan + Shuttle)"
    @exit 1

integration-asan:
    @echo "integration-asan: set up in Phase 5 (ASan + Shuttle)"
    @exit 1

shuttle iterations="1000":
    @echo "shuttle: set up in Phase 5 (ASan + Shuttle)"
    @exit 1

kani:
    @echo "kani: set up in Phase 6 (Kani Proofs)"
    @exit 1

kani-proof name:
    @echo "kani-proof: set up in Phase 6 (Kani Proofs)"
    @exit 1

mutants:
    @echo "mutants: set up in Phase 8 (Mutation Testing)"
    @exit 1

mutants-diff:
    @echo "mutants-diff: set up in Phase 8 (Mutation Testing)"
    @exit 1
```

**Verification:**

Run: `just check`

Expected: `cargo fmt --check` and `cargo clippy` both pass.

Run: `just build`

Expected: `cargo build --release` completes successfully.

Run: `just --list`

Expected: All targets listed.

**Commit:** `chore: add justfile with all targets (Phase 1 fully implemented, later phases stubbed)`
<!-- END_TASK_8 -->

<!-- START_TASK_9 -->
### Task 9: Delete `Makefile`

**Verifies:** testing-upgrade.AC5.1

**Files:**
- Delete: `Makefile` at project root

**Implementation:**

Delete the file at `Makefile`. The justfile replaces all functionality previously in the Makefile. The `test-prefix` setup is no longer needed for a C library install (we only need `libkrunfw.so` which is provided by the nix shellHook).

**Verification:**

Run: `ls Makefile 2>&1`

Expected: `ls: cannot access 'Makefile': No such file or directory`

Run: `just integration configure-vm-1cpu-256MiB`

Expected: A single integration test runs successfully (this also verifies `just integration <name>` works per AC5.4).

**Commit:** `chore: delete Makefile (replaced by justfile)`
<!-- END_TASK_9 -->

<!-- START_TASK_10 -->
### Task 10: Update CLAUDE.md files

**Verifies:** (documentation only)

**Files:**
- Modify: `CLAUDE.md` (root)
- Modify: `src/libkrun/CLAUDE.md`

**Implementation:**

**Step 1: Update root `CLAUDE.md`**

In the `## Tech Stack` section, change:
```
- Build: Makefile + Cargo workspace
```
To:
```
- Build: justfile + Cargo workspace
```

In the `## Commands` section, replace the `make` commands:
```markdown
## Commands
- `just check` - Format check + clippy (replaces `make`)
- `just build` - Build release library (replaces `make`)
- `just test` - Run unit tests for all crates
- `just integration` - Run all integration tests (embedded_init required; libkrunfw must be in test-prefix/lib64/)
- `just integration <name>` - Run a single named integration test
- `cargo test -p devices --features net` - Run devices crate unit tests (net feature needed for async_worker tests)
- `cargo test -p devices --features net,snapshot` - Devices tests including snapshot-dependent tests
- `cargo test -p vmm --features snapshot` - VMM crate unit tests (snapshot feature for snapshot.rs tests)
```

In `## Project Structure`, update the libkrun line:
```
- `src/libkrun/` - Public Rust API (`Builder`, `Context`, `VmHandle`) — C API removed
```

Update `Last verified:` date to `2026-03-03`.

**Step 2: Update `src/libkrun/CLAUDE.md`**

In the `## Purpose` section, change:
```
Public API crate providing both C FFI (`krun_*` functions) and Rust `Builder` API
```
To:
```
Public Rust API crate providing `Builder`, `Context`, and `VmHandle` for configuring and starting microVMs. C API removed.
```

In `## Contracts`, remove the `C API` mention from `**Exposes**` (the `krun_set_vm_config`, `krun_start_enter`, etc. bullets).

Update `Last verified:` to `2026-03-03`.

**Verification:**

Run: `grep -r "pub extern" src/libkrun/src/lib.rs`

Expected: No matches.

Run: `grep "Makefile" CLAUDE.md`

Expected: No matches (unless it's in the Debugging section or other historical context).

**Commit:** `docs: update CLAUDE.md files for C API removal and justfile`
<!-- END_TASK_10 -->

<!-- END_SUBCOMPONENT_C -->

<!-- START_TASK_11 -->
### Task 11: Run `just check` and `just test`

**Verifies:** testing-upgrade.AC1.3, testing-upgrade.AC5.3

**Implementation:**

Run: `just check`

Expected: `cargo fmt --check` passes (no formatting issues), `cargo clippy` passes (no warnings or errors).

Run: `just test`

Expected: All three `cargo test` invocations pass.

If `just check` fails due to clippy warnings introduced by the refactoring, fix them. Common issues after C API removal:
- Unused import warnings in lib.rs → remove the import
- `#[cfg(feature = "aws-nitro")]` blocks that reference removed types → check and clean up

**Commit:** `fix: address clippy warnings after C API removal` (only if changes needed)
<!-- END_TASK_11 -->

<!-- START_TASK_12 -->
### Task 12: Run `just integration` to verify integration tests pass

**Verifies:** testing-upgrade.AC1.4, testing-upgrade.AC5.4

**Implementation:**

Run: `just integration`

Expected: All integration tests pass (or at least as many as before — the suite has some inherent flakiness; 5-6/6 passing is normal per CLAUDE.md).

If specific tests fail with unexpected errors (not normal flakiness):
1. `configure-vm-*` / `vsock-*` / `tsi-*` / `multiport-console` — these are the migrated tests; investigate migration issues
2. `multiport-console` specifically — if the `Autoconfigure` vs `Custom` console mode difference causes failures, check `src/vmm/src/builder.rs` to understand how each mode is handled in `build_microvm`

Run: `just integration configure-vm-1cpu-256MiB`

Expected: Single test passes (verifies AC5.4 — `just integration <name>` works).

**Commit:** `feat: Phase 1 complete — C API removed, justfile added, tests passing`
<!-- END_TASK_12 -->
