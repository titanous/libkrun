# Vhost-User-VSock Implementation Plan - Phase 3

**Goal:** Wire VhostUserVsock into VM construction and expose via the Rust Builder API, with mutual exclusivity enforcement against the userspace vsock.

**Architecture:** Add `vhost_user_vsock` field to `VmResources`, create `attach_vhost_user_vsock_device()` in builder.rs (simpler than VhostUserFs — no DAX, no event manager subscription), add `add_vsock_vhost_user()` and `add_vsock_vhost_user_fd()` to the Rust Builder API. The build flow checks `vhost_user_vsock` before creating the userspace vsock, skipping it when vhost-user-vsock is configured. Mutual exclusivity is enforced bidirectionally in both the C and Rust APIs.

**Tech Stack:** Rust

**Scope:** 3 of 6 phases from original design

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-vsock.AC2: API enforces mutual exclusivity with userspace vsock
- **vhost-user-vsock.AC2.1 Success:** add_vsock_vhost_user() configures VM with vhost-user-vsock when no userspace vsock is configured
- **vhost-user-vsock.AC2.2 Success:** add_vsock_vhost_user_fd() configures VM with pre-provisioned fd
- **vhost-user-vsock.AC2.3 Failure:** Calling both krun_add_vsock() and add_vsock_vhost_user() returns error
- **vhost-user-vsock.AC2.4 Failure:** Calling both add_vsock_vhost_user() and krun_add_vsock() returns error (either order)

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->
<!-- START_TASK_1 -->
### Task 1: Add VmResources field and builder attachment function

**Files:**
- Modify: `src/vmm/src/resources.rs` (add `vhost_user_vsock` field and setter)
- Modify: `src/vmm/src/builder.rs` (add `attach_vhost_user_vsock_device()`, add error variant, call from build flow)

**Implementation:**

**`src/vmm/src/resources.rs`** — Add to the VmResources struct, alongside the existing `vhost_user_fs` field:

```rust
#[cfg(feature = "vhost-user")]
pub vhost_user_vsock: Option<VhostUserVsockConfig>,
```

Add import at the top of resources.rs:
```rust
#[cfg(feature = "vhost-user")]
use crate::vmm_config::vhost_user_vsock::VhostUserVsockConfig;
```

Initialize the field in VmResources construction (Default or new) as `None`.

Add setter method to `impl VmResources`:
```rust
#[cfg(feature = "vhost-user")]
pub fn set_vhost_user_vsock(&mut self, config: VhostUserVsockConfig) {
    self.vhost_user_vsock = Some(config);
}
```

**`src/vmm/src/builder.rs`** — Add error variant to `StartMicrovmError`:

Add after the existing `RegisterVhostUserFsDevice` variant:
```rust
/// Cannot initialize a MMIO vhost-user vsock device or add device to the MMIO Bus.
#[cfg(feature = "vhost-user")]
RegisterVhostUserVsockDevice(device_manager::mmio::Error),
```

Add Display impl for the new variant in the existing `fmt::Display for StartMicrovmError` block:
```rust
#[cfg(feature = "vhost-user")]
RegisterVhostUserVsockDevice(ref err) => {
    write!(f, "Failed to initialize vhost-user vsock device: {err}")
}
```

Add the attachment function (place near `attach_vhost_user_fs_device`):

```rust
#[cfg(not(feature = "tee"))]
#[cfg(feature = "vhost-user")]
fn attach_vhost_user_vsock_device(
    vmm: &mut Vmm,
    config: VhostUserVsockConfig,
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    use devices::virtio::vhost_user::VhostUserVsock;
    use crate::vmm_config::vhost_user_vsock::VhostUserVsockConnection;
    use StartMicrovmError::*;

    let vhost_vsock = match config.connection {
        VhostUserVsockConnection::SocketPath(ref path) => {
            VhostUserVsock::new(path).map_err(RegisterVhostUserDevice)?
        }
        VhostUserVsockConnection::Stream(stream) => {
            VhostUserVsock::from_stream(stream).map_err(RegisterVhostUserDevice)?
        }
    };

    let device = Arc::new(Mutex::new(vhost_vsock));
    let id = "virtio-vsock-vhost".to_string();
    attach_mmio_device(vmm, id, intc, device)
        .map_err(RegisterVhostUserVsockDevice)?;

    Ok(())
}
```

