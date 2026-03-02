# Vhost-User-VSock Implementation Plan - Phase 6

**Goal:** End-to-end integration tests proving vhost-user-vsock works with a real VM, including snapshot/restore with counter continuity.

**Architecture:** Three integration test cases follow the existing `test_vhost_user_fs.rs` pattern: `#[host]` blocks configure and run the VM, `#[guest]` blocks execute inside the VM. The echo test verifies basic vsock communication through the proxy. The fd test verifies the pre-provisioned fd path. The snapshot test uses file-based signaling through the root virtiofs (since vsock is entirely owned by the proxy), snapshots/restores, and verifies the proxy's byte counter survived via DEVICE_STATE. A prerequisite task adds socket_path storage to VhostUserVsock for restore-time reconnection (discovered gap from Phase 4).

**Tech Stack:** Rust, integration test framework (#[host]/#[guest] proc macros)

**Scope:** 6 of 6 phases from original design

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-vsock.AC1: VhostUserVsock device activates and connects to backend
- **vhost-user-vsock.AC1.1 Success:** VhostUserVsock connects to backend via socket path, negotiates features, exposes correct guest_cid in config space
- **vhost-user-vsock.AC1.2 Success:** VhostUserVsock connects via pre-provisioned fd (OwnedFd), same feature negotiation and config behavior

### vhost-user-vsock.AC3: Guest can communicate with backend over vsock
- **vhost-user-vsock.AC3.1 Success:** Guest connects to backend via AF_VSOCK, sends data, receives response
- **vhost-user-vsock.AC3.2 Success:** Multiple concurrent vsock connections work simultaneously

### vhost-user-vsock.AC4: Snapshot and restore preserves state
- **vhost-user-vsock.AC4.2 Success:** Restored VM reconnects to fresh backend via new provisioned fd, resumes vsock communication
- **vhost-user-vsock.AC4.3 Success:** Backend internal state (counter) survives snapshot/restore cycle — post-restore value continues from pre-snapshot

### vhost-user-vsock.AC5: Backward compatibility
- **vhost-user-vsock.AC5.1 Success:** Existing userspace vsock (krun_add_vsock) continues to work unchanged
- **vhost-user-vsock.AC5.2 Success:** krun_add_vsock_port API is preserved and functional with userspace vsock
- **vhost-user-vsock.AC5.3 Success:** Guest kernel (libkrunfw) requires no changes — same TSI patches work with both backends

**Note on AC5:** Backward compatibility is verified by existing tests (`vsock-guest-connect`, `tsi-tcp-guest-connect`, etc.) continuing to pass unchanged. No new test code is needed for AC5 — the implementation adds a new code path without modifying the existing userspace vsock path.

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->
<!-- START_TASK_1 -->
### Task 1: Add socket_path storage for restore-time reconnection

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/vsock.rs`

**Implementation:**

Phase 4's `activate_restore()` assumes the device is "already connected" to a new backend, but the standard `handle.restore_snapshot()` flow operates on existing device objects — it doesn't create new ones. For restore to work with socket-path-based VhostUserVsock, the device must reconnect to the (restarted) backend during activate_restore, exactly as VhostUserFs does.

**Add `socket_path` field to `VhostUserVsock` struct** (after `guest_cid`):

```rust
socket_path: Option<String>,
```

**Set socket_path in constructors:**

In `new()` (the socket-path constructor):
```rust
socket_path: Some(path.to_string()),
```

In `from_stream()`:
```rust
socket_path: None,
```

In `new_for_test()`:
```rust
socket_path: None,
```

**Add `socket_path` field to `VhostUserVsockState`:**

```rust
#[cfg(feature = "snapshot")]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct VhostUserVsockState {
    pub guest_cid: u64,
    pub acked_features: u64,
    pub acked_protocol_features: u64,
    pub vring_bases: Vec<u16>,
    pub daemon_state: Vec<u8>,
    pub socket_path: Option<String>,
}
```

**Update `save_backend_state()`** — include socket_path in state:

```rust
let state = VhostUserVsockState {
    guest_cid: self.guest_cid,
    acked_features: self.vhost_user.acked_features(),
    acked_protocol_features: self.vhost_user.acked_protocol_features().bits(),
    vring_bases,
    daemon_state,
    socket_path: self.socket_path.clone(),
};
```

**Update `restore_backend_state()`** — restore socket_path:

```rust
self.socket_path = state.socket_path.clone();
```

**Update `activate_restore()`** — reconnect via socket_path before activating vrings (add before the `activate_vhost_user` call):

```rust
// Reconnect to fresh backend at the saved socket path.
// For fd-based devices (socket_path is None), the orchestrator must
// provide a new connection before restore — not yet supported.
if let Some(ref path) = state.socket_path {
    let stream = std::os::unix::net::UnixStream::connect(path)
        .map_err(|_| ActivateError::BadActivate)?;
    self.vhost_user
        .reconnect_for_restore(
            stream,
            state.acked_features,
            state.acked_protocol_features,
        )
        .map_err(|_| ActivateError::BadActivate)?;
}
```

This matches the VhostUserFs pattern at `src/devices/src/virtio/vhost_user/fs.rs:449-453` — `reconnect_for_restore` takes `(UnixStream, u64, u64)` for the stream, acked features, and acked protocol features.

**Verification:**

```bash
cd /home/titanous/vm-platform/libkrun/.worktrees/vhost-user-vsock && cargo check -p devices --features vhost-user,snapshot
```
Expected: compiles without errors.

```bash
cargo test -p devices --features vhost-user,snapshot -- vhost_user::vsock
```
Expected: existing unit tests still pass. **Important:** The `test_snapshot_state_roundtrip` test from Phase 4 must be updated to include the `socket_path` field in the `VhostUserVsockState` constructor:

```rust
let state = VhostUserVsockState {
    guest_cid: 42,
    acked_features: 0x1234_5678,
    acked_protocol_features: 0x9abc_def0,
    vring_bases: vec![5, 10, 15],
    daemon_state: vec![1, 2, 3, 4, 5],
    socket_path: Some("/tmp/test.sock".to_string()),
};
```

Also update the assertion to check the new field. Similarly update the `test_restore_backend_state_stores_pending` test's state constructor.

**Commit:** `feat(vhost-user): add socket_path for VhostUserVsock restore reconnection`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add counter query port to test-vsock-proxy

**Files:**
- Modify: `tests/test_vsock_proxy/src/main.rs`

**Implementation:**

The echo proxy from Phase 5 echoes data on any port. For snapshot testing, the guest needs to query the proxy's byte counter to verify it survived restore. Add a counter query behavior on port 9998:

When the proxy receives a `VSOCK_OP_RW` packet with `dst_port == 9998`:
- Instead of echoing the data, respond with an 8-byte little-endian u64 containing the current `bytes_echoed` counter value.
- Do NOT increment the counter for counter-query packets.

Add a constant:
```rust
const COUNTER_QUERY_PORT: u32 = 9998;
```

In the TX queue handler, where `VSOCK_OP_RW` packets are processed, add a port check before the echo logic:

```rust
if hdr.dst_port == COUNTER_QUERY_PORT {
    // Respond with 8-byte LE counter value
    let counter_bytes = self.bytes_echoed.to_le_bytes();
    // Build response header (same as echo but with counter_bytes as data)
    // ... enqueue on RX queue with len = 8 and data = counter_bytes
} else {
    // Normal echo: copy data, increment counter
    // ... existing echo logic
}
```

The response header for the counter query uses the same CID/port swapping as echo, but the data payload is the 8-byte counter instead of the echoed data.

**Verification:**

```bash
cd /home/titanous/vm-platform/libkrun/.worktrees/vhost-user-vsock && cd tests && cargo build -p test-vsock-proxy
```
Expected: builds without errors.

**Commit:** `feat(tests): add counter query port to test-vsock-proxy`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Create integration tests and update build infrastructure

**Verifies:** vhost-user-vsock.AC1.1, vhost-user-vsock.AC1.2, vhost-user-vsock.AC3.1, vhost-user-vsock.AC3.2, vhost-user-vsock.AC4.2, vhost-user-vsock.AC4.3

**Files:**
- Create: `tests/test_cases/src/test_vhost_user_vsock.rs`
- Modify: `tests/test_cases/src/lib.rs` (add module declaration and register test cases)
- Modify: `tests/run.sh` (build test-vsock-proxy, export path)

**Implementation:**

**`tests/run.sh`** — Add proxy build after the existing `cargo build -p test-daemon` line:

```bash
cargo build -p test-vsock-proxy
```

Add export after the existing `KRUN_TEST_DAEMON_PATH` export:

```bash
export KRUN_TEST_VSOCK_PROXY_PATH="target/debug/test-vsock-proxy"
```

**`tests/test_cases/src/lib.rs`** — Add module declaration (near existing `test_vhost_user_fs` line):

```rust
mod test_vhost_user_vsock;
use test_vhost_user_vsock::{
    TestVhostUserVsockEcho, TestVhostUserVsockFd, TestVhostUserVsockSnapshot,
};
```

Add test cases to the `test_cases()` vec (after the vhost-user-fs entries):

```rust
TestCase::new("vhost-user-vsock-echo", Box::new(TestVhostUserVsockEcho)),
TestCase::new("vhost-user-vsock-fd", Box::new(TestVhostUserVsockFd)),
TestCase::new("vhost-user-vsock-snapshot", Box::new(TestVhostUserVsockSnapshot)),
```

**`tests/test_cases/src/test_vhost_user_vsock.rs`** — The integration test file. Follow the `test_vhost_user_fs.rs` pattern exactly: shared structs, `#[host]` helpers, `#[guest]` helpers, then per-test `#[host]`/`#[guest]` impl blocks.

