# Async Net Loopback Implementation Plan — Phase 1

**Goal:** Remove all proxy-specific code and dependencies from the devices crate and test workspace.

**Architecture:** Delete `ProxyNetWorker`, its `VirtioNetBackend::Proxy` enum variant, proxy-only dependencies, and the proxy integration test. The `net` feature flag retains only `tokio` and `bytes` (needed by `AsyncNetWorker` and `AsyncNetBackend`).

**Tech Stack:** Rust, Cargo features

**Scope:** 2 phases from original design (phase 1 of 2)

**Codebase verified:** 2026-02-24

---

## Acceptance Criteria Coverage

This phase implements:

### async-net-loopback.AC1: Proxy code and dependencies removed
- **async-net-loopback.AC1.1 Success:** `proxy.rs` deleted, no `pub mod proxy` in `mod.rs`
- **async-net-loopback.AC1.2 Success:** `VirtioNetBackend::Proxy` variant removed from enum
- **async-net-loopback.AC1.3 Success:** `net` feature flag contains only `tokio` and `bytes`
- **async-net-loopback.AC1.4 Success:** `cargo build --features net` succeeds

### async-net-loopback.AC4: All tests pass (partial)
- **async-net-loopback.AC4.2 Success:** `cargo build --features net` succeeds (no compilation errors from removal)

---

<!-- START_TASK_1 -->
### Task 1: Delete proxy.rs and remove module declaration

**Files:**
- Delete: `src/devices/src/virtio/net/proxy.rs`
- Modify: `src/devices/src/virtio/net/mod.rs:19` (remove `pub mod proxy;`)

**Step 1: Delete proxy.rs**

```bash
rm src/devices/src/virtio/net/proxy.rs
```

**Step 2: Remove module declaration from mod.rs**

In `src/devices/src/virtio/net/mod.rs`, delete line 19:

```rust
pub mod proxy;
```

The file should go from:

```rust
mod backend;
pub mod device;
pub mod proxy;
#[cfg(target_os = "linux")]
mod tap;
```

To:

```rust
mod backend;
pub mod device;
#[cfg(target_os = "linux")]
mod tap;
```

**Step 3: Commit**

```bash
git add -u src/devices/src/virtio/net/proxy.rs src/devices/src/virtio/net/mod.rs
git commit -m "remove proxy.rs and pub mod proxy declaration"
```

Note: The build will NOT compile after this step until the remaining proxy references in device.rs and worker.rs are cleaned up. That's expected — the next tasks address those references.
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Remove VirtioNetBackend::Proxy from device.rs

**Files:**
- Modify: `src/devices/src/virtio/net/device.rs`

**Step 1: Remove the proxy import**

Delete line 18:

```rust
use super::proxy::ProxyNetWorker;
```

**Step 2: Remove the Proxy variant from the VirtioNetBackend enum**

In the `VirtioNetBackend` enum (starting at line 77), delete the `Proxy` variant and its doc comments (lines 87-91):

```rust
    /// Use smoltcp-based ProxyNetWorker as the network backend.
    /// `listeners` maps VM-side ports to host Unix socket paths for ingress connections.
    Proxy {
        listeners: Vec<(u16, String)>,
    },
```

The enum should end with `CustomAsyncFactory` as the last variant:

```rust
pub enum VirtioNetBackend {
    UnixstreamFd(RawFd),
    UnixstreamPath(PathBuf),
    UnixgramFd(RawFd),
    UnixgramPath(PathBuf, bool),
    #[cfg(target_os = "linux")]
    Tap(String),
    /// Custom async backend using factory pattern.
    /// The factory creates the backend inside the worker's tokio runtime.
    CustomAsyncFactory(Box<dyn AsyncNetBackendFactory>),
}
```

**Step 3: Remove the Proxy arm from the Clone impl**

In the `impl Clone for VirtioNetBackend` (line 94-109), delete lines 104-106:

```rust
            Self::Proxy { listeners } => Self::Proxy {
                listeners: listeners.clone(),
            },
```

**Step 4: Remove the Proxy match arm from activate()**

