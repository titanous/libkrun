# Async Net Loopback Implementation Plan — Phase 2

**Goal:** Prove `AsyncNetWorker` + `AsyncNetBackend` works end-to-end with a minimal loopback backend that responds to ARP and ICMP echo requests.

**Architecture:** A `LoopbackBackend` implementing `AsyncNetBackend` lives in the test workspace. It receives raw Ethernet frames (the worker strips the virtio-net header before calling `handle_guest_tx()`), parses them with pnet, and sends ARP replies or ICMP echo replies back via the `mpsc::Sender<Bytes>` channel (the worker prepends a zeroed virtio-net header when writing to the RX queue). A `LoopbackFactory` implementing `AsyncNetBackendFactory` allocates the channel and constructs the backend. An integration test wires this into a VM via `VirtioNetBackend::CustomAsyncFactory` and verifies the guest can ping the loopback IP.

**Tech Stack:** Rust, pnet 0.35, pnet_base 0.35, tokio, bytes

**Scope:** 2 phases from original design (phase 2 of 2)

**Codebase verified:** 2026-02-24

---

## Acceptance Criteria Coverage

This phase implements and tests:

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

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->
<!-- START_TASK_1 -->
### Task 1: Add pnet dependencies to test workspace

**Files:**
- Modify: `tests/test_cases/Cargo.toml`

**Step 1: Add pnet and pnet_base dependencies**

In `tests/test_cases/Cargo.toml`, add these three lines to the `[dependencies]` section (after the `tokio` entry):

```toml
bytes = "1"
pnet = "0.35"
pnet_base = "0.35"
```

`bytes` is needed by the loopback backend for `Bytes::from()` (used to send packets to guest via the mpsc channel). `pnet` and `pnet_base` are needed for ARP/ICMP packet construction and parsing.

**Step 2: Verify dependencies resolve**

```bash
cd tests && cargo check -p test_cases --features host
```

Expected: Check succeeds, dependencies resolve.

**Step 3: Commit**

```bash
git add tests/test_cases/Cargo.toml tests/Cargo.lock
git commit -m "add pnet and pnet_base deps to test workspace"
```
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Implement LoopbackFactory and LoopbackBackend

**Verifies:** async-net-loopback.AC2.1, async-net-loopback.AC2.2, async-net-loopback.AC2.3

**Files:**
- Create: `tests/test_cases/src/loopback_net.rs`

**Implementation:**

Create `tests/test_cases/src/loopback_net.rs`. This file is only compiled for the `host` feature (it uses `libkrun` types). Gate the entire file with `#![cfg(feature = "host")]` or wrap contents in a host-gated block.

The file must contain:

#### `LoopbackFactory` struct

Implements `AsyncNetBackendFactory`. The net backend types are imported from the `krun` module alias (same as `krun::VirtioNetBackend`). Note: `SendBoxFuture` is re-exported as `NetSendBoxFuture` in `src/libkrun/src/lib.rs:26`.

```rust
use krun::{
    AsyncNetBackend, AsyncNetBackendFactory, NetBackendHandle, NetSendBoxFuture,
};
```

The `LoopbackFactory` implementation:

```rust
pub struct LoopbackFactory;

impl LoopbackFactory {
    pub fn new() -> Self {
        Self
    }
}

impl AsyncNetBackendFactory for LoopbackFactory {
    fn create(self: Box<Self>) -> NetSendBoxFuture<'static, io::Result<NetBackendHandle>> {
        Box::pin(async {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let backend = LoopbackBackend::new(tx);
            Ok(NetBackendHandle {
                backend: Box::new(backend),
                to_guest_rx: rx,
                wake_rx: None,
            })
        })
    }
}
```

#### `LoopbackBackend` struct

Implements `AsyncNetBackend`. Contains:
- `to_guest: tokio::sync::mpsc::Sender<bytes::Bytes>` — channel for sending reply packets to the guest
- Constants: `BACKEND_MAC` = `MacAddr::new(0x02, 0x00, 0x00, 0x00, 0x00, 0x01)`, `BACKEND_IP` = `Ipv4Addr::new(192, 168, 100, 1)`

The `handle_guest_tx(&mut self, packet: &[u8])` method:

1. Parse packet as `EthernetPacket` using pnet
2. Match on ethertype:
   - **`EtherTypes::Arp`**: Parse payload as `ArpPacket`. If it's an ARP Request for `BACKEND_IP`, build and send an ARP reply:
     - Ethernet header: dst = request src MAC, src = `BACKEND_MAC`, ethertype = Arp
     - ARP payload: operation = Reply, sender_hw = `BACKEND_MAC`, sender_proto = `BACKEND_IP`, target_hw = request sender_hw, target_proto = request sender_proto
     - Send via `self.to_guest.try_send(Bytes::from(reply_buf))`
   - **`EtherTypes::Ipv4`**: Parse payload as `Ipv4Packet`. If protocol is ICMP and dst is `BACKEND_IP`:
     - Parse ICMP payload. If type is `EchoRequest`:
       - Build full reply: Ethernet header (swapped MACs) + IPv4 header (swapped src/dst IPs, same TTL/ID) + ICMP echo reply (type=0, code=0, same ID/sequence/payload, recomputed checksum)
       - For the IPv4 header: copy the entire incoming IPv4 packet, swap src/dst IPs, set TTL to 64, recompute IPv4 checksum
       - For the ICMP portion: set type to EchoReply, set checksum to 0, compute checksum, set it
       - Wrap in Ethernet frame and send via `self.to_guest.try_send()`
   - **Everything else**: silently drop (no action)

3. `poll(&mut self)` — no-op (empty body)
4. `on_exit(&mut self)` — no-op (empty body)

**Key implementation details for packet construction:**

The reply packets must be complete Ethernet frames because the worker's `push_to_rx_queue()` prepends the virtio-net header itself. The backend only sends raw Ethernet frames.

For the ARP reply, the total size is 14 (Ethernet header) + 28 (ARP payload) = 42 bytes.

For the ICMP echo reply, the total size is 14 (Ethernet) + incoming IPv4 total_length. The simplest approach is:
1. Clone the entire incoming Ethernet frame
2. Swap Ethernet src/dst MACs
3. In the IPv4 header: swap src/dst IPs, set TTL to 64, zero the IPv4 checksum and recompute
4. In the ICMP portion: set type to EchoReply (0), zero checksum, recompute ICMP checksum

For IPv4 checksum: pnet provides `pnet::packet::ipv4::checksum(&Ipv4Packet)` to compute the header checksum.

For ICMP checksum: pnet provides `pnet::packet::icmp::checksum(&IcmpPacket)` to compute the checksum.

**Using MutableIpv4Packet:** After cloning the raw bytes starting from the IPv4 portion of the Ethernet frame, use `MutableIpv4Packet::new(&mut ipv4_bytes)` to modify fields in place. Similarly use `MutableIcmpPacket::new(&mut icmp_bytes)` for the ICMP portion.

**Alternatively**, work on the whole cloned frame buffer:
```rust
let mut reply = packet.to_vec();
// Swap Ethernet MACs (bytes 0-5 = dst, 6-11 = src)
// Parse IPv4 starting at offset 14
// Swap IPv4 IPs, recompute checksums
// Modify ICMP type, recompute checksum
self.to_guest.try_send(Bytes::from(reply)).ok();
```

**Step 1: Create the file**

Write `tests/test_cases/src/loopback_net.rs` with the implementation described above.

**Step 2: Register the module**

In `tests/test_cases/src/lib.rs`, add between the `mod mem_block_backend;` line and the test module declarations:

```rust
#[cfg(feature = "host")]
mod loopback_net;
```

Note: `mem_block_backend` is already gated with `#[cfg(feature = "host")]` on line 25-26. Follow the same pattern.

**Step 3: Verify compilation**

```bash
cd tests && cargo check -p test_cases --features host
```

Expected: Compiles successfully.

**Step 4: Commit**

```bash
git add tests/test_cases/src/loopback_net.rs tests/test_cases/src/lib.rs
git commit -m "add LoopbackFactory and LoopbackBackend for async net testing"
```
<!-- END_TASK_2 -->
<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-4) -->
<!-- START_TASK_3 -->
### Task 3: Implement the integration test

**Verifies:** async-net-loopback.AC3.1, async-net-loopback.AC3.2, async-net-loopback.AC3.3

**Files:**
- Create: `tests/test_cases/src/test_net_async_loopback.rs`
- Modify: `tests/test_cases/src/lib.rs` (register test)

**Implementation:**

Create `tests/test_cases/src/test_net_async_loopback.rs` following the exact same pattern as `test_custom_block_backend.rs` and `test_net_proxy.rs`.

