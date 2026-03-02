# Vhost-User-VSock Implementation Plan - Phase 5

**Goal:** Build a minimal vhost-user-vsock backend binary for integration testing — an echo server that receives vsock packets and echoes data back, with DEVICE_STATE support for snapshot testing.

**Architecture:** Follows the `tests/test_daemon/` pattern: a standalone binary implementing `VhostUserBackendMut` from the `vhost-user-backend` crate. The proxy handles 3 queues (RX, TX, Event), parses `virtio_vsock_hdr` from TX queue descriptors, and echoes data back via the RX queue. An internal counter tracks total bytes echoed and survives snapshot/restore via DEVICE_STATE protocol. The `get_config()` method returns a `virtio_vsock_config { guest_cid: u64 }` config space.

**Tech Stack:** Rust, vhost-user-backend 0.21, virtio-queue 0.17, vm-memory 0.18, clap, bincode

**Scope:** 5 of 6 phases from original design

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase is infrastructure for testing. It enables verification of AC1, AC3, and AC4 in Phase 6.

**Verifies: None** (infrastructure phase; operational verification only)

---

<!-- START_TASK_1 -->
### Task 1: Create test_vsock_proxy crate and implement echo backend

**Files:**
- Create: `tests/test_vsock_proxy/Cargo.toml`
- Create: `tests/test_vsock_proxy/src/main.rs`
- Modify: `tests/Cargo.toml` (add to workspace members)

**Implementation:**

**`tests/Cargo.toml`** — add `test_vsock_proxy` to workspace members:

Change:
```toml
members = ["runner", "guest-agent", "macros", "test_cases", "test_daemon"]
```
to:
```toml
members = ["runner", "guest-agent", "macros", "test_cases", "test_daemon", "test_vsock_proxy"]
```

**`tests/test_vsock_proxy/Cargo.toml`** — new file (mirrors test_daemon dependencies):

```toml
[package]
name = "test-vsock-proxy"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "test-vsock-proxy"
path = "src/main.rs"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
log = "0.4"
env_logger = "0.10"
libc = "0.2"
bincode = "1"
serde = { version = "1", features = ["derive"] }
vhost = { version = "0.15", features = ["vhost-user"] }
vhost-user-backend = "0.21"
virtio-queue = "0.17"
vm-memory = { version = "0.18", features = ["backend-mmap", "backend-atomic"] }
vmm-sys-util = ">=0.14"
```

**`tests/test_vsock_proxy/src/main.rs`** — the complete proxy implementation in a single file.

The proxy needs to:

1. **Accept CLI args**: `--socket-path <path>` and `--guest-cid <cid>` (default 3)
2. **Implement VhostUserBackendMut** with:
   - `num_queues()` → 3 (RX, TX, Event)
   - `max_queue_size()` → 256
   - `features()` → `VIRTIO_F_VERSION_1 | VIRTIO_VSOCK_F_DGRAM | VHOST_USER_F_PROTOCOL_FEATURES`
   - `protocol_features()` → CONFIG | MQ | CONFIGURE_MEM_SLOTS | DEVICE_STATE
   - `get_config()` → 8 bytes of guest_cid as little-endian u64
   - `handle_event()` → process TX queue (index 1), parse vsock headers, echo data back to RX queue (index 0)
   - `set_device_state_fd()` / `check_device_state()` → save/load counter via bincode. **Reference implementation:** See `tests/test_daemon/src/backend.rs` for the pipe-based DEVICE_STATE protocol pattern — `set_device_state_fd` stores the pipe fd and direction, `check_device_state` performs the actual read/write on the pipe. For SAVE direction, serialize state via bincode and write to pipe. For LOAD direction, read from pipe and deserialize. The LOAD must be deferred to `check_device_state` (not done in `set_device_state_fd`) to avoid deadlock.
3. **Echo logic**: On the TX queue, read vsock packets. For each VSOCK_OP_RW packet targeting a test port (e.g., port 9999), copy the data payload and enqueue a response packet on the RX queue with src/dst CID/port swapped. Increment the byte counter by the data length.
4. **Connection handling**: For VSOCK_OP_REQUEST (connect), respond with VSOCK_OP_RESPONSE on the RX queue. For VSOCK_OP_SHUTDOWN/RST, respond with VSOCK_OP_RST.
5. **Counter state**: Store a `u64` counter that tracks total bytes echoed. Serialize/deserialize via bincode for DEVICE_STATE.
6. **Main**: parse args, create backend, wrap in `Arc<RwLock<>>`, create `VhostUserDaemon`, call `daemon.serve()`.

Key vsock constants needed in the proxy:
```rust
const VSOCK_OP_INVALID: u16 = 0;
const VSOCK_OP_REQUEST: u16 = 1;
const VSOCK_OP_RESPONSE: u16 = 2;
const VSOCK_OP_RST: u16 = 3;
const VSOCK_OP_SHUTDOWN: u16 = 4;
const VSOCK_OP_RW: u16 = 5;
const VSOCK_OP_CREDIT_UPDATE: u16 = 6;
const VSOCK_OP_CREDIT_REQUEST: u16 = 7;

const VSOCK_TYPE_STREAM: u16 = 1;

const VSOCK_HDR_SIZE: usize = 44;
```

The virtio_vsock_hdr layout (44 bytes):
```rust
#[repr(C, packed)]
struct VsockHdr {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    r#type: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}
```

The echo handler reads from the TX queue (queue index 1). For each descriptor chain:
1. Read the vsock header (44 bytes) from the first readable descriptor
2. If `op == VSOCK_OP_REQUEST`, send VSOCK_OP_RESPONSE on RX queue
3. If `op == VSOCK_OP_RW`, read data from remaining readable descriptors, then enqueue a response on the RX queue with the same data (echo) and swapped CID/port fields
4. If `op == VSOCK_OP_SHUTDOWN`, send VSOCK_OP_RST
5. For all other ops, ignore

To write to the RX queue (queue index 0): construct a response header + data, write to the first available descriptor chain, mark as used, signal via eventfd.

The counter state for DEVICE_STATE:
```rust
#[derive(serde::Serialize, serde::Deserialize)]
struct ProxyState {
    bytes_echoed: u64,
}
```

**Verification:**

```bash
cd /home/titanous/vm-platform/libkrun/.worktrees/vhost-user-vsock && cd tests && cargo build -p test-vsock-proxy
```
Expected: binary builds without errors at `tests/target/debug/test-vsock-proxy`.

```bash
# Verify it starts and creates a socket
./target/debug/test-vsock-proxy --socket-path /tmp/test-proxy.sock --guest-cid 3 &
sleep 1 && ls -la /tmp/test-proxy.sock && kill %1
```
Expected: socket file exists, proxy starts and accepts connections.

**Commit:** `feat(tests): add test-vsock-proxy binary for vhost-user-vsock integration testing`
<!-- END_TASK_1 -->