In the `activate` method (line 270), delete the entire `VirtioNetBackend::Proxy` match arm (lines 316-348):

```rust
            VirtioNetBackend::Proxy { listeners } => {
                debug!("virtio-net ({}): starting proxy worker", self.id());
                let queue_list = vec![rx_q.queue.clone(), tx_q.queue.clone()];
                let queue_evts = vec![
                    rx_q.event.try_clone().unwrap(),
                    tx_q.event.try_clone().unwrap(),
                ];
                let interrupt_status = interrupt.status_arc();
                let interrupt_evt = interrupt.event().try_clone().unwrap();
                let intc = Some(interrupt.intc().clone());
                let irq_line = interrupt.irq_line();

                match ProxyNetWorker::new(
                    queue_list,
                    queue_evts,
                    interrupt_status,
                    interrupt_evt,
                    intc,
                    irq_line,
                    mem.clone(),
                    listeners,
                ) {
                    Ok(worker) => {
                        std::thread::spawn(move || worker.run());
                        self.device_state = DeviceState::Activated(mem, interrupt);
                        Ok(())
                    }
                    Err(err) => {
                        error!("Error activating ProxyNetWorker: {err:?}");
                        Err(ActivateError::BadActivate)
                    }
                }
            }
```

**Step 5: Commit**

```bash
git add src/devices/src/virtio/net/device.rs
git commit -m "remove VirtioNetBackend::Proxy variant and activation arm"
```
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Remove Proxy panic arm from worker.rs

**Files:**
- Modify: `src/devices/src/virtio/net/worker.rs:71-73`

**Step 1: Remove the Proxy panic arm**

In `src/devices/src/virtio/net/worker.rs`, in the `NetWorker::new` method, delete lines 71-73:

```rust
            VirtioNetBackend::Proxy { .. } => {
                panic!("Proxy should use ProxyNetWorker, not NetWorker")
            }
```

**Step 2: Commit**

```bash
git add src/devices/src/virtio/net/worker.rs
git commit -m "remove Proxy panic arm from NetWorker::new"
```
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Trim net feature and remove proxy-only dependencies

**Files:**
- Modify: `src/devices/Cargo.toml`

**Step 1: Trim the net feature flag**

In `src/devices/Cargo.toml`, change line 12 from:

```toml
net = ["tokio", "bytes", "mio", "pnet", "pnet_base", "smoltcp", "socket2", "tracing"]
```

To:

```toml
net = ["tokio", "bytes"]
```

**Step 2: Remove proxy-only dependency entries**

Delete these 6 lines from the `[dependencies]` section (lines 51-56):

```toml
mio = { version = "1.1", optional = true }
pnet = { version = "0.35", optional = true }
pnet_base = { version = "0.35", optional = true }
smoltcp = { version = "0.12", optional = true }
socket2 = { version = "0.6", optional = true }
tracing = { version = "0.1", optional = true }
```

Keep `tokio` (line 48) and `bytes` (line 50) — they are used by `async_worker.rs` and `async_backend.rs`.

**Step 3: Verify build**

```bash
cargo build -p devices --features net
```

Expected: Builds successfully with no errors.

**Step 4: Commit**

```bash
git add src/devices/Cargo.toml
git commit -m "trim net feature to tokio+bytes, remove proxy-only deps"
```
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Delete proxy integration test and update test registry

**Files:**
- Delete: `tests/test_cases/src/test_net_proxy.rs`
- Modify: `tests/test_cases/src/lib.rs:31-32,71`

**Step 1: Delete the test file**

```bash
rm tests/test_cases/src/test_net_proxy.rs
```

**Step 2: Remove module declaration and import from lib.rs**

In `tests/test_cases/src/lib.rs`, delete lines 31-32:

```rust
mod test_net_proxy;
use test_net_proxy::TestNetProxy;
```

**Step 3: Remove the test case registration from lib.rs**

In the `test_cases()` function, delete line 71:

```rust
        TestCase::new("net-proxy-ping-pong", Box::new(TestNetProxy)),
```

The `test_cases()` vec should end with:

```rust
        TestCase::new("custom-block-backend", Box::new(TestCustomBlockBackend)),
    ]
```