#### Host side (`#[host] mod host`)

```rust
use macros::{guest, host};

pub struct TestNetAsyncLoopback;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::loopback_net::LoopbackFactory;
    use crate::{Test, TestSetup};
    use std::thread;

    impl Test for TestNetAsyncLoopback {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;

            // AC3.1: Use CustomAsyncFactory with LoopbackFactory
            builder.add_net_device(
                krun::VirtioNetBackend::CustomAsyncFactory(
                    Box::new(LoopbackFactory::new()),
                ),
                [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee], // Guest MAC
                0, // features
            );

            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());
            vm_thread.join().ok();
            Ok(())
        }
    }
}
```

#### Guest side (`#[guest] mod guest`)

The guest configures `eth0` with IP `192.168.100.2/24` using the same ioctl pattern from `test_net_proxy.rs`, then sends an ICMP echo request to `192.168.100.1` using a `SOCK_DGRAM` + `IPPROTO_ICMP` socket (unprivileged ICMP, no `CAP_NET_RAW` needed).

```rust
#[guest]
mod guest {
    use super::*;
    use crate::Test;

    // Reuse the configure_eth0 function from test_net_proxy.rs
    // (copy the function here since test_net_proxy.rs is deleted in Phase 1)
    fn configure_eth0() {
        // Same ioctl pattern: set IP 192.168.100.2, netmask 255.255.255.0, bring up
        // (copy from test_net_proxy.rs lines 70-118)
    }

    impl Test for TestNetAsyncLoopback {
        fn in_guest(self: Box<Self>) {
            configure_eth0();

            // AC3.2: Open ICMP datagram socket (unprivileged, no CAP_NET_RAW)
            unsafe {
                let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP);
                assert!(sock >= 0, "socket(SOCK_DGRAM, IPPROTO_ICMP) failed");

                // Set receive timeout
                let tv = libc::timeval { tv_sec: 5, tv_usec: 0 };
                let ret = libc::setsockopt(
                    sock,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &tv as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                );
                assert!(ret == 0, "setsockopt SO_RCVTIMEO failed");

                // Build ICMP echo request manually:
                // Type=8 (echo request), Code=0, Checksum, Identifier, Sequence, Payload
                let id: u16 = 0x1234;
                let seq: u16 = 1;
                let payload = b"loopback";

                let mut icmp_buf = vec![0u8; 8 + payload.len()];
                icmp_buf[0] = 8; // type = echo request
                icmp_buf[1] = 0; // code = 0
                // checksum at [2..4], set to 0 first
                icmp_buf[4] = (id >> 8) as u8;
                icmp_buf[5] = (id & 0xff) as u8;
                icmp_buf[6] = (seq >> 8) as u8;
                icmp_buf[7] = (seq & 0xff) as u8;
                icmp_buf[8..].copy_from_slice(payload);

                // Compute checksum
                let cksum = icmp_checksum(&icmp_buf);
                icmp_buf[2] = (cksum >> 8) as u8;
                icmp_buf[3] = (cksum & 0xff) as u8;

                // Send to 192.168.100.1
                let dst = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: 0,
                    sin_addr: libc::in_addr {
                        s_addr: 0xc0a86401_u32.to_be(), // 192.168.100.1
                    },
                    sin_zero: [0; 8],
                };

                let sent = libc::sendto(
                    sock,
                    icmp_buf.as_ptr() as *const libc::c_void,
                    icmp_buf.len(),
                    0,
                    &dst as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                );
                assert!(sent == icmp_buf.len() as isize, "sendto failed");

                // AC3.3: Receive ICMP echo reply
                let mut recv_buf = vec![0u8; 256];
                let received = libc::recvfrom(
                    sock,
                    recv_buf.as_mut_ptr() as *mut libc::c_void,
                    recv_buf.len(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                assert!(received > 0, "recvfrom failed or timed out");

                // The kernel strips the IP header for SOCK_DGRAM sockets,
                // so recv_buf starts with the ICMP header
                assert_eq!(recv_buf[0], 0, "Expected ICMP type 0 (echo reply)");
                assert_eq!(recv_buf[1], 0, "Expected ICMP code 0");
                // Check identifier matches
                let reply_id = ((recv_buf[4] as u16) << 8) | (recv_buf[5] as u16);
                assert_eq!(reply_id, id, "ICMP identifier mismatch");
                // Check sequence matches
                let reply_seq = ((recv_buf[6] as u16) << 8) | (recv_buf[7] as u16);
                assert_eq!(reply_seq, seq, "ICMP sequence mismatch");

                libc::close(sock);
            }

            println!("OK");
        }
    }

    fn icmp_checksum(data: &[u8]) -> u16 {
        let mut sum: u32 = 0;
        let mut i = 0;
        while i + 1 < data.len() {
            sum += ((data[i] as u32) << 8) | (data[i + 1] as u32);
            i += 2;
        }
        if i < data.len() {
            sum += (data[i] as u32) << 8;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !sum as u16
    }
}
```

