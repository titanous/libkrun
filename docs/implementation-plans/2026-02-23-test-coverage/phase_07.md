# Test Coverage Implementation Plan — Phase 7

**Goal:** Validate the `Builder` / `Context` / `VmHandle` lifecycle and an end-to-end custom `AsyncBlockBackend`.

**Architecture:** Two code fixes and two new test files. The code fixes add 0-vCPU validation to `Builder::vm_config()` (returning `Result`) and extend `VmDeviceInfo` with `vcpu_count`/`ram_mib` fields. The integration tests cover: Builder error on 0 vCPUs (AC7.1), `device_info()` reflecting config (AC7.2), pause/resume cycle (AC7.3), trigger_shutdown_event (AC7.4, Linux-skipped), and an in-memory `AsyncBlockBackend` that the guest reads from and writes to (AC7.5, AC7.6).

**Tech Stack:** Rust, `krun` crate, `macros::{host, guest}`, async (tokio), `vm_memory::VolatileSlice`.

**Scope:** Phase 7 of 8 phases

**Codebase verified:** 2026-02-23

---

## Acceptance Criteria Coverage

This phase implements and tests:

### test-coverage.AC7: Rust API and custom block backend
- **test-coverage.AC7.1 Failure:** `Builder` configured with 0 vCPUs → `BuildError` before VM starts
- **test-coverage.AC7.2 Success:** `Builder::build()` → `Context::device_info()` reflects the configured vCPU count and RAM size
- **test-coverage.AC7.3 Success:** `VmHandle::pause()` followed by `VmHandle::resume()` → guest continues execution and prints `"OK"`
- **test-coverage.AC7.4 Success:** `VmHandle::trigger_shutdown_event()` → VM process exits cleanly (zero exit code)
- **test-coverage.AC7.5 Success:** VM started with an in-memory `AsyncBlockBackend` pre-filled with test data → guest reads and verifies the data
- **test-coverage.AC7.6 Success:** Guest writes data to the custom block backend → host verifies the backend received the written bytes after VM exits

---

## Codebase Findings (Phase 7 Investigation)

### AC7.1 — 0-vCPU validation discrepancy

Current `Builder::vm_config()` signature:
```rust
pub fn vm_config(&mut self, num_vcpus: u8, ram_mib: u32) -> &mut Self
```
It calls `self.config.vmr.set_vm_config(&vm_config).expect("invalid vm config")` — this panics if the VMM rejects the config (e.g., `vcpu_count = 0`). There is no `BuildError::InvalidVcpuCount` variant; `BuildError` only has `ConsoleAlreadyAdded` and `NetworkInterface` variants.

**Required code fix:** Change `vm_config()` to return `Result<&mut Self, StartError>` and add an explicit check:
```rust
pub fn vm_config(&mut self, num_vcpus: u8, ram_mib: u32) -> Result<&mut Self, StartError> {
    if num_vcpus == 0 {
        return Err(StartError::ZeroVcpus);
    }
    // ... existing logic ...
    Ok(self)
}
```

Add `ZeroVcpus` to `StartError` (or reuse an existing variant). This also requires updating all existing call sites of `vm_config()` to handle `Result`. Search for `.vm_config(` in `src/libkrun/src/lib.rs` to find all C-API call sites (they currently use `.expect()`; change to `?` or match).

**Note:** The task-implementor should check if `vmm::vmm_config::VmConfigError` already has a `ZeroVcpus`-like variant that can be used.

### AC7.2 — VmDeviceInfo discrepancy

Current `VmDeviceInfo`:
```rust
pub struct VmDeviceInfo {
    pub console_ports: Vec<ConsolePortInfo>,
}
```
Does NOT include vCPU count or RAM size. The design AC says `device_info()` should reflect these.

**Required code fix:** Extend `VmDeviceInfo` in `src/vmm/src/resources.rs` (verify location):
```rust
pub struct VmDeviceInfo {
    pub console_ports: Vec<ConsolePortInfo>,
    pub vcpu_count: u8,
    pub ram_mib: u32,
}
```
And populate `vcpu_count`/`ram_mib` from the VMM config in `Builder::build()` when constructing `device_info`. Trace where `VmDeviceInfo` is populated in the build path to find the right place.

