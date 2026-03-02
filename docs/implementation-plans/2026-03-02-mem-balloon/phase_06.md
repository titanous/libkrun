# Memory Balloon Device Implementation Plan — Phase 6: Rust API

**Goal:** Expose balloon control through Builder and VmHandle, enabling host-side resize, target await with stall detection, and stats access.

**Architecture:** Add `enable_balloon()` to Builder (sets flag on VmResources), make `attach_balloon_device` conditional, store `Arc<Mutex<Balloon>>` on Vmm, create `BalloonHandle` with condvar signaling for `actual` changes, add `balloon()` accessor to VmHandle. Condvar fired from `write_config` when guest updates `actual` field.

**Tech Stack:** Rust, std::sync (Condvar, Mutex, Arc)

**Scope:** 7 phases from original design (phase 6 of 7)

**Codebase verified:** 2026-03-02

---

## Acceptance Criteria Coverage

This phase implements and tests:

### mem-balloon.AC4: Rust API enables inflate->snapshot workflow
- **mem-balloon.AC4.1 Success:** `Builder::enable_balloon()` creates balloon device during build
- **mem-balloon.AC4.2 Success:** `VmHandle::balloon()` returns `Some(&BalloonHandle)` when enabled, `None` when not
- **mem-balloon.AC4.3 Success:** `BalloonHandle::resize(target_mb)` triggers guest inflation (guest `actual` increases toward target)
- **mem-balloon.AC4.4 Success:** `BalloonHandle::await_target(target, stall_timeout, max_timeout)` returns `Reached` when target met, `Stalled` when guest stops progressing, `Err(Timeout)` when max_timeout exceeded
- **mem-balloon.AC4.6 Failure:** `resize` on inactive device returns `Err(DeviceNotActive)`
- **mem-balloon.AC4.7 Failure:** `await_target` with `max_timeout = None` and stalled guest returns `Stalled` (does not hang)
- **mem-balloon.AC4.8 Edge:** Concurrent resize updates target; guest inflates toward new target; `await_target` waiters see new target

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->
<!-- START_TASK_1 -->
### Task 1: Add balloon_enabled flag to VmResources

**Verifies:** None (infrastructure prerequisite for AC4.1)

**Files:**
- Modify: `src/vmm/src/resources.rs:184-258` (add `balloon_enabled` field to `VmResources`)

**Implementation:**

Add to `VmResources` struct:
```rust
#[cfg(not(feature = "tee"))]
pub balloon_enabled: bool,
```

The `Default` derive on VmResources will set it to `false`.

Note: The flag is added ONLY to `VmResources`, not to `ContextConfig`. Builder accesses VmResources via `self.config.vmr`, so no ContextConfig change is needed.

**Verification:**
Run: `cargo check -p vmm`
Expected: Compiles without errors

**Commit:** `feat(vmm): add balloon_enabled flag to VmResources`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add Builder::enable_balloon()

**Verifies:** mem-balloon.AC4.1

**Files:**
- Modify: `src/libkrun/src/lib.rs:2436` (add `enable_balloon()` method to `impl Builder` block)

**Implementation:**

Add method to `impl Builder`. Builder accesses VmResources via `self.config.vmr`:
```rust
#[cfg(not(feature = "tee"))]
pub fn enable_balloon(&mut self) -> &mut Self {
    self.config.vmr.balloon_enabled = true;
    self
}
```

`VmResources` flows directly into `build_microvm()`, so no additional propagation is needed.

**Verification:**
Run: `cargo check -p libkrun`
Expected: Compiles without errors

**Commit:** `feat(api): add Builder::enable_balloon() method`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Update attach_balloon_device to return Arc and store on Vmm

**Verifies:** mem-balloon.AC4.1, mem-balloon.AC4.2

**Files:**
- Modify: `src/vmm/src/builder.rs:2860-2879` (update attach_balloon_device to return Arc<Mutex<Balloon>>)

**Implementation:**

The balloon device is always attached to the MMIO bus (unconditionally, as now at builder.rs:1280-1281). This preserves stable MMIO device indices — consoles and other devices depend on balloon being device index 0 for the `Device order: balloon(1) + rng(1) + rtc(1) + ...` calculation at builder.rs:2670.

The `balloon_enabled` flag from VmResources controls only whether the `BalloonHandle` is created and exposed through the Rust API (Task 8), NOT whether the device is attached. An unused balloon device has negligible overhead (no guest interaction without the host setting `num_pages`).