**Key implementation note on SOCK_DGRAM + IPPROTO_ICMP:**

Linux allows unprivileged ICMP sockets via `socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP)`. The kernel handles the IP header — `sendto()` takes just the ICMP payload, and `recvfrom()` returns just the ICMP payload (no IP header). This avoids needing `CAP_NET_RAW` in the guest.

The kernel also handles the ICMP echo request ID mapping (like it does for port numbers with UDP). The ID field in the sent request may be remapped by the kernel — the reply's ID will match whatever the kernel sent. So for strict assertion, the test should either:
- Trust the kernel to match the socket (just check type=0 and code=0)
- Or read back what the kernel actually sent

The safest approach: assert type=0 (echo reply), code=0, and that we received a non-zero-length response. Optionally check sequence number. The ID check may need to be relaxed if the kernel remaps it.

**Step 1: Create the test file**

Write `tests/test_cases/src/test_net_async_loopback.rs` with the above host/guest implementation. Copy the `configure_eth0()` function from the deleted `test_net_proxy.rs` (it's the same ioctl pattern for 192.168.100.2/24).

**Step 2: Register in lib.rs**

In `tests/test_cases/src/lib.rs`, add the module declaration (near the other test modules):

```rust
mod test_net_async_loopback;
use test_net_async_loopback::TestNetAsyncLoopback;
```

Add to the `test_cases()` vec:

```rust
        TestCase::new("net-async-loopback", Box::new(TestNetAsyncLoopback)),
```

**Step 3: Verify compilation**

```bash
cd tests && cargo check -p test_cases --features host && cargo check -p test_cases --features guest
```

Expected: Both compile successfully.

**Step 4: Commit**

```bash
git add tests/test_cases/src/test_net_async_loopback.rs tests/test_cases/src/lib.rs
git commit -m "add async net loopback integration test"
```
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Run full test suite and verify

**Verifies:** async-net-loopback.AC4.1

**Verification:**

Run:
```bash
make test FEATURE_FLAGS="--features embedded_init"
```

Expected: All tests pass, including the new `net-async-loopback` test. The guest should successfully ping `192.168.100.1` through the loopback backend.

If the new test fails while other tests pass, debug the loopback backend:
1. Check that the backend receives packets (add temporary eprintln in `handle_guest_tx`)
2. Check that ARP replies are being sent (the guest kernel needs ARP resolution before ICMP)
3. Check packet format — the backend receives raw Ethernet frames (no virtio-net header)
4. Check ICMP checksum computation

Also verify:
```bash
cargo build -p devices --features net
```

Expected: Build succeeds (confirming Phase 1 removal didn't break anything).

This task is verification-only — no code changes unless debugging is needed.

**Commit (only if debugging required changes):**

```bash
git add -u
git commit -m "fix: [describe the fix]"
```
<!-- END_TASK_4 -->
<!-- END_SUBCOMPONENT_B -->

<!-- START_TASK_5 -->
### Task 5: Update test workspace CLAUDE.md

**Files:**
- Modify: `tests/CLAUDE.md`

**Step 1: Add new test case to documentation**

In `tests/CLAUDE.md`, in the `## Test Cases` section, add:

```
- `net-async-loopback` - AsyncNetBackend loopback ICMP echo through CustomAsyncFactory
```

**Step 2: Add loopback_net.rs to key files**

In `tests/CLAUDE.md`, in the `## Key Files` section, add:

```
- `test_cases/src/loopback_net.rs` - Loopback AsyncNetBackend and factory for net tests (host-only)
```

**Step 3: Commit**

```bash
git add tests/CLAUDE.md
git commit -m "docs: update tests/CLAUDE.md with loopback net test"
```
<!-- END_TASK_5 -->
