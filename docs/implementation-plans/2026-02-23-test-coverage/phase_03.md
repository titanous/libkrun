# Test Coverage Implementation Plan — Phase 3

**Goal:** Unit-test the `VirtualDevice` packet bridge and fix the ephemeral port exhaustion loop in `ProxyNetWorker`. TCP SYN interception and UDP NAT tests require real localhost connections but run as `#[test]` functions within `proxy.rs`.

**Architecture:** `#[cfg(test)]` module added to `src/devices/src/virtio/net/proxy.rs` (no existing tests). Tests for AC4.1/4.2 construct `VirtualDevice` directly in the test module using the same virtio queue setup pattern as block/net tests. Tests for AC4.3/4.4/4.5 start a localhost listener and call `intercept_new_session` / `handle_udp_datagram` directly on a `ProxyNetWorker` instance. AC4.6 tests the fixed `get_ephemeral_port` by verifying it returns an error after exhausting available ports. The bug fix changes `get_ephemeral_port` from an infinite loop to a bounded loop returning `Result<u16, ProxyError>`.

**Tech Stack:** Rust, `smoltcp` (already used), `vm_memory` crate, `std::net`, `socket2`.

**Scope:** Phase 3 of 8 phases

**Codebase verified:** 2026-02-23

---

## Acceptance Criteria Coverage

This phase implements and tests:

### test-coverage.AC4: Net proxy internals
- **test-coverage.AC4.1 Success:** `VirtualDevice::receive_raw_from_guest` strips the virtio-net header and delivers the raw Ethernet payload
- **test-coverage.AC4.2 Edge:** Packet with zero-length payload after header → backend receives empty slice; no panic
- **test-coverage.AC4.3 Success:** TCP SYN packet → `intercept_new_session` creates a host-side `TcpStream` and a smoltcp twin socket
- **test-coverage.AC4.4 Success:** First UDP datagram to a new endpoint → NAT entry created in `nat_table`
- **test-coverage.AC4.5 Success:** Second UDP datagram to the same endpoint → forwarded via existing NAT entry without creating a duplicate
- **test-coverage.AC4.6 Failure:** All ephemeral ports exhausted → port allocation returns an error (new behavior; current code loops indefinitely)

---

## Codebase Findings (Phase 3 Investigation)

### proxy.rs (`src/devices/src/virtio/net/proxy.rs`)

**`VirtualDevice` struct** (lines 51–56) — private:
```rust
struct VirtualDevice {
    rx_buffer: VecDeque<Bytes>,
    mem: GuestMemoryMmap,
    queues: Vec<Queue>,
    rx_frame_buf: [u8; MAX_BUFFER_SIZE],  // MAX_BUFFER_SIZE = 65562
    tx_frame_buf: [u8; MAX_BUFFER_SIZE],
}
```
Queue indices: `RX_INDEX = 0`, `TX_INDEX = 1` (imported from `crate::virtio::net`).

**`VirtualDevice::receive_raw_from_guest(&mut self) -> Option<Bytes>`** (lines 60–100):
- Pops from `queues[TX_INDEX]` (the virtio TX queue from the guest)
- Reads all descriptors into `rx_frame_buf`
- Strips `size_of::<virtio_net_hdr_v1>()` = 12 bytes of header
- Returns `Some(Bytes)` if `read_count > header_len`, else `None`
- **AC4.2 behavior:** If the descriptor contains only the header (zero payload), `read_count == 12 == header_len`, condition `read_count > header_len` is false → returns `None`. No panic.

**`intercept_new_session(&mut self, data: &[u8]) -> bool`** (line 842):
- Parses Ethernet + IP + TCP from raw bytes
- Checks `tcp.get_flags() == TcpFlags::SYN`
- On SYN: connects a real `TcpStream` to the destination, creates a smoltcp twin socket, adds it to `self.sockets`
- Returns `true` if intercepted

**`handle_udp_datagram`** (line 1175): processes UDP NAT creation and forwarding; inserts into `self.nat_table`.

**`get_ephemeral_port(&mut self) -> u16`** (lines 1142–1173):
- **BUG:** Infinite `loop` with no error path. If all ports 49152–65535 are in use, loops forever.
- Checks `self.sockets.iter().any(...)` for each candidate port.

**`ProxyNetWorker::new(...)` constructor** (lines 274–387) — complex, requires queues, EventFds, interrupt machinery, mem, listeners. Constructing in a test is hard. For AC4.3/4.4/4.5, see construction approach below.