**Step 4: Commit**

```bash
git add -u tests/test_cases/src/test_net_proxy.rs tests/test_cases/src/lib.rs
git commit -m "delete proxy integration test and remove from registry"
```
<!-- END_TASK_5 -->

<!-- START_TASK_6 -->
### Task 6: Update CLAUDE.md files

**Files:**
- Modify: `src/devices/src/virtio/net/CLAUDE.md`
- Modify: `tests/CLAUDE.md`
- Modify: `CLAUDE.md` (root)

**Step 1: Update virtio-net CLAUDE.md**

Replace the entire contents of `src/devices/src/virtio/net/CLAUDE.md` with:

```markdown
# Virtio Net

Last verified: 2026-02-24

## Purpose
Implements virtio-net device with multiple backend strategies: synchronous (tap, unix) and async (tokio).

## Contracts
- **Exposes**: `Net` virtio device, `VirtioNetBackend` enum (Tap, UnixGram, UnixStream, CustomAsyncFactory), `AsyncNetBackend` trait
- **Guarantees**:
  - `VirtioNetBackend::CustomAsyncFactory(factory)` activates an `AsyncNetWorker` that bridges virtio queues to a user-supplied `AsyncNetBackend`
  - `InterruptTransport::status_arc()` provides `Arc<AtomicUsize>` for workers needing shared interrupt status
  - Sync backends use `NetWorker`; async backends use `AsyncNetWorker`
- **Expects**: Valid virtio queues and guest memory from MMIO activation

## Dependencies
- **Uses**: `tokio` (async worker), `bytes` (packet buffers), `vm-memory`
- **Used by**: `libkrun` (configures backend via `VirtioNetBackend` enum)
- **Boundary**: `tokio` and `bytes` deps gated behind `net` feature flag

## Key Decisions
- `NetWorker::new` panics if given `CustomAsyncFactory` variant (wrong worker type)

## Invariants
- `NetWorker::new` panics on `CustomAsyncFactory` (must use `AsyncNetWorker` instead)

## Key Files
- `device.rs` - `Net` virtio device, `VirtioNetBackend` enum, activation logic
- `async_backend.rs` - `AsyncNetBackend` trait, `AsyncNetBackendFactory` trait, `NetBackendHandle`
- `async_worker.rs` - Tokio-based async net worker
- `worker.rs` - Synchronous net worker (tap/unix backends)
- `mod.rs` - Module declarations and re-exports
```

**Step 2: Update tests/CLAUDE.md**

In `tests/CLAUDE.md`, in the `## Test Cases` section, remove the line:

```
- `net-proxy-ping-pong` - ProxyNetWorker TCP round-trip through smoltcp stack
```

**Step 3: Update root CLAUDE.md**

In `CLAUDE.md` (root), in the `## Tech Stack` section, change:

```
- Network stack: smoltcp (proxy mode), tokio (async workers)
```

To:

```
- Network stack: tokio (async workers)
```

Also in `## Commands`, change:

```
- `cargo test -p devices --features net` - Run devices crate unit tests (net feature needed for proxy/async_worker tests)
```

To:

```
- `cargo test -p devices --features net` - Run devices crate unit tests (net feature needed for async_worker tests)
```

Also in `## Feature Flags (Cargo)`, change:

```
- `net` - Enables virtio-net backends (tokio, smoltcp proxy deps)
```

To:

```
- `net` - Enables virtio-net async backend (tokio, bytes)
```

**Step 4: Commit**

```bash
git add src/devices/src/virtio/net/CLAUDE.md tests/CLAUDE.md CLAUDE.md
git commit -m "update CLAUDE.md files to reflect proxy removal"
```
<!-- END_TASK_6 -->

<!-- START_TASK_7 -->
### Task 7: Verify full build

**Verification:**

Run:
```bash
cargo build -p devices --features net
```
Expected: Build succeeds with no errors.

Run:
```bash
cargo build -p devices --features net,snapshot
```
Expected: Build succeeds with no errors.

This task is verification-only — no code changes, no commit.
<!-- END_TASK_7 -->
