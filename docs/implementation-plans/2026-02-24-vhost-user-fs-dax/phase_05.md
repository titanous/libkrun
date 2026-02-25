# VhostUserFs with DAX Implementation Plan - Phase 5

**Goal:** Add DEVICE_STATE protocol support to `VhostUserDevice` for daemon state transfer during snapshot/restore.

**Architecture:** The vhost crate v0.15 includes wire-format types for DEVICE_STATE (message constants, body structs, enums) and backend-side handler traits, but the `Frontend` struct has no methods for *sending* SET_DEVICE_STATE_FD or CHECK_DEVICE_STATE messages. This is an upstream gap: the backend can receive them, but the frontend cannot send them. This phase patches the vhost crate to add these two methods to `VhostUserFrontend` and `Frontend`, following the same internal patterns as existing Frontend methods (like `add_mem_region`). The patch is a candidate for upstream contribution to rust-vmm/vhost. With the patched crate, `VhostUserDevice` adds `save_device_state()` and `load_device_state()` helpers that wrap the Frontend methods with pipe creation and data transfer logic.

**Tech Stack:** Rust, vhost crate v0.15 (patched with Frontend DEVICE_STATE methods), Unix pipes

**Scope:** 8 phases from original design (phase 5 of 8)

**Codebase verified:** 2026-02-24