### smoltcp usage in proxy.rs

```rust
use smoltcp::iface::{Config, Interface, SocketSet, SocketHandle};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp::Socket as TcpSocket;
use smoltcp::socket::raw::{PacketBuffer, PacketMetadata, Socket as RawSocket};
use smoltcp::wire::{HardwareAddress, IpEndpoint};
```

---

## Tasks

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Fix `get_ephemeral_port` — bounded loop returning `Result`

**Verifies:** test-coverage.AC4.6 (prerequisite — infinite loop without this fix)

**Files:**
- Modify: `src/devices/src/virtio/net/proxy.rs` — `get_ephemeral_port` function (lines 1142–1173) and all call sites

**Implementation:**

Change the return type from `u16` to `Result<u16, ProxyError>`. Add a `ProxyError` enum (or use an existing error type in the proxy module — check for an existing error type first).

```rust
#[derive(Debug)]
pub(crate) enum ProxyError {
    EphemeralPortsExhausted,
}
```

Rewrite `get_ephemeral_port`:

```rust
fn get_ephemeral_port(&mut self) -> Result<u16, ProxyError> {
    const EPHEMERAL_PORT_MIN: u16 = 49152;
    const EPHEMERAL_PORT_MAX: u16 = 65535;
    let total_ports = (EPHEMERAL_PORT_MAX - EPHEMERAL_PORT_MIN) as u32 + 1;

    for _ in 0..total_ports {
        let candidate = self.next_ephemeral_port;
        self.next_ephemeral_port = self.next_ephemeral_port.wrapping_add(1);
        if self.next_ephemeral_port < EPHEMERAL_PORT_MIN {
            self.next_ephemeral_port = EPHEMERAL_PORT_MIN;
        }

        let is_in_use = self.sockets.iter().any(|(_, socket)| {
            let local_port = match socket {
                smoltcp::socket::Socket::Tcp(s) => s.local_endpoint().map(|ep| ep.port),
                smoltcp::socket::Socket::Udp(s) => Some(s.endpoint().port),
                _ => None,
            };
            local_port == Some(candidate)
        });

        if !is_in_use {
            return Ok(candidate);
        }
    }

    Err(ProxyError::EphemeralPortsExhausted)
}
```

Update all call sites of `get_ephemeral_port` in `proxy.rs` to handle the `Result`. Search for `get_ephemeral_port()` calls and add appropriate error handling (log the error and return early from the calling function).

**Verification:**

Run: `cargo build -p devices`
Expected: Builds without errors. All call sites updated.

**Commit:** `fix(proxy): replace infinite ephemeral port loop with bounded error-returning version`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add `VirtualDevice` header-stripping unit tests (AC4.1–AC4.2)

**Verifies:** test-coverage.AC4.1, test-coverage.AC4.2

**Files:**
- Modify: `src/devices/src/virtio/net/proxy.rs` — add `#[cfg(test)] mod tests { ... }` at the bottom

**Implementation:**

Add a test module. The `VirtualDevice` struct is private but accessible from the same-file test module. Construct it directly:

```rust
fn make_virtual_device(mem: &GuestMemoryMmap, queues: Vec<Queue>) -> VirtualDevice {
    VirtualDevice {
        rx_buffer: std::collections::VecDeque::new(),
        mem: mem.clone(),
        queues,
        rx_frame_buf: [0u8; MAX_BUFFER_SIZE],
        tx_frame_buf: [0u8; MAX_BUFFER_SIZE],
    }
}
```

Queue and descriptor setup: populate `queues[TX_INDEX]` (index 1) with a descriptor containing virtio-net header + payload. Use the same `mem.write_obj` approach as in block/net tests (see `src/devices/src/virtio/block/async_worker.rs` lines 2575–2651 for the exact pattern).

- **AC4.1 — header stripped:** Write a descriptor with `VIRTIO_NET_HDR_SIZE + 5` bytes. Bytes 0..12 = virtio-net header (all zeros), bytes 12..17 = `[0xDE, 0xAD, 0xBE, 0xEF, 0x42]` (recognizable payload). Call `receive_raw_from_guest()`. Assert returns `Some(payload)` where `payload` = `[0xDE, 0xAD, 0xBE, 0xEF, 0x42]`.

- **AC4.2 — header-only (zero payload):** Write a descriptor with exactly `VIRTIO_NET_HDR_SIZE` bytes. Call `receive_raw_from_guest()`. Assert returns `None` (because `read_count == header_len`, condition `read_count > header_len` is false). No panic.