Three test structs:
```rust
pub struct TestVhostUserVsockEcho;
pub struct TestVhostUserVsockFd;
pub struct TestVhostUserVsockSnapshot;
```

Constants:
```rust
const ECHO_PORT: u32 = 9999;
const COUNTER_PORT: u32 = 9998;
```

**Host helpers** (`#[host] mod host_helpers`):

`start_proxy(socket_path: &Path, guest_cid: u64) -> Child`:
- Read `KRUN_TEST_VSOCK_PROXY_PATH` env var, fall back to `current_exe().parent().join("test-vsock-proxy")`
- Spawn process with `--socket-path <path> --guest-cid <cid>`
- Poll up to 5 seconds for socket file to appear (same pattern as `start_test_daemon` in `test_vhost_user_fs.rs:22-45`)
- Return child handle

`wait_for_file(path: &Path, timeout_secs: u64)`:
- Poll with 100ms sleep until file exists or timeout
- Panic on timeout

**Guest helpers** (`#[guest] mod guest_helpers`):

`echo_roundtrip(port: u32, data: &[u8]) -> Vec<u8>`:
- Call `vsock_helpers::vsock_connect(port)`
- Write data, read back same length, return buffer

`query_counter() -> u64`:
- Call `vsock_helpers::vsock_connect(COUNTER_PORT)`
- Write a single dummy byte (triggers counter response)
- Read 8 bytes, parse as `u64::from_le_bytes`