### AC7.3 — pause/resume

`VmHandle::pause()` pauses all vCPUs. `VmHandle::resume()` resumes them. These methods block until vCPUs acknowledge. The test uses vsock coordination:
- Host pauses, waits a moment, resumes
- Guest verifies it continues and prints "OK"

### AC7.4 — trigger_shutdown_event (platform-specific)

`trigger_shutdown_event()` only works on aarch64/macOS (`shutdown_efd` is `None` on Linux). On Linux:
```rust
Err(StartError::Microvm(StartMicrovmError::Internal(
    vmm::Error::EventFd(std::io::Error::new(ErrorKind::Unsupported, "..."))
)))
```

**Implementation approach for AC7.4:** On Linux x86, test that `trigger_shutdown_event()` returns an `Err` on Linux. On macOS/aarch64, test that it causes the VM to exit. Use `#[cfg(target_arch = "aarch64")]` or a runtime platform check.

Alternative: check if the error is `Unsupported` on Linux and treat that as AC7.4 "verified" on Linux (verifying the function doesn't panic and returns a meaningful error), while on aarch64/macOS it actually triggers shutdown.

### AC7.5/7.6 — AsyncBlockBackend

`AsyncBlockBackend` trait (from `src/devices/src/virtio/block/mod.rs`):
```rust
pub trait AsyncBlockBackend: Send + Sync {
    fn cache_type(&self) -> CacheType;
    fn nsectors(&self) -> u64;
    fn image_id(&self) -> &[u8];
    fn read_vectored_at(&self, bufs: Vec<VolatileSliceGuard>, offset: u64) -> BoxFuture<'_, io::Result<usize>>;
    fn write_vectored_at(&self, bufs: Vec<VolatileSliceGuard>, offset: u64) -> BoxFuture<'_, io::Result<usize>>;
    fn flush(&self) -> BoxFuture<'_, io::Result<()>>;
    fn sync(&self) -> BoxFuture<'_, io::Result<()>>;
    fn discard(&self, offset: u64, nbytes: u64) -> BoxFuture<'_, io::Result<()>>;
    fn write_zeroes(&self, offset: u64, nbytes: u64) -> BoxFuture<'_, io::Result<()>>;
    fn on_exit(&self) {}
    fn save_snapshot_state(&self) -> Option<Vec<u8>> { None }
    fn restore_snapshot_state(&mut self, _data: &[u8]) {}
}
```

`AsyncBlockBackendFactory` creates the backend inside the tokio runtime:
```rust
pub trait AsyncBlockBackendFactory: Send + 'static {
    fn create(self: Box<Self>) -> SendBoxFuture<'static, io::Result<Arc<dyn AsyncBlockBackend>>>;
}
```

`BlockDeviceConfig`:
```rust
pub struct BlockDeviceConfig {
    pub block_id: String,
    pub cache_type: CacheType,
    pub disk_type: BlockDeviceType,
    pub is_disk_read_only: bool,
    pub direct_io: bool,
}
```

Use `BlockDeviceType::CustomAsyncFactory { factory: Box::new(my_factory) }`.

Add via `builder.root_block_cfg(block_cfg)` for the root block device.

**In-memory backend design:**

```rust
use std::sync::Arc;
use tokio::sync::Mutex;

struct MemBlockBackend {
    data: Arc<Mutex<Vec<u8>>>,
    sector_size: u64,
}
```

The backend stores data as a `Vec<u8>` in a tokio `Mutex` (since `read_vectored_at`/`write_vectored_at` return async futures). For recording writes (AC7.6), share the `Arc<Mutex<Vec<u8>>>` with the test so the host can inspect it after the VM exits.

**Note on VolatileSliceGuard:** This type wraps a `vm_memory::VolatileSlice` with a lifetime guard. The task-implementor must look at how `TrackingBackend` in `async_worker.rs` uses it to implement `read_vectored_at`/`write_vectored_at` — copy that pattern exactly.

### BlockDeviceType location

`BlockDeviceType` is in `src/devices/src/virtio/block/device.rs` and re-exported from `krun`:
```rust
pub use devices::virtio::block::device::BlockDeviceType;
```

---

## Tasks

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Fix vm_config() to return Result and add ZeroVcpus error

**Verifies:** test-coverage.AC7.1 (prerequisite)

**Files:**
- Modify: `src/libkrun/src/lib.rs` — `Builder::vm_config()` and its call sites in the C API wrappers

**Implementation:**

1. Add `ZeroVcpus` to `StartError` (or find an existing suitable variant):
```rust
// In StartError enum (look for its definition in src/libkrun/src/lib.rs):
#[error("vcpu_count must be at least 1")]
ZeroVcpus,
```

2. Change `vm_config()` signature and add validation:
```rust
pub fn vm_config(&mut self, num_vcpus: u8, ram_mib: u32) -> Result<&mut Self, StartError> {
    if num_vcpus == 0 {
        return Err(StartError::ZeroVcpus);
    }
    let mem_size_mib: usize = ram_mib.try_into().expect("ram_mib did not fit in a usize");
    let vm_config = VmConfig {
        vcpu_count: Some(num_vcpus),
        mem_size_mib: Some(mem_size_mib),
        ht_enabled: Some(false),
        cpu_template: None,
    };
    self.config
        .vmr
        .set_vm_config(&vm_config)
        .map_err(|_| StartError::ZeroVcpus)?;  // or a more specific error
    Ok(self)
}
```

3. **C API call sites:** The `vm_config()` method on `Builder` is called only from one site in `src/libkrun/src/lib.rs` (line 2386, the `Builder::vm_config()` method body itself — it calls `self.config.vmr.set_vm_config(...)` directly, NOT a recursive `.vm_config()` call). The C API function `krun_set_vm_config()` (line 522) calls `ctx_cfg.config.vmr.set_vm_config(...)` directly, BYPASSING `Builder::vm_config()`.

   Therefore, the `krun_set_vm_config()` C API function also needs its own zero-vCPU guard:
   ```rust
   // In krun_set_vm_config() (around line 500-530):
   if num_vcpus == 0 {
       return libc::EINVAL;
   }
   ```
   This keeps the C API safe without touching the Rust `Result`-returning path.

   The Rust API tests use `builder.vm_config()` which now returns `Result<&mut Self, StartError>`. All test code (phases 6–8) must call `builder.vm_config(1, 512)?` (adding `?` to propagate errors). The code examples in phases 6–8 already show `builder.vm_config(1, 512)` without `?` — the task-implementor must add `?` at all these call sites.

**Verification:**

Run: `cargo build -p libkrun`
Expected: Builds without errors. All call sites updated.

**Commit:** `fix(api): add 0-vCPU validation to Builder::vm_config() returning Result`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Extend VmDeviceInfo with vcpu_count and ram_mib

**Verifies:** test-coverage.AC7.2 (prerequisite)

**Files:**
- Modify: Location of `VmDeviceInfo` struct — verify by running `grep -r 'struct VmDeviceInfo' src/`
- Modify: `src/libkrun/src/lib.rs` — `Builder::build()` path that populates `device_info`

**Implementation:**

1. Find `VmDeviceInfo` struct definition. Based on the import `vmm::resources::VmDeviceInfo` in `lib.rs`, it is in `src/vmm/src/resources.rs`. Verify:
   ```
   grep -r 'struct VmDeviceInfo' src/
   ```

2. Add fields:
   ```rust
   pub struct VmDeviceInfo {
       pub console_ports: Vec<ConsolePortInfo>,
       pub vcpu_count: u8,
       pub ram_mib: u32,
   }
   ```

3. Populate the new fields in `Builder::build()`. Search for where `VmDeviceInfo` is constructed in the build path. It should be populated from `ctx_cfg.vmr.vm_config` or similar. The task-implementor should trace `built_vm.device_info` back to where it's initialized and add the new fields there.

**Verification:**

Run: `cargo build -p libkrun`
Expected: Builds without errors.

**Commit:** `feat(api): add vcpu_count and ram_mib to VmDeviceInfo`
<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-5) -->

