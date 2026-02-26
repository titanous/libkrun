# Userfaultd Implementation Plan — Phase 3: UFFD Handler (Demand-Paging)

**Goal:** Implement the UFFD handler that demand-pages guest memory during cold restore, resolving faults via the `SnapshotStore` trait.

**Architecture:** `UffdHandler` runs on a dedicated thread with its own tokio runtime. It creates the UFFD fd, registers guest memory regions, and runs a fault loop using `AsyncFd<Uffd>`. Each fault spawns a `tokio::spawn` task that calls `store.read_page()` then `uffd.copy()`. EEXIST is silently handled. Fatal errors signal `VmExit::Error` via `SharedVmExit`. This phase implements fault-only demand paging (no preload — that's Phase 4).

**Tech Stack:** Rust, userfaultfd crate, tokio (AsyncFd, spawn), futures

**Scope:** 6 phases from original design (phase 3 of 6)

**Codebase verified:** 2026-02-25

---

## Acceptance Criteria Coverage

This phase implements and tests:

### userfaultd.AC3: UFFD handler
- **userfaultd.AC3.1 Success:** `UffdHandler` creates UFFD fd, registers guest memory regions, runs on dedicated thread with tokio runtime
- **userfaultd.AC3.2 Success:** Fault loop uses `AsyncFd<Uffd>` (non-blocking) and `tokio::spawn` per fault for parallel resolution
- **userfaultd.AC3.3 Success:** Each fault task calls `store.read_page(guest_addr)` then `uffd.copy()`; EEXIST return is silently ignored
- **userfaultd.AC3.6 Success:** Fatal `read_page` error signals VMM stop via `VmExit::Error`; preload stream errors are non-fatal (logged, preload stops, faults handle remaining pages)

---

## Reference Files

- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/CLAUDE.md` — VMM crate contracts, VmExit signaling
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/src/vm_exit.rs` — `VmExit` enum, `SharedVmExit` type (`Arc<Mutex<Option<VmExit>>>`)
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/devices/src/virtio/block/async_worker.rs:197-221` — Dedicated thread + tokio runtime pattern
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/devices/src/virtio/net/async_worker.rs:95-112` — Same pattern, single-threaded runtime
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/src/builder.rs:1730-1916` — Guest memory creation (anonymous mmap vs memfd)
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/.reference/firecracker/src/firecracker/examples/uffd/uffd_utils.rs` — Firecracker UFFD handler reference (EEXIST handling, registration)
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/.reference/linux/mm/userfaultfd.c` — Kernel UFFDIO_COPY race safety (EEXIST via PTE check under spinlock)

---

## External Dependency Reference

### userfaultfd crate (0.8+)
- `UffdBuilder::new()` → `.close_on_exec(true)` → `.non_blocking(true)` → `.user_mode_only(true)` → `.create()` → `Result<Uffd>`
- `uffd.register(start: *mut c_void, len: usize)` → `Result<IoctlFlags>` — registers memory range for missing-page faults
- `unsafe { uffd.copy(src, dst, len, wake) }` → `Result<usize>` — copies data into faulting page; returns `Err(CopyFailed)` with EEXIST errno when page already mapped
- `uffd.read_event()` → `Result<Option<Event>>` — non-blocking when created with `.non_blocking(true)`, returns `None` if no event ready
- `Event::Pagefault { kind, rw, addr, .. }` — faulting address in `addr`
- `Uffd: AsRawFd` — wrappable with `tokio::io::unix::AsyncFd`

---

<!-- START_TASK_1 -->
### Task 1: Add uffd feature flag and userfaultfd dependency

**Verifies:** None (infrastructure)

**Files:**
- Modify: `src/vmm/Cargo.toml` (add `uffd` feature and `userfaultfd`, `tokio`, `futures` dependencies)
- Modify: `src/libkrun/Cargo.toml` (add `uffd = ["vmm/uffd"]` feature)

**Implementation:**

Add to `src/vmm/Cargo.toml`:

In `[features]` section:
```toml
uffd = ["snapshot", "userfaultfd", "futures", "tokio"]
```

Note: `uffd` implies `snapshot` (needs SnapshotStore trait). Also needs `futures` (for BoxStream in SnapshotStore) and `tokio` (for runtime on UFFD thread). `tokio` is currently only a dependency of the `devices` crate — it needs to be added to the `vmm` crate too for the UFFD handler.

In `[target.'cfg(target_os = "linux")'.dependencies]` section:
```toml
userfaultfd = { version = "0.8", optional = true }
```

In `[dependencies]` section, add tokio (optional, for UFFD thread):
```toml
tokio = { version = "1", features = ["rt", "sync", "macros", "io-util", "net"], optional = true }
futures = { version = "0.3", optional = true }
```

The `uffd` feature is Linux-only. The `userfaultfd` crate is placed under `cfg(target_os = "linux")` dependencies.

Also update `src/libkrun/Cargo.toml` to propagate the `uffd` feature to the vmm crate. In the `[features]` section, add:
```toml
uffd = ["vmm/uffd"]
```

**Verification:**
Run: `cargo check -p vmm --features uffd` (on Linux)
Expected: Compiles without errors

**Commit:** `feat(vmm): add uffd feature flag and userfaultfd dependency`
<!-- END_TASK_1 -->

<!-- START_SUBCOMPONENT_A (tasks 2-4) -->

<!-- START_TASK_2 -->
### Task 2: Implement UffdHandler struct and UFFD creation

**Verifies:** userfaultd.AC3.1

**Files:**
- Create: `src/vmm/src/uffd.rs`
- Modify: `src/vmm/src/lib.rs` (add `pub mod uffd` gated on `#[cfg(all(target_os = "linux", feature = "uffd"))]`)

**Implementation:**

Create `src/vmm/src/uffd.rs` with the `UffdHandler` struct. Gate the entire module with `#[cfg(all(target_os = "linux", feature = "uffd"))]`.

`UffdHandler` encapsulates UFFD lifecycle:

```rust
pub struct UffdHandler {
    uffd: Arc<userfaultfd::Uffd>,
    store: Arc<dyn SnapshotStore>,
    vm_exit: SharedVmExit,
    // Memory region info for guest_addr → host_addr translation
    regions: Vec<UffdRegion>,
}

struct UffdRegion {
    guest_addr: u64,
    host_addr: u64,
    size: u64,
}
```

Provide a `UffdHandler::new()` that:
1. Creates `Uffd` via `UffdBuilder::new().close_on_exec(true).non_blocking(true).user_mode_only(true).create()`
2. Registers each guest memory region with `uffd.register(host_addr as *mut _, size)`
3. Stores region mapping for guest_addr ↔ host_addr translation (needed because `read_page` uses guest addresses but `uffd.copy` uses host addresses)

The handler needs a function to translate guest address to host address using the region list.

Also provide a `UffdHandler::run()` method that spawns the handler on a dedicated thread with tokio runtime (following the async_worker pattern from `src/devices/src/virtio/block/async_worker.rs:197-221`):
```rust
pub fn run(self) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("uffd-handler".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create uffd tokio runtime");
            rt.block_on(self.fault_loop());
        })
        .expect("failed to spawn uffd handler thread")
}
```

**Testing:**

Test that UffdHandler can be constructed with a mock SnapshotStore and registers memory regions. Since UFFD requires real anonymous mmap'd memory, the test must:
1. `mmap` an anonymous region
2. Create `UffdHandler` with the region
3. Verify the Uffd was created and region registered (no panic/error)

This is a unit test in `uffd.rs`. Note: requires running on Linux with UFFD support.

**Verification:**
Run: `cargo test -p vmm --features uffd`
Expected: Tests pass on Linux

**Commit:** `feat(vmm): implement UffdHandler struct and UFFD creation`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Implement fault loop with parallel resolution

**Verifies:** userfaultd.AC3.2, userfaultd.AC3.3

**Files:**
- Modify: `src/vmm/src/uffd.rs` (add fault_loop async method)

**Implementation:**

Implement the `fault_loop` async method on `UffdHandler`:

```rust
async fn fault_loop(self) {
    let async_uffd = tokio::io::unix::AsyncFd::new(self.uffd.clone())
        .expect("failed to create AsyncFd for uffd");

    loop {
        // Wait for UFFD to be readable
        let mut guard = match async_uffd.readable().await {
            Ok(guard) => guard,
            Err(_) => break, // Uffd fd closed (shutdown)
        };

        // Read event (non-blocking)
        match async_uffd.get_ref().read_event() {
            Ok(Some(userfaultfd::Event::Pagefault { addr, .. })) => {
                let guest_addr = self.host_to_guest(addr as u64);
                let store = self.store.clone();
                let uffd = self.uffd.clone();
                let host_addr = addr as u64;
                let vm_exit = self.vm_exit.clone();

                tokio::spawn(async move {
                    match store.read_page(guest_addr).await {
                        Ok(data) => {
                            let result = unsafe {
                                uffd.copy(
                                    data.as_ptr() as *const _,
                                    host_addr as *mut _,
                                    data.len(),
                                    true, // wake
                                )
                            };
                            match result {
                                Ok(_) => {},
                                Err(e) => {
                                    // Check for EEXIST (page already mapped)
                                    // Silently ignore — race with preload or another fault
                                    // For other errors, signal fatal
                                    if !is_eexist(&e) {
                                        signal_error(&vm_exit, format!("uffd copy failed: {e:?}"));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            // Fatal: read_page failed, signal VmExit::Error
                            signal_error(&vm_exit, format!("demand page read failed at 0x{guest_addr:x}: {e}"));
                        }
                    }
                });
            }
            Ok(Some(_)) => {
                // Other events (Fork, Remap, Remove, Unmap) — ignore
            }
            Ok(None) => {
                // No event ready (WouldBlock) — clear readiness and retry
                guard.clear_ready();
            }
            Err(_) => {
                // Uffd fd error (likely closed) — exit loop
                break;
            }
        }
    }
}
```

`AsyncFd::new()` requires the inner type to implement `AsRawFd`. Since `Arc<Uffd>` doesn't implement `AsRawFd`, create a newtype wrapper:

```rust
struct UffdFd(Arc<userfaultfd::Uffd>);

impl std::os::unix::io::AsRawFd for UffdFd {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.0.as_raw_fd()
    }
}
```

Use `AsyncFd::new(UffdFd(self.uffd.clone()))` for the fault loop. Spawned tasks clone the `Arc<Uffd>` directly for `uffd.copy()` calls.

Helper functions:
- `host_to_guest(host_addr: u64) -> u64` — translates using `regions` list: find region containing `host_addr`, compute `guest_addr = region.guest_addr + (host_addr - region.host_addr)`
- `is_eexist(e: &userfaultfd::Error) -> bool` — check if the error represents EEXIST. The `userfaultfd` crate's `copy()` returns `Err(userfaultfd::Error)` wrapping an `io::Error` for ioctl failures. Extract the underlying `io::Error` and check `raw_os_error() == Some(libc::EEXIST)`. Reference: Firecracker's `uffd_utils.rs` uses the same pattern. Concretely:
  ```rust
  fn is_eexist(e: &userfaultfd::Error) -> bool {
      matches!(e, userfaultfd::Error::SystemError(errno) if *errno == libc::EEXIST)
  }
  ```
  Note: The exact `Error` variant name may differ by crate version — check the userfaultfd 0.8 API. The key is matching the ioctl errno value against `libc::EEXIST`.
- `signal_error(vm_exit: &SharedVmExit, message: String)` — locks `vm_exit`, stores `VmExit::Error { message }`

**Page alignment:** The faulting `addr` from the kernel is page-aligned. The `read_page(guest_addr)` must return data for that page. The `uffd.copy` `len` parameter is the page size (4096 on x86_64 Linux).

**Testing:**

Tests must verify:
- userfaultd.AC3.2: Create a mock SnapshotStore that tracks `read_page` calls. Set up UFFD with anonymous mmap'd memory. Trigger a page fault by accessing the registered memory from another thread. Verify the mock store's `read_page` was called. Verify the page contains the expected data.
- userfaultd.AC3.3: Test EEXIST handling by having two concurrent faults on the same page (or by pre-populating a page before faulting). Verify no panic or error propagation.

**Verification:**
Run: `cargo test -p vmm --features uffd`
Expected: Tests pass on Linux

**Commit:** `feat(vmm): implement UFFD fault loop with parallel resolution via tokio::spawn`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Wire UFFD handler into restore_and_run_with_store

**Verifies:** userfaultd.AC3.1, userfaultd.AC3.6

**Files:**
- Modify: `src/vmm/src/builder.rs` (modify `restore_from_store` to use UFFD when `uffd` feature enabled)
- Modify: `src/vmm/src/uffd.rs` (add vmstate exchange via oneshot channel)

**Implementation:**

When the `uffd` feature is enabled, the cold restore flow changes from eager (drain preload) to demand-paged:

1. `BuiltVm::restore_from_store()` (modified from Phase 2):
   - Creates guest memory as anonymous mmap (already the default for non-vhost-user)
   - Creates `UffdHandler` and registers all guest memory regions
   - Spawns UFFD handler thread
   - Handler thread calls `store.read_vmstate()`, sends vmstate bytes back to main thread via `tokio::sync::oneshot` channel
   - Main thread receives vmstate, deserializes, validates header, restores device/vCPU states
   - Main thread signals "ready" to handler thread (via another oneshot or atomic flag)
   - Handler thread starts fault loop
   - Main thread resumes vCPUs — page faults are now handled by the UFFD thread

The vmstate exchange via oneshot channel follows the design's "Handler calls `store.read_vmstate()`, sends bytes to main thread via oneshot channel" pattern.

**Conditional compilation:** Use `#[cfg(feature = "uffd")]` to select between eager restore (Phase 2) and UFFD restore:
```rust
#[cfg(feature = "uffd")]
{
    // UFFD path: register memory, spawn handler, exchange vmstate, resume
}
#[cfg(not(feature = "uffd"))]
{
    // Eager path: drain preload stream, populate memory, restore
}
```

**Error signaling (AC3.6):** The UFFD handler thread has access to `SharedVmExit`. When `read_page` fails fatally:
1. Store `VmExit::Error { message }` in `SharedVmExit`
2. Drop the `Uffd` fd — this unblocks all vCPU threads waiting on page faults
3. The main event loop in `Context::run()` will see the `VmExit::Error` on next poll

**Shutdown:** When the VM exits normally:
1. `Context::run()` event loop exits
2. Drop the `VmHandle` (or explicit shutdown)
3. The `Uffd` fd is dropped (Arc refcount reaches 0 when handler thread exits)
4. `AsyncFd::readable()` returns error, fault loop breaks
5. Tokio runtime drops, cancels outstanding tasks
6. Thread joins

**Testing:**

Tests must verify:
- userfaultd.AC3.1: UFFD handler creates fd, registers regions, runs on dedicated thread
- userfaultd.AC3.6: When mock store's `read_page` returns an error, verify `SharedVmExit` gets `VmExit::Error`

These are unit tests with mock SnapshotStore. Full integration testing is in Phase 6.

**Verification:**
Run: `cargo test -p vmm --features uffd`
Expected: Tests pass on Linux

**Commit:** `feat(vmm): wire UFFD handler into cold restore path with vmstate exchange`
<!-- END_TASK_4 -->

<!-- END_SUBCOMPONENT_A -->