`signal_ready()`:
- `std::fs::write("/ready", "")` — creates file on root virtiofs

`wait_for_check()`:
- Poll for `/check` file with 100ms sleep, 30s timeout
- Panic on timeout

---

**Test 1: TestVhostUserVsockEcho** (AC1.1, AC3.1, AC3.2)

Host (`start_vm`):
1. `let socket_path = test_setup.tmp_dir.join("proxy.sock");`
2. `let mut proxy = host_helpers::start_proxy(&socket_path, 3);`
3. Configure VM: `builder.vm_config(1, 512)?`, `setup_fs_builder(&mut builder, &test_setup)?`, `builder.add_vsock_vhost_user(socket_path.to_str().unwrap())?`
4. `let context = builder.build()?;`
5. Run VM in thread: `thread::spawn(move || context.run())`
6. Wait for VM thread to finish
7. Kill proxy

Guest (`in_guest`):
1. AC3.1: `let resp = guest_helpers::echo_roundtrip(ECHO_PORT, b"hello"); assert_eq!(&resp, b"hello");`
2. AC3.2: Open TWO simultaneous connections to ECHO_PORT:
   ```rust
   let mut s1 = crate::vsock_helpers::vsock_connect(ECHO_PORT);
   let mut s2 = crate::vsock_helpers::vsock_connect(ECHO_PORT);
   s1.write_all(b"first").unwrap();
   s2.write_all(b"second").unwrap();
   // Read from both
   let mut buf1 = [0u8; 5];
   let mut buf2 = [0u8; 6];
   s1.read_exact(&mut buf1).unwrap();
   s2.read_exact(&mut buf2).unwrap();
   assert_eq!(&buf1, b"first");
   assert_eq!(&buf2, b"second");
   ```
3. `println!("OK");`

---

**Test 2: TestVhostUserVsockFd** (AC1.2)

Host (`start_vm`):
1. Start proxy on socket_path (same as Test 1)
2. Connect to proxy socket to get a UnixStream:
   ```rust
   let stream = std::os::unix::net::UnixStream::connect(&socket_path)?;
   ```
3. Configure VM with fd: `builder.add_vsock_vhost_user_fd(stream)?`
4. Build and run VM (same pattern as Test 1)

Guest (`in_guest`):
1. Same basic echo test as Test 1 (AC3.1 portion)
2. `println!("OK");`

---

**Test 3: TestVhostUserVsockSnapshot** (AC4.2, AC4.3)