Update `attach_balloon_device` to return the `Arc<Mutex<Balloon>>`:
```rust
fn attach_balloon_device(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
) -> std::result::Result<Arc<Mutex<devices::virtio::Balloon>>, StartMicrovmError> {
    use self::StartMicrovmError::*;

    let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));

    event_manager
        .add_subscriber(balloon.clone())
        .map_err(RegisterEvent)?;

    let id = String::from(balloon.lock().unwrap().id());
    attach_mmio_device(vmm, id, intc.clone(), balloon.clone())
        .map_err(RegisterBalloonDevice)?;

    Ok(balloon)
}
```

Update the call site at builder.rs:1280-1281 to capture the return value:
```rust
#[cfg(not(feature = "tee"))]
let balloon_device = attach_balloon_device(&mut vmm, event_manager, intc.clone())?;
```

Store on Vmm (see Task 4):
```rust
vmm.balloon = Some(balloon_device);
```

**Verification:**
Run: `cargo check -p vmm`
Expected: Compiles without errors

**Commit:** `feat(vmm): return balloon Arc from attach_balloon_device for API access`
<!-- END_TASK_3 -->
<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 4-5) -->
<!-- START_TASK_4 -->
### Task 4: Add balloon field to Vmm and set during build

**Verifies:** None (infrastructure for AC4.2, Phase 4 snapshot integration)

**Note:** Phase 4 Task 4 is the canonical location for this field. If Phase 4 was executed first, the field already exists on Vmm — skip adding it and only update `build_microvm` to store the returned Arc from Task 3. If Phase 6 executes first, add the field here.

**Files:**
- Modify: `src/vmm/src/lib.rs:216-253` (add `balloon` field to `Vmm` struct — skip if already exists from Phase 4)
- Modify: `src/vmm/src/builder.rs` (set balloon field during VM construction)

**Implementation:**

Add to `Vmm` struct:
```rust
#[cfg(not(feature = "tee"))]
pub(crate) balloon: Option<Arc<Mutex<devices::virtio::Balloon>>>,
```

Initialize in `build_microvm` after `attach_balloon_device` returns:
```rust
vmm.balloon = balloon_device;  // Option<Arc<Mutex<Balloon>>> from Task 3
```

Note: Phase 4 Task 4 also specifies adding this field. If Phase 4 is executed first, this task merges into it. If Phase 6 is executed first, Phase 4 will find the field already present and skip adding it. The implementation plan handles this by having both phases mention the field — the executor should check if it already exists.

**Verification:**
Run: `cargo check -p vmm`
Expected: Compiles without errors

**Commit:** `feat(vmm): store balloon device reference on Vmm struct`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Add condvar to Balloon device for actual-change notification

**Verifies:** None (infrastructure for AC4.4)

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs:48-55` (add condvar field to Balloon struct)
- Modify: `src/devices/src/virtio/balloon/device.rs:154-160` (update write_config to notify condvar)

**Implementation:**

Add to the `Balloon` struct:
```rust
actual_condvar: Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
```

Initialize in `Balloon::new()`:
```rust
actual_condvar: Arc::new((std::sync::Mutex::new(0), std::sync::Condvar::new())),
```

Add public accessor:
```rust
pub fn actual_condvar(&self) -> Arc<(std::sync::Mutex<u64>, std::sync::Condvar)> {
    self.actual_condvar.clone()
}
```

In `write_config()` (Phase 1 Task 5 implements the actual-field write logic), after writing the `actual` bytes to config, detect if the `actual` field was touched and notify:

```rust
fn write_config(&mut self, offset: u64, data: &[u8]) {
    let config_slice = self.config.as_mut_slice();
    let config_len = config_slice.len() as u64;

    // Only allow writes to the 'actual' field (bytes 4..8)
    for i in 0..data.len() as u64 {
        let byte_offset = offset + i;
        if byte_offset >= 4 && byte_offset < 8 {
            config_slice[byte_offset as usize] = data[i as usize];
        }
    }

    // If write touched the actual field, notify waiters
    if offset < 8 && offset + data.len() as u64 > 4 {
        let actual_pages = self.config.actual;
        let (lock, cvar) = &*self.actual_condvar;
        if let Ok(mut val) = lock.lock() {
            *val = actual_pages as u64;
            cvar.notify_all();
        }
    }
}
```

This builds on Phase 1 Task 5's `write_config` implementation, adding condvar notification.

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(balloon): add condvar notification on actual field update`
<!-- END_TASK_5 -->
<!-- END_SUBCOMPONENT_B -->