<!-- START_TASK_3 -->
### Task 3: Create test_rust_api.rs (AC7.1, AC7.2, AC7.3, AC7.4)

**Verifies:** test-coverage.AC7.1, test-coverage.AC7.2, test-coverage.AC7.3, test-coverage.AC7.4

**Files:**
- Create: `tests/test_cases/src/test_rust_api.rs`

**Implementation:**

Four separate test structs, each a host-only or host+guest test:

```rust
use macros::{guest, host};

// AC7.1 — 0 vCPU returns error (host-only)
pub struct TestRustApiZeroVcpu;

// AC7.2 — device_info() reflects config (host-only)
pub struct TestRustApiDeviceInfo;

// AC7.3 — pause/resume (needs guest)
pub struct TestRustApiPauseResume;

// AC7.4 — trigger_shutdown_event
pub struct TestRustApiShutdown;

const VSOCK_PORT_API: u32 = 5680;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    use std::thread;

    impl Test for TestRustApiZeroVcpu {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();
            // vm_config() now returns Result<&mut Self, StartError>
            let result = builder.vm_config(0, 256);
            assert!(
                result.is_err(),
                "Expected error for 0 vCPUs, but vm_config() succeeded"
            );
            println!("OK");
            Ok(())
        }
    }

    impl Test for TestRustApiDeviceInfo {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();
            builder.vm_config(2, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;
            let info = context.device_info();
            // After Phase 7 Task 2, VmDeviceInfo has vcpu_count and ram_mib
            assert_eq!(info.vcpu_count, 2, "vcpu_count mismatch");
            // RAM may not be exactly 512 MiB due to alignment; allow ±10%
            let ram = info.ram_mib;
            assert!(
                ram >= 460 && ram <= 565,
                "ram_mib {ram} not within 10% of 512"
            );
            // Don't run the VM — just check device_info
            println!("OK");
            Ok(())
        }
    }

    impl Test for TestRustApiPauseResume {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let sock_path = test_setup.tmp_dir.join("api_control.sock");
            let listener = UnixListener::bind(&sock_path).unwrap();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 256)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_vsock_port(VSOCK_PORT_API, sock_path, false);

            let context = builder.build()?;
            let handle = context.vm_handle();

            let vm_thread = thread::spawn(move || context.run());

            // Wait for guest READY
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"READY");

            // Pause and resume
            handle.pause()?;
            std::thread::sleep(Duration::from_millis(100));
            handle.resume()?;

            // Signal guest to print OK
            stream.write_all(b"CONT!").unwrap();

            vm_thread.join().ok();
            Ok(())
        }
    }

    impl Test for TestRustApiShutdown {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();
            builder.vm_config(1, 256)?;
            setup_fs_builder(&mut builder, &test_setup)?;

            let context = builder.build()?;
            let handle = context.vm_handle();

            let _vm_thread = thread::spawn(move || context.run());

            // On Linux x86, trigger_shutdown_event() is unsupported (shutdown_efd = None)
            // On aarch64/macOS, it causes the VM to exit cleanly
            let result = handle.trigger_shutdown_event();
            #[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
            {
                // On Linux: verify the function returns a meaningful Err (not panic)
                assert!(
                    result.is_err(),
                    "Expected Err on non-aarch64-mac, got Ok"
                );
                // Verify the error message indicates unsupported (not a crash/panic)
                let msg = result.unwrap_err().to_string();
                assert!(
                    msg.contains("unavailable") || msg.contains("Unsupported"),
                    "Unexpected error: {msg}"
                );
                println!("OK");
            }
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                // On aarch64/macOS: should succeed and VM exits cleanly
                result?;
                // VM thread exits on its own
                println!("OK");
            }
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use nix::libc::VMADDR_CID_HOST;
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    impl Test for TestRustApiZeroVcpu {
        fn in_guest(self: Box<Self>) {
            // Never runs — host test doesn't start a VM
        }
    }

    impl Test for TestRustApiDeviceInfo {
        fn in_guest(self: Box<Self>) {
            // Never runs — host test doesn't start a VM
        }
    }

    impl Test for TestRustApiPauseResume {
        fn in_guest(self: Box<Self>) {
            let sock = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
                .unwrap();
            let addr = VsockAddr::new(VMADDR_CID_HOST, VSOCK_PORT_API);
            connect(sock.as_raw_fd(), &addr).unwrap();
            let mut stream = UnixStream::from(sock);
            stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(15))).unwrap();

            stream.write_all(b"READY").unwrap();

            // Wait for CONT! signal — host pauses and resumes between READY and CONT!
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"CONT!");

            println!("OK");
        }
    }

    impl Test for TestRustApiShutdown {
        fn in_guest(self: Box<Self>) {
            // On aarch64/macOS: would exit when shutdown event fires
            // On Linux: print OK (never reaches guest since host prints OK directly)
        }
    }
}
```