Host (`start_vm`):
1. Start proxy: `let mut proxy = host_helpers::start_proxy(&socket_path, 3);`
2. Configure VM with `add_vsock_vhost_user()`
3. Build, get handle, run in thread:
   ```rust
   let context = builder.build()?;
   let handle = context.vm_handle();
   let vm_thread = thread::spawn(move || context.run());
   ```
4. Wait for guest ready signal: `host_helpers::wait_for_file(&root_dir.join("ready"), 30);`
5. Snapshot: `handle.snapshot(&snap_dir)?;`
6. Kill and restart proxy:
   ```rust
   proxy.kill()?;
   proxy.wait()?;
   std::fs::remove_file(&socket_path).ok();
   proxy = host_helpers::start_proxy(&socket_path, 3);
   ```
7. Restore: `handle.restore_snapshot(&snap_dir)?;`
8. Signal guest: `std::fs::write(root_dir.join("check"), "")?;`
9. Wait for VM, clean up proxy

Guest (`in_guest`):
1. Pre-snapshot echo: send 100 bytes to ECHO_PORT, verify echo
   ```rust
   let data = vec![0xABu8; 100];
   let resp = guest_helpers::echo_roundtrip(ECHO_PORT, &data);
   assert_eq!(resp, data, "pre-snapshot echo failed");
   ```
2. Query counter: `let pre_counter = guest_helpers::query_counter(); assert_eq!(pre_counter, 100);`
3. Signal ready: `guest_helpers::signal_ready();`
4. Wait for restore: `guest_helpers::wait_for_check();`
5. AC4.2 — Post-restore echo works: send 50 more bytes, verify echo
   ```rust
   let data2 = vec![0xCDu8; 50];
   let resp2 = guest_helpers::echo_roundtrip(ECHO_PORT, &data2);
   assert_eq!(resp2, data2, "post-restore echo failed");
   ```
6. AC4.3 — Counter survived: `let post_counter = guest_helpers::query_counter(); assert_eq!(post_counter, 150, "counter should continue from pre-snapshot value");`
7. `println!("OK");`

Note on signaling: This test uses file-based signaling through the root virtiofs (PassthroughFs) instead of the vsock-based READY/CHECK pattern used in test_vhost_user_fs.rs. This is necessary because with vhost-user-vsock, ALL vsock traffic goes through the proxy — `krun_add_vsock_port` is only functional with the userspace vsock backend. The root virtiofs passthrough provides synchronous bidirectional communication: guest writes → host sees file immediately, and vice versa.

**Testing:**

Tests verify each AC:
- AC1.1: TestVhostUserVsockEcho boots VM with `add_vsock_vhost_user(socket_path)` — device activates successfully, guest communicates with proxy
- AC1.2: TestVhostUserVsockFd boots VM with `add_vsock_vhost_user_fd(stream)` — same behavior via pre-provisioned fd
- AC3.1: TestVhostUserVsockEcho guest sends "hello", receives "hello" echo
- AC3.2: TestVhostUserVsockEcho guest opens two simultaneous connections, sends/receives on both
- AC4.2: TestVhostUserVsockSnapshot guest echoes data after restore (proves device reconnected to fresh backend)
- AC4.3: TestVhostUserVsockSnapshot guest queries counter post-restore, verifies it continued from pre-snapshot value
- AC5.1-AC5.3: Existing `vsock-guest-connect` and `tsi-tcp-guest-connect` tests continue to pass (no changes to userspace vsock path)

**Verification:**

```bash
cd /home/titanous/vm-platform/libkrun/.worktrees/vhost-user-vsock && cd tests && cargo test -p test_cases --features guest
```
Expected: unit tests pass (unique name check, compilation).

```bash
cd /home/titanous/vm-platform/libkrun/.worktrees/vhost-user-vsock && make test FEATURE_FLAGS="--features embedded_init" -- --test-case vhost-user-vsock-echo
```
Expected: test passes — guest echoes data through proxy.

```bash
make test FEATURE_FLAGS="--features embedded_init" -- --test-case vhost-user-vsock-fd
```
Expected: test passes — same echo via fd-based connection.

```bash
make test FEATURE_FLAGS="--features embedded_init" -- --test-case vhost-user-vsock-snapshot
```
Expected: test passes — counter survives snapshot/restore.

```bash
make test FEATURE_FLAGS="--features embedded_init" -- --test-case vsock-guest-connect
```
Expected: existing vsock test still passes (backward compat AC5.1, AC5.2).

**Commit:** `test: add vhost-user-vsock integration tests`
<!-- END_TASK_3 -->
<!-- END_SUBCOMPONENT_A -->
