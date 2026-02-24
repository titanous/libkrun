# Async Net Loopback Design

## Summary

libkrun's virtio-net device supports multiple backend strategies. Until now, the `net` feature flag has included a proxy backend (`ProxyNetWorker`) that embeds a userspace TCP/IP stack (smoltcp) to route guest network traffic through host connections. This design removes that backend entirely and replaces the only end-to-end test of async networking with a purpose-built loopback backend.

The loopback backend is a minimal implementation of the `AsyncNetBackend` trait that lives in the test workspace. It responds to two packet types — ARP requests (so the guest can discover a MAC address) and ICMP echo requests (ping) — and silently drops everything else. No host networking, no background tasks, and no TCP/IP stack are involved. This backend is wired into an integration test using `VirtioNetBackend::CustomAsyncFactory`, the same pluggable factory mechanism available to library consumers, proving that the existing `AsyncNetWorker` event loop handles a real guest-to-backend packet round-trip correctly. The net effect is a leaner `net` feature flag (`tokio` + `bytes` only) and a test that validates the async path without the operational complexity of the proxy stack.

## Definition of Done
1. `ProxyNetWorker`, `VirtioNetBackend::Proxy`, and proxy-only dependencies (smoltcp, mio proxy code) are deleted from the codebase
2. A loopback `AsyncNetBackend` implementation exists in the test workspace that responds to ARP requests and ICMP echo (ping) requests
3. An integration test uses `VirtioNetBackend::CustomAsyncFactory` with the loopback backend, configures guest networking, and verifies ping works
4. `cargo test` with appropriate features passes

## Acceptance Criteria

### async-net-loopback.AC1: Proxy code and dependencies removed
- **async-net-loopback.AC1.1 Success:** `proxy.rs` deleted, no `pub mod proxy` in `mod.rs`
- **async-net-loopback.AC1.2 Success:** `VirtioNetBackend::Proxy` variant removed from enum
- **async-net-loopback.AC1.3 Success:** `net` feature flag contains only `tokio` and `bytes`
- **async-net-loopback.AC1.4 Success:** `cargo build --features net` succeeds

### async-net-loopback.AC2: Loopback backend responds to ARP and ICMP
- **async-net-loopback.AC2.1 Success:** ARP request for `192.168.100.1` receives ARP reply with correct MAC
- **async-net-loopback.AC2.2 Success:** ICMP echo request to `192.168.100.1` receives echo reply with matching ID/sequence
- **async-net-loopback.AC2.3 Failure:** Non-ARP, non-ICMP packets are silently dropped (no crash, no reply)

### async-net-loopback.AC3: Integration test proves async path
- **async-net-loopback.AC3.1 Success:** Test uses `VirtioNetBackend::CustomAsyncFactory` with `LoopbackFactory`
- **async-net-loopback.AC3.2 Success:** Guest configures `eth0` with `192.168.100.2/24` and pings `192.168.100.1`
- **async-net-loopback.AC3.3 Success:** Guest receives ICMP echo reply within timeout

### async-net-loopback.AC4: All tests pass
- **async-net-loopback.AC4.1 Success:** `make test FEATURE_FLAGS="--features embedded_init"` passes
- **async-net-loopback.AC4.2 Success:** `cargo build --features net` succeeds (no compilation errors from removal)

## Glossary

- **virtio-net**: The virtio standard network device interface. The guest OS communicates with the hypervisor through a pair of virtio queues (TX and RX) using a ring-buffer protocol, rather than emulating a real NIC.
- **`VirtioNetBackend`**: An enum in the devices crate that selects which backend strategy activates when the virtio-net device starts. The variant `CustomAsyncFactory` accepts a user-supplied factory object.
- **`AsyncNetBackend`**: A trait defining the packet-handling contract for async backends. Implementations receive guest packets via `handle_guest_tx()` and send packets to the guest via an `mpsc::Sender<Bytes>`.
- **`AsyncNetBackendFactory`**: A factory trait whose `create()` method is called inside the worker's tokio runtime to initialize the backend.
- **`AsyncNetWorker`**: The tokio-based event loop that bridges virtio queues to an `AsyncNetBackend`. It reads from the TX queue, forwards to the backend, and writes backend replies into the RX queue.
- **`NetBackendHandle`**: The struct returned by a factory's `create()` call. Bundles the backend object with the receiving end of the guest-bound packet channel and an optional wake channel.
- **`ProxyNetWorker`**: The existing proxy backend being deleted. Embeds a smoltcp userspace TCP/IP stack to forward guest TCP/UDP connections to host sockets.
- **smoltcp**: A userspace TCP/IP stack library written in Rust. Used by `ProxyNetWorker` to parse and synthesize network packets without relying on host kernel networking.
- **ARP**: Address Resolution Protocol. The guest uses ARP to map an IP address to a MAC address before sending IP packets. The loopback backend must reply to ARP requests so the guest kernel routes ICMP packets correctly.
- **ICMP echo request / echo reply**: The packet types underlying `ping`. The guest sends an echo request; the backend responds with an echo reply carrying the same identifier and sequence number.
- **`#[host]` / `#[guest]` proc macros**: Attribute macros in the test workspace that split a single test file into host-side and guest-side halves. The host macro builds the VM; the guest macro compiles code that runs inside it.
- **pnet / pnet_base**: Rust libraries for low-level packet construction and parsing (Ethernet, ARP, IP, ICMP headers).
- **`SOCK_DGRAM` + `IPPROTO_ICMP`**: Socket options for opening an unprivileged ICMP socket in the guest. Linux allows ICMP datagram sockets without `CAP_NET_RAW`.
- **`SendBoxFuture`**: A type alias for `Pin<Box<dyn Future + Send>>`. Required for factory creation since the future crosses a thread boundary.