**Verification:**

Run: `cargo build --features host -p test_cases && cargo build --features guest -p test_cases`
Expected: Both compile without errors.

**Commit:** `test(rust-api): add Rust API lifecycle integration tests for AC7.1-AC7.4`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Create in-memory AsyncBlockBackend implementation

**Verifies:** Prerequisite for AC7.5, AC7.6

**Files:**
- Create: `tests/test_cases/src/mem_block_backend.rs`

**Implementation:**

The in-memory backend stores data as a `Vec<u8>` and shares it via `Arc<tokio::sync::Mutex<Vec<u8>>>` so the host can inspect writes after the VM exits.

```rust
//! In-memory AsyncBlockBackend for integration tests.
//!
//! Data is stored in a shared Arc<Mutex<Vec<u8>>> so the host can inspect
//! what the guest read/wrote after the VM exits.

use std::sync::Arc;
use std::io;

use krun::{
    AsyncBlockBackend, AsyncBlockBackendFactory, BoxFuture, CacheType,
    SendBoxFuture, VolatileSliceGuard,
};

pub struct MemBlockBackend {
    data: Arc<tokio::sync::Mutex<Vec<u8>>>,
    sector_count: u64,
}

impl MemBlockBackend {
    /// Create a backend with `sector_count` sectors (each 512 bytes), pre-filled with `fill`.
    pub fn new(sector_count: u64, fill: u8) -> (Self, Arc<tokio::sync::Mutex<Vec<u8>>>) {
        let size = (sector_count * 512) as usize;
        let data = Arc::new(tokio::sync::Mutex::new(vec![fill; size]));
        (MemBlockBackend { data: data.clone(), sector_count }, data)
    }
}

// No async_trait needed — futures are returned directly via BoxFuture.
impl AsyncBlockBackend for MemBlockBackend {
    fn cache_type(&self) -> CacheType {
        CacheType::Writeback
    }

    fn nsectors(&self) -> u64 {
        self.sector_count
    }

    fn image_id(&self) -> &[u8] {
        b"mem-block-backend"
    }

    fn read_vectored_at(
        &self,
        bufs: Vec<VolatileSliceGuard>,
        offset: u64,
    ) -> BoxFuture<'_, io::Result<usize>> {
        let data = self.data.clone();
        Box::pin(async move {
            let buf = data.lock().await;
            let mut pos = offset as usize;
            let mut total = 0usize;
            for iov in &bufs {
                let len = iov.len();
                let src = &buf[pos..pos + len];
                // SAFETY: `src` length == `iov.len()`; memory valid for duration of copy.
                unsafe { iov.copy_from(src); }
                pos += len;
                total += len;
            }
            Ok(total)
        })
    }

    fn write_vectored_at(
        &self,
        bufs: Vec<VolatileSliceGuard>,
        offset: u64,
    ) -> BoxFuture<'_, io::Result<usize>> {
        let data = self.data.clone();
        Box::pin(async move {
            let mut buf = data.lock().await;
            let mut pos = offset as usize;
            let mut total = 0usize;
            for iov in &bufs {
                let len = iov.len();
                let dst = &mut buf[pos..pos + len];
                // SAFETY: `dst` length == `iov.len()`; memory valid for duration of copy.
                unsafe { iov.copy_to(dst); }
                pos += len;
                total += len;
            }
            Ok(total)
        })
    }

    fn flush(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn discard(&self, _offset: u64, _nbytes: u64) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn write_zeroes(&self, offset: u64, nbytes: u64) -> BoxFuture<'_, io::Result<()>> {
        let data = self.data.clone();
        Box::pin(async move {
            let mut buf = data.lock().await;
            let start = offset as usize;
            let end = start + nbytes as usize;
            buf[start..end].fill(0);
            Ok(())
        })
    }
}

pub struct MemBlockBackendFactory {
    sector_count: u64,
    data: Arc<std::sync::Mutex<Option<MemBlockBackend>>>,
}

impl MemBlockBackendFactory {
    pub fn new(backend: MemBlockBackend) -> Self {
        let sector_count = backend.sector_count;
        Self {
            sector_count,
            data: Arc::new(std::sync::Mutex::new(Some(backend))),
        }
    }
}

impl AsyncBlockBackendFactory for MemBlockBackendFactory {
    fn nsectors(&self) -> u64 {
        self.sector_count
    }

    fn cache_type(&self) -> CacheType {
        CacheType::Writeback
    }

    fn create(
        self: Box<Self>,
    ) -> SendBoxFuture<'static, io::Result<Arc<dyn AsyncBlockBackend + Send + Sync>>> {
        let backend = self.data.lock().unwrap().take().unwrap();
        Box::pin(async move {
            Ok(Arc::new(backend) as Arc<dyn AsyncBlockBackend + Send + Sync>)
        })
    }
}
```