**Reference files:**
- VhostUserDevice: `src/devices/src/virtio/vhost_user/device.rs` (from Phase 1/PR #527)
- vhost crate Frontend: `vhost/src/vhost_user/frontend.rs` (trait at lines 25-101, impl at lines 367-599)
- vhost crate wire types: `vhost/src/vhost_user/message.rs` — `FrontendReq::SET_DEVICE_STATE_FD` (42), `CHECK_DEVICE_STATE` (43), `VhostUserTransferDeviceState`, `VhostUserU64`
- vhost crate enums: `VhostTransferStateDirection` (SAVE=0, LOAD=1), `VhostTransferStatePhase` (STOPPED=0)
- vhost crate protocol features: `VhostUserProtocolFeatures::DEVICE_STATE` (bit 19, `0x0008_0000`)

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-fs-dax.AC4: Snapshot/restore preserves device and daemon state
- **vhost-user-fs-dax.AC4.1 Success:** SET_DEVICE_STATE_FD (msg 42) transfers daemon state to VMM via pipe
- **vhost-user-fs-dax.AC4.2 Success:** CHECK_DEVICE_STATE (msg 43) confirms daemon finished state transfer

---

## Frontend DEVICE_STATE Gap

The vhost crate v0.15 has all wire-format types for DEVICE_STATE:
- `FrontendReq::SET_DEVICE_STATE_FD` (42) and `CHECK_DEVICE_STATE` (43) message opcodes
- `VhostUserTransferDeviceState` request body struct
- `VhostUserU64` reply body struct
- `VhostTransferStateDirection`, `VhostTransferStatePhase` enums
- `VhostUserProtocolFeatures::DEVICE_STATE` (bit 19) protocol feature flag

It also has backend-side handlers (`VhostUserBackendReqHandler::set_device_state_fd/check_device_state`) that dispatch incoming messages to trait methods. But the `VhostUserFrontend` trait and `Frontend` struct have **no methods** to *send* these messages. The trait ends with `postcopy_end()` and `remove_mem_region()` — no DEVICE_STATE methods.

This is an upstream gap introduced in vhost v0.11.0 (PR #203) which added backend-side support without corresponding frontend-side API. Task 1 patches the crate to close this gap.

---

<!-- START_SUBCOMPONENT_A (tasks 1-4) -->

<!-- START_TASK_1 -->
### Task 1: Patch vhost crate to add Frontend DEVICE_STATE methods

**Files:**
- Fork: vhost crate at v0.15.0 tag
- Modify (in fork): `vhost/src/vhost_user/frontend.rs`
- Modify: root `Cargo.toml` (add `[patch.crates-io]` section)

**Implementation:**

**Step 1: Fork the vhost crate**

Fork `rust-vmm/vhost` at the `vhost-v0.15.0` tag. Clone locally or push to the project's GitHub org.

**Step 2: Add trait methods to `VhostUserFrontend`** (in `vhost/src/vhost_user/frontend.rs`, after the existing trait methods):

```rust
/// Send SET_DEVICE_STATE_FD to the backend for device state transfer.
/// Returns an optional replacement fd from the backend.
fn set_device_state_fd(
    &mut self,
    direction: VhostTransferStateDirection,
    phase: VhostTransferStatePhase,
    fd: &dyn AsRawFd,
) -> Result<Option<File>>;

/// Send CHECK_DEVICE_STATE to verify the backend completed state transfer.
fn check_device_state(&mut self) -> Result<()>;
```

**Step 3: Implement on `Frontend`** (in the `impl VhostUserFrontend for Frontend` block):

Follow the pattern of existing methods like `add_mem_region()` (sends body + fd, receives reply) and `set_backend_request_fd()` (sends fd, receives ack). The `Frontend` struct has access to its private `main_sock: Endpoint<VhostUserMsgHeader<FrontendReq>>` for message send/receive:

```rust
fn set_device_state_fd(
    &mut self,
    direction: VhostTransferStateDirection,
    phase: VhostTransferStatePhase,
    fd: &dyn AsRawFd,
) -> Result<Option<File>> {
    let body = VhostUserTransferDeviceState::new(direction, phase);
    // Send request with body + fd attachment (same pattern as add_mem_region)
    let hdr = self.main_sock.send_request_with_body(
        FrontendReq::SET_DEVICE_STATE_FD,
        &body,
        Some(&[fd.as_raw_fd()]),
    )?;
    // Receive reply: VhostUserU64 + optional fd
    // Note: verify exact return type against v0.15 source. May be a 2-tuple
    // (T, Option<Vec<File>>) rather than 3-tuple.
    let (body, reply_fds) = self.main_sock
        .recv_reply_with_files::<VhostUserU64>(&hdr)?;
    let val = body.value;
    // Bits 0-7: error code (0 = success)
    // Bit 8: "invalid fd" flag (1 = no fd in reply)
    let error_code = val & 0xFF;
    if error_code != 0 {
        return Err(Error::OperationFailedInBackend);
    }
    let has_valid_fd = (val & 0x100) == 0;
    if has_valid_fd {
        Ok(reply_fds.and_then(|fds| fds.into_iter().next()))
    } else {
        Ok(None)
    }
}

fn check_device_state(&mut self) -> Result<()> {
    // Send empty request (no body)
    let hdr = self.main_sock.send_request_header(
        FrontendReq::CHECK_DEVICE_STATE,
        None,
    )?;
    // Receive reply: VhostUserU64 (0 = success, nonzero = error)
    let reply = self.main_sock.recv_reply::<VhostUserU64>(&hdr)?;
    if reply.value != 0 {
        return Err(Error::OperationFailedInBackend);
    }
    Ok(())
}
```

**Note on internal methods:** The exact `Endpoint` method names (`send_request_with_body`, `recv_reply_with_files`, `send_request_header`, `recv_reply`) should be verified against the v0.15.0 source. They follow the patterns used by `add_mem_region`, `get_inflight_fd`, and other existing methods. Since this is a modification to the crate source itself, the implementer has full access to `Endpoint`'s internal API.

**Step 4: Add cargo patch to root `Cargo.toml`:**

Option A (git fork, preferred for CI):
```toml
[patch.crates-io]
vhost = { git = "https://github.com/<org>/vhost", branch = "frontend-device-state" }
```

Option B (local path, for development):
```toml
[patch.crates-io]
vhost = { path = "vendor/vhost" }
```

**Note:** The `[patch.crates-io]` applies only to the main workspace. The `tests/` workspace (Phase 7 test daemon) uses vhost from crates.io without the patch, which is correct — the test daemon only needs backend-side APIs which already exist in the unpatched crate.

**Upstream contribution:** Submit the patch as a PR to `rust-vmm/vhost`. These are standard vhost-user protocol methods (defined in the spec since v0.11) that should exist on the `Frontend`. The PR should include tests using a mock backend.

**Verification:**
```bash
cargo build --features vhost-user
```

**Commit:** `feat: patch vhost crate to add Frontend DEVICE_STATE methods`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Negotiate DEVICE_STATE protocol feature

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/device.rs`

**Implementation:**

Add `VhostUserProtocolFeatures::DEVICE_STATE` to the protocol feature negotiation in `VhostUserDevice::new()`. Phase 3 Task 1 already adds `CONFIGURE_MEM_SLOTS`; extend that set:

```rust
use vhost::vhost_user::{VhostUserProtocolFeatures, VhostTransferStateDirection, VhostTransferStatePhase};

// In VhostUserDevice::new(), protocol feature negotiation:
let desired_protocol_features = VhostUserProtocolFeatures::CONFIG
    | VhostUserProtocolFeatures::MQ
    | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
    | VhostUserProtocolFeatures::DEVICE_STATE;
```

The crate's `VhostUserProtocolFeatures::DEVICE_STATE` is a named constant (bit 19, `0x0008_0000`), so no `from_bits_truncate` hack is needed.

Store whether DEVICE_STATE was actually negotiated (the daemon may not support it) so `save_device_state()`/`load_device_state()` can check before attempting the protocol exchange:

```rust
// Add to VhostUserDevice fields:
device_state_supported: bool,
```

Set during negotiation:
```rust
let negotiated = desired_protocol_features & backend_protocol_features;
// ... existing set_protocol_features() call ...
self.device_state_supported = negotiated.contains(VhostUserProtocolFeatures::DEVICE_STATE);
```

Add accessor:
```rust
pub fn device_state_supported(&self) -> bool {
    self.device_state_supported
}
```

**Verification:**
```bash
cargo build --features vhost-user
```

**Commit:** `feat(devices): negotiate DEVICE_STATE protocol feature`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Implement save_device_state() and load_device_state() helpers

**Verifies:** vhost-user-fs-dax.AC4.1, vhost-user-fs-dax.AC4.2

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/device.rs`

**Implementation:**

Add high-level methods on `VhostUserDevice` that use the patched Frontend's DEVICE_STATE methods:

```rust
use std::io::Read as IoRead;
use std::os::unix::io::{AsRawFd, FromRawFd};

/// Save daemon internal state via DEVICE_STATE protocol.
/// Returns the serialized state blob.
pub fn save_device_state(&self) -> Result<Vec<u8>, Error> {
    if !self.device_state_supported {
        return Err(Error::DeviceStateNotSupported);
    }

    // 1. Create pipe
    let (read_end, write_end) = nix::unistd::pipe()
        .map_err(|e| Error::DeviceStateTransfer(format!("pipe: {e}")))?;

    // Safety: wrap in File for RAII cleanup
    let read_file = unsafe { std::fs::File::from_raw_fd(read_end) };
    let write_file = unsafe { std::fs::File::from_raw_fd(write_end) };

    // 2. Send SET_DEVICE_STATE_FD with SAVE direction + write end
    //    The daemon will write its state to the pipe.
    //    Frontend may return a replacement fd (or None).
    let _reply_fd = self.frontend.lock().unwrap()
        .set_device_state_fd(
            VhostTransferStateDirection::Save,
            VhostTransferStatePhase::Stopped,
            &write_file,
        )
        .map_err(|e| Error::DeviceStateTransfer(format!("set_device_state_fd: {e}")))?;

    // 3. Drop write end so we see EOF after daemon finishes writing
    drop(write_file);

    // 4. Read all data from pipe until EOF
    let mut state = Vec::new();
    let mut read_file = read_file;
    read_file.read_to_end(&mut state)
        .map_err(|e| Error::DeviceStateTransfer(format!("read pipe: {e}")))?;

    // 5. CHECK_DEVICE_STATE confirms transfer completed successfully.
    //    Returns Result<()> — Ok(()) on success, Err on failure.
    self.frontend.lock().unwrap()
        .check_device_state()
        .map_err(|e| Error::DeviceStateTransfer(format!("check_device_state: {e}")))?;

    Ok(state)
}

/// Load daemon internal state via DEVICE_STATE protocol.
pub fn load_device_state(&self, data: &[u8]) -> Result<(), Error> {
    if !self.device_state_supported {
        return Err(Error::DeviceStateNotSupported);
    }

    // 1. Create pipe
    let (read_end, write_end) = nix::unistd::pipe()
        .map_err(|e| Error::DeviceStateTransfer(format!("pipe: {e}")))?;

    // Safety: wrap in File for RAII cleanup
    let read_file = unsafe { std::fs::File::from_raw_fd(read_end) };
    let write_file = unsafe { std::fs::File::from_raw_fd(write_end) };

    // 2. Send SET_DEVICE_STATE_FD with LOAD direction + read end
    //    The daemon will read state from the pipe.
    let _reply_fd = self.frontend.lock().unwrap()
        .set_device_state_fd(
            VhostTransferStateDirection::Load,
            VhostTransferStatePhase::Stopped,
            &read_file,
        )
        .map_err(|e| Error::DeviceStateTransfer(format!("set_device_state_fd: {e}")))?;

    // 3. Drop read end (we only write)
    drop(read_file);

    // 4. Write state data to pipe, then close to signal EOF
    use std::io::Write as IoWrite;
    let mut write_file = write_file;
    write_file.write_all(data)
        .map_err(|e| Error::DeviceStateTransfer(format!("write pipe: {e}")))?;
    drop(write_file); // Signal EOF to daemon

    // 5. CHECK_DEVICE_STATE confirms transfer completed successfully.
    //    Returns Result<()> — Ok(()) on success, Err on failure.
    self.frontend.lock().unwrap()
        .check_device_state()
        .map_err(|e| Error::DeviceStateTransfer(format!("check_device_state: {e}")))?;

    Ok(())
}
```

Add error variants to `VhostUserDevice`'s `Error` enum:
```rust
DeviceStateNotSupported,
DeviceStateTransfer(String),
```

**Note:** The patched Frontend's `check_device_state()` returns `Result<()>` (not a numeric code). Success is `Ok(())`, errors are propagated via `map_err`. No `if result != 0` check is needed.

**Verification:**
```bash
cargo build --features vhost-user
```

**Commit:** `feat(devices): implement save/load_device_state helpers`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Unit tests for DEVICE_STATE helpers

**Verifies:** vhost-user-fs-dax.AC4.1, vhost-user-fs-dax.AC4.2

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/device.rs` (add to tests module)

**Testing:**

Since `save_device_state()` and `load_device_state()` call through the Frontend's DEVICE_STATE methods (which require a connected daemon), unit tests focus on:

- **`test_save_device_state_not_supported`** — construct VhostUserDevice with `device_state_supported = false` (via `new_for_test()`), call `save_device_state()`, assert `Error::DeviceStateNotSupported`.

- **`test_load_device_state_not_supported`** — same setup, call `load_device_state(b"data")`, assert `Error::DeviceStateNotSupported`.

Full protocol roundtrip testing (AC4.1, AC4.2) requires a real vhost-user daemon that supports DEVICE_STATE. This is tested in Phase 8's integration tests where the test daemon is running and the full save/restore cycle exercises the protocol.

**Verification:**
```bash
cargo test -p devices --features vhost-user
```

**Commit:** `test(devices): add DEVICE_STATE helper unit tests`
<!-- END_TASK_4 -->

<!-- END_SUBCOMPONENT_A -->