**Update `use_vhost_user` checks for file-backed memory.** The `build_microvm()` function has two locations that compute `use_vhost_user` to determine if guest memory needs memfd backing (required for vhost-user backends to mmap guest memory). Both must include `vhost_user_vsock`. Find the existing checks (around lines 1667-1669 and 1908-1910) which look like:

```rust
let use_vhost_user = !vhost_user_devices.is_empty() || !vm_resources.vhost_user_fs.is_empty();
```

Add `|| vm_resources.vhost_user_vsock.is_some()` to both:

```rust
let use_vhost_user = !vhost_user_devices.is_empty()
    || !vm_resources.vhost_user_fs.is_empty()
    || vm_resources.vhost_user_vsock.is_some();
```

**Wire into the build flow in `build_microvm()`.** After the existing vsock attachment block (around line 1376-1385), add:

```rust
#[cfg(feature = "vhost-user")]
if let Some(vhost_vsock_config) = vm_resources.vhost_user_vsock.take() {
    attach_vhost_user_vsock_device(&mut vmm, vhost_vsock_config, intc.clone())?;
}
```

**Update the build flow to skip userspace vsock when vhost-user-vsock is configured.** In `lib.rs`, the vsock creation block (around line 2943) matches on `vsock_config`. Wrap the entire match in a check for `vhost_user_vsock`:

```rust
#[cfg(feature = "vhost-user")]
let skip_userspace_vsock = cfg.config.vhost_user_vsock;
#[cfg(not(feature = "vhost-user"))]
let skip_userspace_vsock = false;

if !skip_userspace_vsock {
    match cfg.config.vsock_config {
        // ... existing Disabled/Explicit/Implicit handling
    }
}
```

This allows `krun_add_vsock_port()` to store port configs without error (matching design intent: "The API does not error — the config is stored but unused"), while preventing the userspace vsock device from being created when vhost-user-vsock is configured.