<!-- START_SUBCOMPONENT_C (tasks 6-8) -->
<!-- START_TASK_6 -->
### Task 6: Create BalloonHandle, BalloonResult, and BalloonError types

**Verifies:** None (types prerequisite for AC4.2-AC4.8)

**Files:**
- Modify: `src/libkrun/src/lib.rs` (add BalloonHandle struct, BalloonResult enum, BalloonError enum near VmHandle definition)

**Implementation:**

Add the types near the `VmHandle` definition:

```rust
#[cfg(not(feature = "tee"))]
pub use devices::virtio::balloon::BalloonStats;

/// Handle for controlling the memory balloon device.
#[cfg(not(feature = "tee"))]
pub struct BalloonHandle {
    balloon: Arc<Mutex<devices::virtio::Balloon>>,
    actual_condvar: Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
}

/// Result of awaiting a balloon resize target.
#[cfg(not(feature = "tee"))]
#[derive(Debug)]
pub enum BalloonResult {
    /// Target reached — actual >= target
    Reached(u64),
    /// Guest stopped making progress — actual stalled at this value
    Stalled(u64),
}

/// Error from balloon operations.
#[cfg(not(feature = "tee"))]
#[derive(Debug)]
pub enum BalloonError {
    /// Maximum timeout exceeded
    Timeout { actual: u64 },
    /// Balloon device not activated
    DeviceNotActive,
}
```

Feature-gated with `not(tee)` matching the balloon device's gate.

**Verification:**
Run: `cargo check -p libkrun`
Expected: Compiles without errors

**Commit:** `feat(api): add BalloonHandle, BalloonResult, and BalloonError types`
<!-- END_TASK_6 -->

<!-- START_TASK_7 -->
### Task 7: Implement BalloonHandle methods (resize, await_target, actual, stats)

**Verifies:** mem-balloon.AC4.3, mem-balloon.AC4.4, mem-balloon.AC4.6, mem-balloon.AC4.7, mem-balloon.AC4.8

**Files:**
- Modify: `src/libkrun/src/lib.rs` (add `impl BalloonHandle` block with methods)

**Implementation:**

```rust
#[cfg(not(feature = "tee"))]
impl BalloonHandle {
    fn new(
        balloon: Arc<Mutex<devices::virtio::Balloon>>,
        actual_condvar: Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
    ) -> Self {
        BalloonHandle {
            balloon,
            actual_condvar,
        }
    }
```

**`resize(&self, target_mb: u64) -> Result<(), BalloonError>`:**
1. Lock balloon: `self.balloon.lock().unwrap()`
2. Check device is activated (device_state is not Inactive). If inactive, return `Err(BalloonError::DeviceNotActive)` (AC4.6)
3. Convert MB to pages: `let target_pages = (target_mb * 1024 * 1024) / 4096`
4. Write `num_pages` in config: `balloon.config.num_pages = target_pages as u32`
5. Signal config change: `balloon.device_state.signal_config_change()` — this notifies the guest to read the new num_pages value
6. Return `Ok(())`

Concurrent resize (AC4.8): The method simply overwrites `num_pages`. The guest will see the latest value when it reads config. `await_target` waiters are woken by condvar on each `actual` update and re-check against the new target.

**`await_target(&self, target_mb: u64, stall_timeout: Duration, max_timeout: Option<Duration>) -> Result<BalloonResult, BalloonError>`:**
1. Convert target_mb to pages: `let target_pages = (target_mb * 1024 * 1024) / 4096`
2. Get condvar: `let (lock, cvar) = &*self.actual_condvar`
3. Lock the mutex: `let mut actual = lock.lock().unwrap()`
4. Record `start = Instant::now()`
5. Loop:
   a. If `*actual >= target_pages`: return `Ok(BalloonResult::Reached(*actual * 4096 / (1024 * 1024)))` (AC4.4 Reached)
   b. If `max_timeout` exceeded: return `Err(BalloonError::Timeout { actual: *actual * 4096 / (1024 * 1024) })` (AC4.4 Timeout)
   c. Wait on condvar with `stall_timeout`: `let (new_actual, timeout_result) = cvar.wait_timeout(actual, stall_timeout).unwrap()`
   d. `actual = new_actual`
   e. If `timeout_result.timed_out()`: guest didn't update actual for stall_timeout — return `Ok(BalloonResult::Stalled(*actual * 4096 / (1024 * 1024)))` (AC4.7)
   f. Continue loop (condvar was signaled, re-check target)