## Architecture

Delete `ProxyNetWorker` and the `VirtioNetBackend::Proxy` variant from the devices crate. Remove proxy-only dependencies (smoltcp, mio, pnet, pnet_base, socket2, tracing) from the `net` feature flag. The `net` feature retains `tokio` and `bytes`, which `AsyncNetWorker` and `AsyncNetBackend` depend on.

A loopback `AsyncNetBackend` implementation lives in the test workspace (`tests/test_cases/src/loopback_net.rs`). It handles two packet types:

- **ARP requests** for the backend's IP (`192.168.100.1`) — replies with the backend's MAC address
- **ICMP echo requests** to `192.168.100.1` — replies with an ICMP echo reply (swapped src/dst, type changed)

All other packets are silently dropped. The backend has no timers, no background tasks, and no host networking. Replies are crafted synchronously in `handle_guest_tx()` and sent to the guest via the `mpsc::Sender<Bytes>` channel.

The factory (`LoopbackFactory`) implements `AsyncNetBackendFactory`. On `create()`, it allocates the mpsc channel, constructs the backend with the sender half, and returns a `NetBackendHandle` with `wake_rx: None`.

A single integration test (`test_net_async_loopback.rs`) uses the `#[host]`/`#[guest]` proc macro split. The host creates a VM with `VirtioNetBackend::CustomAsyncFactory(Box::new(LoopbackFactory::new()))`. The guest configures `eth0` via libc ioctls (IP `192.168.100.2/24`), sends an ICMP echo request to `192.168.100.1` using a raw socket, and asserts a reply arrives.

### Contract: LoopbackFactory

```rust
pub struct LoopbackFactory;

impl AsyncNetBackendFactory for LoopbackFactory {
    fn create(self: Box<Self>) -> SendBoxFuture<'static, io::Result<NetBackendHandle>>;
}
```

### Contract: LoopbackBackend

```rust
pub struct LoopbackBackend {
    to_guest: mpsc::Sender<Bytes>,
}

impl AsyncNetBackend for LoopbackBackend {
    fn handle_guest_tx(&mut self, packet: &[u8]);  // ARP + ICMP handling
    fn poll(&mut self);                              // no-op
    fn on_exit(&mut self);                           // no-op
}
```

## Existing Patterns

The integration test follows the existing `#[host]`/`#[guest]` proc macro pattern used by all other tests in `tests/test_cases/src/`. Guest network configuration (libc ioctls for `eth0`) is taken directly from the existing `test_net_proxy.rs`.

The `AsyncNetBackend` trait and `AsyncNetWorker` already exist and are tested in `async_worker.rs` unit tests. This design adds no new patterns — it exercises the existing async path with a minimal backend.

Packet crafting with pnet follows the same approach used in the current `proxy.rs` (which is being deleted). The test workspace gains pnet as a dependency for this purpose.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: Delete ProxyNetWorker and Proxy Dependencies
**Goal:** Remove all proxy-specific code and dependencies from the devices crate.

**Components:**
- Delete `src/devices/src/virtio/net/proxy.rs`
- Remove `pub mod proxy;` from `src/devices/src/virtio/net/mod.rs`
- Remove `VirtioNetBackend::Proxy` variant and its activation arm from `src/devices/src/virtio/net/device.rs`
- Remove `Proxy` panic arm from `src/devices/src/virtio/net/worker.rs`
- Trim `net` feature in `src/devices/Cargo.toml` to `["tokio", "bytes"]`
- Remove unused dependency entries (smoltcp, mio, pnet, pnet_base, socket2, tracing) from `[dependencies]`
- Delete `tests/test_cases/src/test_net_proxy.rs`
- Update `src/devices/src/virtio/net/CLAUDE.md`

**Dependencies:** None

**Done when:** `cargo build --features net` succeeds with no proxy code remaining. No references to `ProxyNetWorker`, `VirtioNetBackend::Proxy`, or removed dependencies exist in the devices crate.
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: Loopback Backend and Integration Test
**Goal:** Prove `AsyncNetWorker` + `AsyncNetBackend` works end-to-end with a minimal loopback backend.

**Components:**
- `tests/test_cases/src/loopback_net.rs` — `LoopbackFactory` and `LoopbackBackend` implementing `AsyncNetBackendFactory` and `AsyncNetBackend`
- `tests/test_cases/src/test_net_async_loopback.rs` — integration test with `#[host]`/`#[guest]` split
- `tests/test_cases/Cargo.toml` — add `pnet` and `pnet_base` as test workspace dependencies

**Dependencies:** Phase 1 (proxy code removed, `VirtioNetBackend::Proxy` gone)

**Done when:** `make test FEATURE_FLAGS="--features embedded_init"` passes with the new test exercising the async backend path. Guest successfully pings `192.168.100.1` through the loopback backend. Covers `async-net-loopback.AC1.*` and `async-net-loopback.AC2.*`.
<!-- END_PHASE_2 -->

## Additional Considerations

**Guest ICMP socket:** The guest test uses `socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP)` (ICMP datagram socket, no root required) rather than `SOCK_RAW`. This matches what the guest kernel supports without special capabilities.

**No snapshot support:** The loopback backend does not implement `save_snapshot_state`/`restore_snapshot_state` (uses defaults). Snapshot testing of async net backends is out of scope for this design.