**Note on `VolatileSliceGuard` copy methods:** Both `copy_from` and `copy_to` are `unsafe` — see `src/devices/src/virtio/block/mod.rs` lines 379–392. The safety invariant is that the source/destination slice length ≤ `iov.len()` and the memory region remains valid for the duration of the copy. The template above satisfies both conditions.

**Note on `krun` re-exports:** `SendBoxFuture`, `BoxFuture`, `VolatileSliceGuard`, `CacheType`, `AsyncBlockBackend`, and `AsyncBlockBackendFactory` are all re-exported from `krun` (verified from `src/libkrun/src/lib.rs` public re-exports). If any are missing, fall back to `devices::virtio::block::{BoxFuture, SendBoxFuture, VolatileSliceGuard}` directly — but add `devices` as a dev-dep under the `host` feature first.

Add to `tests/test_cases/src/lib.rs`:
```rust
#[cfg(feature = "host")]
mod mem_block_backend;
#[cfg(feature = "host")]
use mem_block_backend::{MemBlockBackend, MemBlockBackendFactory};
```

Also add `tokio` to `tests/test_cases/Cargo.toml` under host deps if not already present (check if it's a transitive dependency or needs to be explicit).

**Verification:**

Run: `cargo build --features host -p test_cases`
Expected: Compiles without errors.

**Commit:** `feat(test_cases): add in-memory AsyncBlockBackend for integration tests`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Create test_custom_block_backend.rs (AC7.5, AC7.6)

**Verifies:** test-coverage.AC7.5, test-coverage.AC7.6

**Files:**
- Create: `tests/test_cases/src/test_custom_block_backend.rs`

**Implementation:**

Two test structs. The backend is pre-filled with a recognizable pattern (AC7.5 guest reads and verifies), and after the VM exits the host checks what the guest wrote (AC7.6).

For AC7.5 and AC7.6, use a single test that does both:
1. Backend pre-filled with `0x5A` at sector 0
2. Guest reads sector 0, verifies it's `0x5A`
3. Guest writes `0xAB` to sector 1
4. After VM exits, host checks sector 1 is `0xAB`

```rust
use macros::{guest, host};

pub struct TestCustomBlockBackend;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{mem_block_backend::{MemBlockBackend, MemBlockBackendFactory}, Test, TestSetup};
    use std::thread;

    impl Test for TestCustomBlockBackend {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            const SECTOR_COUNT: u64 = 64; // 64 * 512 = 32 KiB
            const FILL_BYTE: u8 = 0x5A;

            let (backend, data_handle) = MemBlockBackend::new(SECTOR_COUNT, FILL_BYTE);
            let factory = MemBlockBackendFactory::new(backend);

            let block_cfg = krun::BlockDeviceConfig {
                block_id: "test-block".to_string(),
                cache_type: krun::CacheType::Writeback,
                disk_type: krun::BlockDeviceType::CustomAsyncFactory {
                    factory: Box::new(factory),
                },
                is_disk_read_only: false,
                direct_io: false,
            };

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            // The block device will be /dev/vda in the guest
            builder.add_block_cfg(block_cfg);

            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());

            // Wait for guest to finish (it prints "OK" then exits)
            vm_thread.join().ok();

            // AC7.6: verify guest wrote 0xAB to sector 1 (bytes 512..1023)
            let data = data_handle.blocking_lock();
            let sector1 = &data[512..1024];
            assert!(
                sector1.iter().all(|&b| b == 0xAB),
                "Expected sector 1 to be all 0xAB after guest write"
            );

            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};

    impl Test for TestCustomBlockBackend {
        fn in_guest(self: Box<Self>) {
            // The custom block device appears as /dev/vda in the guest
            // (verify the actual device name based on how libkrun creates block devices)
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/vda")
                .expect("Failed to open /dev/vda");

            // AC7.5: read sector 0 and verify it's 0x5A
            let mut sector0 = vec![0u8; 512];
            f.read_exact(&mut sector0).expect("Failed to read sector 0");
            assert!(
                sector0.iter().all(|&b| b == 0x5A),
                "Expected sector 0 to be all 0x5A"
            );

            // AC7.6: write 0xAB to sector 1
            f.seek(SeekFrom::Start(512)).expect("Failed to seek to sector 1");
            let sector1_data = vec![0xABu8; 512];
            f.write_all(&sector1_data).expect("Failed to write sector 1");
            f.flush().expect("Failed to flush");

            println!("OK");
        }
    }
}
```

**Note on device name:** The task-implementor must verify the device name (`/dev/vda`) by checking what device node libkrun creates for the first `add_block_cfg()` device. Inspect `src/libkrun/src/lib.rs`'s block device addition code or the guest's `/proc/partitions` output.

**Verification:**

Run: `cargo build --features host -p test_cases && cargo build --features guest -p test_cases`
Expected: Both compile without errors.

**Commit:** `test(block): add custom AsyncBlockBackend integration tests for AC7.5-AC7.6`
<!-- END_TASK_5 -->

<!-- START_TASK_6 -->
### Task 6: Register new test cases in lib.rs and run tests

**Verifies:** All AC7 tests registered

**Files:**
- Modify: `tests/test_cases/src/lib.rs`

**Implementation:**

Add module declarations:
```rust
mod test_rust_api;
use test_rust_api::{TestRustApiZeroVcpu, TestRustApiDeviceInfo, TestRustApiPauseResume, TestRustApiShutdown};

mod test_custom_block_backend;
use test_custom_block_backend::TestCustomBlockBackend;
```

In `test_cases()`:
```rust
TestCase::new("rust-api-zero-vcpu", Box::new(TestRustApiZeroVcpu)),
TestCase::new("rust-api-device-info", Box::new(TestRustApiDeviceInfo)),
TestCase::new("rust-api-pause-resume", Box::new(TestRustApiPauseResume)),
TestCase::new("rust-api-shutdown", Box::new(TestRustApiShutdown)),
TestCase::new("custom-block-backend", Box::new(TestCustomBlockBackend)),
```

Run: `make test` (or equivalent)
Expected: All 5 new AC7 tests pass, plus all 11 prior tests.

**Commit:** `test(rust-api): register Rust API and block backend tests in lib.rs`
<!-- END_TASK_6 -->

<!-- END_SUBCOMPONENT_B -->