Note: Convert actual back to MB for return values. Use pages internally for comparison.

**`actual(&self) -> u64`:**
```rust
let balloon = self.balloon.lock().unwrap();
let actual_pages = balloon.config.actual as u64;
actual_pages * 4096 / (1024 * 1024)  // Convert pages to MB
```

**`stats(&self) -> Option<BalloonStats>`:**
```rust
let balloon = self.balloon.lock().unwrap();
balloon.stats().cloned()
```

Calls the `stats()` method added in Phase 2 Task 2. Returns `None` before first stats collection.

**Testing:**
Tests must verify:
- AC4.3: After `resize(target)`, balloon config `num_pages` is set to target in pages, config change is signaled
- AC4.4: `await_target` returns `Reached` when actual >= target; returns `Stalled` after stall_timeout; returns `Err(Timeout)` after max_timeout
- AC4.6: `resize` on inactive device returns `Err(DeviceNotActive)`
- AC4.7: `await_target` with stalled guest returns `Stalled` (no hang)
- AC4.8: Concurrent resize overwrites target; waiter sees new target

**Verification:**
Run: `cargo check -p libkrun`
Expected: Compiles without errors

**Commit:** `feat(api): implement BalloonHandle resize, await_target, actual, and stats`
<!-- END_TASK_7 -->

<!-- START_TASK_8 -->
### Task 8: Add balloon() accessor to VmHandle and populate during build

**Verifies:** mem-balloon.AC4.2

**Files:**
- Modify: `src/libkrun/src/lib.rs:3357-3360` (add `balloon` field to `VmHandle`)
- Modify: `src/libkrun/src/lib.rs` (add `balloon()` method to `impl VmHandle`)
- Modify: `src/libkrun/src/lib.rs` (populate balloon field when creating VmHandle during build)

**Implementation:**

Add field to `VmHandle`:
```rust
pub struct VmHandle {
    vmm: Arc<Mutex<vmm::Vmm>>,
    shutdown_efd: Option<Arc<EventFd>>,
    #[cfg(not(feature = "tee"))]
    balloon: Option<BalloonHandle>,
}
```

Add accessor method:
```rust
#[cfg(not(feature = "tee"))]
pub fn balloon(&self) -> Option<&BalloonHandle> {
    self.balloon.as_ref()
}
```

When `VmHandle` is constructed (find the location where `VmHandle { vmm, shutdown_efd }` is created in the build path), populate the balloon field. The balloon device is always attached to the MMIO bus, but the `BalloonHandle` is only created when `balloon_enabled` is true. When not enabled, `balloon()` returns `None` (AC4.2):

```rust
#[cfg(not(feature = "tee"))]
let balloon_handle = if vm_resources.balloon_enabled {
    let vmm_guard = vmm.lock().unwrap();
    vmm_guard.balloon.as_ref().map(|b| {
        let condvar = b.lock().unwrap().actual_condvar();
        BalloonHandle::new(b.clone(), condvar)
    })
} else {
    None
};
```

Then include in VmHandle construction:
```rust
VmHandle {
    vmm,
    shutdown_efd,
    #[cfg(not(feature = "tee"))]
    balloon: balloon_handle,
}
```

Find the exact VmHandle construction site by searching for `VmHandle {` in lib.rs. There may be multiple construction sites (e.g., `run()`, `restore_and_run()`). All must populate the balloon field.

**Testing:**
Tests must verify:
- AC4.2: When balloon enabled, `vm_handle.balloon()` returns `Some`. When not enabled, returns `None`.

**Verification:**
Run: `cargo check -p libkrun`
Expected: Compiles without errors

Run: `cargo build -p libkrun`
Expected: Full build succeeds

**Commit:** `feat(api): add balloon() accessor to VmHandle`
<!-- END_TASK_8 -->
<!-- END_SUBCOMPONENT_C -->

<!-- START_TASK_9 -->
### Task 9: Verify full phase builds

**Verifies:** None (verification)

**Files:** None

**Verification:**
Run: `cargo build -p libkrun`
Expected: Builds without errors

Run: `cargo build -p vmm`
Expected: Builds without errors

Run: `cargo build -p devices`
Expected: Builds without errors

**Commit:** Not needed if previous tasks committed individually.
<!-- END_TASK_9 -->