**Note on AC4.2:** The current code returns `None` (not `Some(empty)`) for a header-only packet. The AC says "backend receives empty slice; no panic" — the key invariant being tested is "no panic". The task-implementor should verify whether `None` or `Some(Bytes::new())` is the intended behavior. If the design requires `Some(Bytes::new())`, a one-line code change is needed (`>=` instead of `>`); otherwise test for `None`. The plan assumes `None` matches the current code and both are valid since the caller treats `None` as "no packet".

**Verification:**

Run: `cargo test -p devices net::proxy::tests`
Expected: Both tests pass.

**Commit:** `test(proxy): add VirtualDevice header-stripping unit tests for AC4.1-AC4.2`
<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-5) -->

<!-- START_TASK_3 -->
### Task 3: Add TCP SYN interception test (AC4.3)

**Verifies:** test-coverage.AC4.3

**Files:**
- Modify: `src/devices/src/virtio/net/proxy.rs` — add test to the test module

**Implementation:**

`intercept_new_session` calls `std::net::TcpStream::connect(dest)` and creates a smoltcp socket. This requires a real host-side TCP listener. The test:

1. Start a host listener: `let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap()` and get its port with `listener.local_addr().unwrap().port()`.

2. Construct a minimal `ProxyNetWorker`. The confirmed constructor signature (from `src/devices/src/virtio/net/proxy.rs` lines 274–387) is:
   ```rust
   ProxyNetWorker::new(
       queues: Vec<Queue>,
       queue_evts: Vec<EventFd>,
       interrupt_status: Arc<AtomicUsize>,
       interrupt_evt: EventFd,
       intc: Option<IrqChip>,
       irq_line: Option<u32>,
       mem: GuestMemoryMmap,
       listeners: Vec<(u16, String)>,
   ) -> io::Result<Self>
   ```
   Construct with:
   - `queues`: two `Queue` objects (RX at index 0, TX at index 1) — look at how net tests in `async_worker.rs` build queues
   - `queue_evts`: two `EventFd::new(0).unwrap()` objects
   - `interrupt_status`: `Arc::new(AtomicUsize::new(0))`
   - `interrupt_evt`: `EventFd::new(0).unwrap()`
   - `intc`: `None` (no interrupt controller)
   - `irq_line`: `None`
   - `mem`: a minimal `GuestMemoryMmap` (e.g., 1 MiB region at address 0)
   - `listeners`: `vec![]`

3. Craft a raw Ethernet frame containing a TCP SYN packet from a guest source (e.g., `192.168.1.2:54321`) to `127.0.0.1:[listener_port]`. Construct the frame manually using smoltcp wire types:
   ```rust
   // Use smoltcp::wire::{EthernetFrame, Ipv4Packet, TcpPacket, TcpFlags}
   // to build a valid Ethernet+IP+TCP SYN frame into a byte buffer
   ```
   The task-implementor should look at how `intercept_new_session` parses the packet (it likely uses smoltcp wire types to check the TCP flags) and craft a matching byte buffer.

4. Call `proxy.intercept_new_session(&frame_bytes)`. Assert it returns `true` (packet was intercepted).

5. Assert the `proxy.sockets` set has grown by 1 (a smoltcp twin socket was created). The socket count before and after can be compared.

**Commit:** `test(proxy): add TCP SYN interception test for AC4.3`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Add UDP NAT entry tests (AC4.4–AC4.5)

**Verifies:** test-coverage.AC4.4, test-coverage.AC4.5

**Files:**
- Modify: `src/devices/src/virtio/net/proxy.rs` — add tests to the test module

**Implementation:**

UDP NAT entries are stored in `self.nat_table: HashMap<IpEndpoint, Token>`. `handle_udp_datagram` is the function that creates/looks up these entries.

Construct a `ProxyNetWorker` as in Task 3.