Note: The `take()` on `vhost_user_vsock` consumes the config (which holds an `UnixStream` that can't be cloned).

**Verification:**

```bash
cd /home/titanous/vm-platform/libkrun/.worktrees/vhost-user-vsock && cargo check -p vmm --features vhost-user
```
Expected: compiles without errors.

**Commit:** `feat(vmm): add vhost-user-vsock builder attachment`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Rust Builder API methods and mutual exclusivity

**Files:**
- Modify: `src/libkrun/src/lib.rs` (add `add_vsock_vhost_user()`, `add_vsock_vhost_user_fd()`, add `VsockConflict` error variant, add mutual exclusivity checks)

**Verifies:** vhost-user-vsock.AC2.1, vhost-user-vsock.AC2.2, vhost-user-vsock.AC2.3, vhost-user-vsock.AC2.4

**Implementation:**

**Add error variant to `StartError`:**

```rust
#[error("cannot configure both userspace vsock and vhost-user vsock")]
VsockConflict,
```

**Add field to `ContextConfig`** (around line 184, after `unix_ipc_port_map`):

```rust
#[cfg(feature = "vhost-user")]
vhost_user_vsock: bool,
```

This boolean tracks whether `add_vsock_vhost_user` was called. The actual config is stored in `vmr.vhost_user_vsock`.

**Add Rust Builder API methods to `impl Builder`** (alongside `add_virtiofs_vhost_user`):

```rust
/// Configure a vhost-user-vsock device via Unix socket path.
///
/// The backend process must be listening at `socket_path` before the VM starts.
/// Cannot be used together with `krun_add_vsock()` (userspace vsock).
#[cfg(not(feature = "tee"))]
#[cfg(feature = "vhost-user")]
pub fn add_vsock_vhost_user(
    &mut self,
    socket_path: &str,
) -> Result<&mut Self, StartError> {
    use vmm::vmm_config::vhost_user_vsock::{VhostUserVsockConfig, VhostUserVsockConnection};

    // Reject if explicit userspace vsock was already configured via krun_add_vsock()
    if matches!(self.config.vsock_config, VsockConfig::Explicit { .. }) {
        return Err(StartError::VsockConflict);
    }
    if self.config.vhost_user_vsock {
        return Err(StartError::VsockConflict);
    }

    // Do NOT set vsock_config = Disabled — leave it as Implicit so that
    // krun_add_vsock_port() still accepts port configs (stored but unused,
    // per design: "The API does not error — the config is stored but unused").
    // The build flow in lib.rs checks vhost_user_vsock to skip userspace vsock creation.
    self.config.vhost_user_vsock = true;

    self.config.vmr.set_vhost_user_vsock(VhostUserVsockConfig {
        connection: VhostUserVsockConnection::SocketPath(socket_path.to_string()),
    });
    Ok(self)
}

/// Configure a vhost-user-vsock device via pre-provisioned file descriptor.
///
/// The `stream` must be a connected UnixStream to the vhost-user backend.
/// Cannot be used together with `krun_add_vsock()` (userspace vsock).
#[cfg(not(feature = "tee"))]
#[cfg(feature = "vhost-user")]
pub fn add_vsock_vhost_user_fd(
    &mut self,
    stream: std::os::unix::net::UnixStream,
) -> Result<&mut Self, StartError> {
    use vmm::vmm_config::vhost_user_vsock::{VhostUserVsockConfig, VhostUserVsockConnection};

    if matches!(self.config.vsock_config, VsockConfig::Explicit { .. }) {
        return Err(StartError::VsockConflict);
    }
    if self.config.vhost_user_vsock {
        return Err(StartError::VsockConflict);
    }

    self.config.vhost_user_vsock = true;

    self.config.vmr.set_vhost_user_vsock(VhostUserVsockConfig {
        connection: VhostUserVsockConnection::Stream(stream),
    });
    Ok(self)
}
```

**Add reverse check in `krun_add_vsock()`** (around line 2193):

Add before the existing `if cfg.config.vsock_config != VsockConfig::Disabled` check:

```rust
#[cfg(feature = "vhost-user")]
if cfg.config.vhost_user_vsock {
    return -libc::EEXIST;
}
```

**Testing:**

The mutual exclusivity is tested via unit tests in Task 3.

**Verification:**

```bash
cargo check -p libkrun --features vhost-user
```
Expected: compiles without errors.

**Commit:** `feat(api): add vhost-user-vsock Builder API with mutual exclusivity`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Unit tests for mutual exclusivity

**Verifies:** vhost-user-vsock.AC2.1, vhost-user-vsock.AC2.2, vhost-user-vsock.AC2.3, vhost-user-vsock.AC2.4

**Files:**
- Modify: `src/libkrun/src/lib.rs` (add test module or extend existing tests)

**Testing:**

The mutual exclusivity tests verify the API constraints at the Builder level. These test the public API behavior.

Tests must verify:
- AC2.1: `add_vsock_vhost_user()` succeeds when no explicit vsock configured (vsock_config is Implicit or Disabled)
- AC2.2: `add_vsock_vhost_user_fd()` succeeds with a UnixStream (use `UnixStream::pair()` for test)
- AC2.3: calling `krun_add_vsock()` after `add_vsock_vhost_user()` returns EEXIST
- AC2.4: calling `add_vsock_vhost_user()` after explicit vsock config returns VsockConflict

Since the Builder's `new()` creates a full VM configuration including firmware loading, and test binaries don't have libkrunfw available, these tests should verify the error conditions by directly manipulating ContextConfig fields rather than going through the full Builder lifecycle. If the Builder can be constructed in tests, use it; otherwise, document that full integration testing of the success path (AC2.1, AC2.2) is deferred to Phase 6 integration tests.

At minimum, test the error paths (AC2.3, AC2.4) since those are pure logic checks that don't require VM construction.

**Verification:**

```bash
cargo test -p libkrun --features vhost-user -- vsock
```
Expected: all tests pass.

**Commit:** `test(api): add mutual exclusivity tests for vhost-user-vsock`
<!-- END_TASK_3 -->
<!-- END_SUBCOMPONENT_A -->