- **AC4.4 — first datagram creates NAT entry:**
  - Record `proxy.nat_table.len()` before (should be 0).
  - Craft a UDP datagram. Look at how `handle_udp_datagram` is called within `ProxyNetWorker::run` to understand what form the input takes. The function signature is `handle_udp_datagram(&mut self, guest_addr: Ipv4Addr, dest_addr: Ipv4Addr, udp_packet: UdpPacket)`. The `UdpPacket` type is `pnet::packet::udp::UdpPacket` (from the `pnet` crate, already a dependency of `devices`). Construct it from a byte buffer:
  ```rust
  use pnet::packet::udp::{MutableUdpPacket, UdpPacket};
  let mut buf = vec![0u8; MutableUdpPacket::minimum_packet_size()];
  let mut udp = MutableUdpPacket::new(&mut buf).unwrap();
  udp.set_source(54321);
  udp.set_destination(/* host UDP port */);
  udp.set_length(MutableUdpPacket::minimum_packet_size() as u16);
  let udp_ref = UdpPacket::new(&buf).unwrap();
  ```
  Call it directly with appropriate arguments. Use a real host UDP listener at `127.0.0.1:0` to receive the forwarded datagram.
  - Assert `proxy.nat_table.len() == 1`.

- **AC4.5 — second datagram to same endpoint reuses NAT entry:**
  - After AC4.4 state, call `handle_udp_datagram` again with the same guest source and destination.
  - Assert `proxy.nat_table.len()` is still 1 (no new entry created).

The task-implementor must read `handle_udp_datagram` (lines 1175+) carefully to understand: when is a NAT entry created vs. reused? The first call inserts into `nat_table`; subsequent calls find the existing entry.

**Commit:** `test(proxy): add UDP NAT entry creation and reuse tests for AC4.4-AC4.5`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Add ephemeral port exhaustion test (AC4.6)

**Verifies:** test-coverage.AC4.6

**Files:**
- Modify: `src/devices/src/virtio/net/proxy.rs` — add test to the test module

**Implementation:**

After the bug fix in Task 1, `get_ephemeral_port` returns `Result<u16, ProxyError>`. The range has 16,384 ports (49152–65535). Testing exhaustion by filling all 16,384 smoltcp sockets is impractical.

**Approach:** The bounded for loop in the fixed code iterates `total_ports` times. For the unit test, verify the error path using one of these approaches (the task-implementor should choose based on what's cleanest in context):

**Option A — Use a ProxyNetWorker and mock the in-use check via a small port range for testing:**
If the constants `EPHEMERAL_PORT_MIN` and `EPHEMERAL_PORT_MAX` can be made configurable via cfg(test) or a test constructor, use 4 ports and add 4 smoltcp sockets to fill them. Verify `get_ephemeral_port()` returns `Err(ProxyError::EphemeralPortsExhausted)`.

**Option B — Extract a testable helper:**
Refactor `get_ephemeral_port` to call a private helper:
```rust
fn is_port_in_use(&self, port: u16) -> bool {
    self.sockets.iter().any(|(_, socket)| { ... })
}
```
Then test `get_ephemeral_port` by constructing a `ProxyNetWorker` and adding smoltcp TCP sockets listening on all ports in a narrow range. Each smoltcp `tcp::Socket` can be configured to listen on a port via `socket.listen(port)` against the local smoltcp interface.

**Option C — Integration-style test:**
Skip the direct unit test and note that AC4.6 is structurally verified by code review (bounded for loop replacing infinite loop). Add a basic smoke test verifying `get_ephemeral_port` returns `Ok(_)` for a fresh `ProxyNetWorker` with no sockets.

The task-implementor should implement at least Option C and attempt Option A or B. The minimum requirement for AC4.6 is: a test that demonstrates `ProxyError::EphemeralPortsExhausted` can be returned (not just that the function has changed signature).

**Verification:**

Run: `cargo test -p devices net::proxy::tests`
Expected: All proxy tests pass.

**Commit:** `test(proxy): add ephemeral port exhaustion test for AC4.6`
<!-- END_TASK_5 -->

<!-- START_TASK_6 -->
### Task 6: Run full devices test suite and verify

**Files:** None

**Step 1: Run the full devices test suite**

Run: `cargo test -p devices`
Expected: All tests pass. Verify presence of:
- `net::proxy::tests::test_receive_raw_strips_header` (AC4.1)
- `net::proxy::tests::test_receive_raw_header_only_no_panic` (AC4.2)
- `net::proxy::tests::test_tcp_syn_interception` (AC4.3)
- `net::proxy::tests::test_udp_nat_entry_created` (AC4.4)
- `net::proxy::tests::test_udp_nat_entry_reused` (AC4.5)
- `net::proxy::tests::test_ephemeral_port_exhaustion` (AC4.6)

**Step 2: Commit if needed**

All test code and bug fix should be committed before proceeding.
<!-- END_TASK_6 -->

<!-- END_SUBCOMPONENT_B -->
