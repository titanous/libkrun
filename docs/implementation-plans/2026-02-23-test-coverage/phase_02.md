# Test Coverage Implementation Plan — Phase 2

**Goal:** Unit-test the TX and RX packet paths, snapshot state persistence, and wake/poll timer branches in `AsyncNetWorker`.

**Architecture:** `#[cfg(test)]` unit tests inside `src/devices/src/virtio/net/async_worker.rs`. A new `TrackingNetBackend` mock (following the `TrackingBackend` pattern from `src/devices/src/virtio/block/async_worker.rs`) records `handle_guest_tx` calls, can inject RX packets via the `to_guest_rx` channel, tracks `poll()` call count, and implements `save_snapshot_state` returning `Some(data)`. The `read_tx_packet` private function is tested directly from within the module's test block. Full-worker tests use a tokio runtime (matching the existing `test_quiesce_ack_resume` pattern).

**Tech Stack:** Rust, tokio (already used in this crate), `vm_memory` crate (`GuestMemoryMmap`, `GuestAddress`), `bytes::Bytes`, `std::sync::{Arc, Mutex, atomic::AtomicUsize}`, `std::time::Duration`.

**Scope:** Phase 2 of 8 phases

**Codebase verified:** 2026-02-23

---

## Acceptance Criteria Coverage

This phase implements and tests:

### test-coverage.AC3: Net async worker packet paths
- **test-coverage.AC3.1 Success:** TX empty packet (virtio header only, zero payload) → backend `handle_guest_tx` receives 0-byte slice
- **test-coverage.AC3.2 Success:** TX max-size packet (65535 B payload) → backend receives full payload intact
- **test-coverage.AC3.3 Success:** TX packet split across multiple virtio descriptors → payload reassembled correctly before delivery to backend
- **test-coverage.AC3.4 Edge:** TX descriptor with header smaller than `VIRTIO_NET_HDR_SIZE` → `read_tx_packet` returns `None`; descriptor consumed without panic
- **test-coverage.AC3.5 Success:** Backend injects RX packet via `to_guest_rx` channel → packet appears in guest RX virtio queue
- **test-coverage.AC3.6 Edge:** RX packet arrives with no guest RX buffers available → packet dropped; no panic
- **test-coverage.AC3.7 Success:** Backend implementing `save_snapshot_state` returning `Some(data)` → state bytes survive a quiesce → resync cycle
- **test-coverage.AC3.8 Success:** `NetBackendHandle` with `wake_rx = Some(_)` → `poll()` called when wake signal fires
- **test-coverage.AC3.9 Success:** Backend `poll_delay` returning `Some(d < 1s)` → poll timer fires within `d`

---

## Codebase Findings (Phase 2 Investigation)

### async_worker.rs (`src/devices/src/virtio/net/async_worker.rs`)

- **`read_tx_packet(mem: &GuestMemoryMmap, head: &DescriptorChain, buf: &mut [u8]) -> Option<usize>`** (lines 546–571): private function. Reads the descriptor chain into `buf`, returns `Some(payload_len)` where payload starts after `VIRTIO_NET_HDR_SIZE` bytes. Returns `None` if first descriptor is smaller than the header.
- **`VIRTIO_NET_HDR_SIZE`** (line 34): `std::mem::size_of::<virtio_net_hdr_v1>()` = 12 bytes.
- **`DummyNetBackend`** (lines 676–683): minimal no-op implementation of `AsyncNetBackend`.
- **`DummyNetBackendFactory`** (lines 685–701): creates a `NetBackendHandle` with a `DummyNetBackend`.
- **Existing tests** (lines 704–813): `test_quiesce_ack_resume`, `test_dup_fd`, `test_virtio_net_hdr_size` — use a full tokio-spawned async worker.
- **Quiesce/resync state**: `shared_backend_state: Arc<Mutex<Option<Vec<u8>>>>` receives the output of `save_snapshot_state`; on resync, `restore_snapshot_state` is called with that data.
- **`wake_rx`**: `Option<mpsc::Receiver<()>>` in `NetBackendHandle` — when `Some`, worker listens and calls `poll()` on receipt.
- **`poll_delay()`**: worker sets a timer with this duration and calls `poll()` when it fires.

### async_backend.rs (`src/devices/src/virtio/net/async_backend.rs`)

**`AsyncNetBackend` trait** (lines 70–118):
```
handle_guest_tx(packet: &[u8])
poll(&mut self)
poll_delay() -> Option<Duration>   // default: None
save_snapshot_state() -> Option<Vec<u8>>   // default: None
restore_snapshot_state(&mut self, _data: &[u8])   // default: no-op
on_exit(&mut self)
```

**`NetBackendHandle`** (lines 39–52):
```rust
pub struct NetBackendHandle {
    pub backend: Box<dyn AsyncNetBackend>,
    pub to_guest_rx: mpsc::Receiver<Bytes>,
    pub wake_rx: Option<mpsc::Receiver<()>>,
}
```

### Virtio queue setup pattern (from block/async_worker.rs tests)

The block tests set up descriptor chains by writing raw bytes to a `GuestMemoryMmap` at fixed addresses:
- `DESC_TABLE_ADDR`, `AVAIL_RING_ADDR`, `USED_RING_ADDR` — chosen addresses in the 0x10000–0x20000 range
- Each descriptor: 16 bytes (`addr: u64`, `len: u32`, `flags: u16`, `next: u16`) written via `mem.write_obj(...)`
- Available ring: `flags: u16` at ring base, `idx: u16` at `+2`, then `ring[0]: u16` pointing to head descriptor index
- Queue configured with `q.desc_table = GuestAddress(DESC_TABLE_ADDR)` etc.
- `queue.pop(&mem)` returns the head `DescriptorChain`

Net tests for `read_tx_packet` follow the exact same pattern. The task-implementor should look at the `write_desc` logic in block tests (~lines 2575–2651) and copy the descriptor chain construction helper.

---

## Tasks

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Add `TrackingNetBackend` to the test module in `async_worker.rs`

**Verifies:** Prerequisite for AC3.1–AC3.9

**Files:**
- Modify: `src/devices/src/virtio/net/async_worker.rs` — extend `#[cfg(test)] mod tests { ... }` with the new mock and a factory

**Implementation:**

Add the following to the existing test module (after the `DummyNetBackend` / `DummyNetBackendFactory` definitions, or separately — keeping them intact):

```rust
use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};
use bytes::Bytes;
use std::time::Duration;
use tokio::sync::mpsc;

/// Mock AsyncNetBackend that records TX calls and supports injecting RX packets.
struct TrackingNetBackend {
    /// All payloads delivered via handle_guest_tx, in order.
    tx_received: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Set this before running a test to control poll_delay().
    poll_delay_value: Option<Duration>,
    /// Count of poll() invocations.
    poll_count: Arc<AtomicUsize>,
    /// Data returned by save_snapshot_state (None means no state).
    snapshot_data: Option<Vec<u8>>,
    /// Data received by restore_snapshot_state.
    restored_state: Arc<Mutex<Option<Vec<u8>>>>,
}

impl TrackingNetBackend {
    fn new(snapshot_data: Option<Vec<u8>>) -> (
        Self,
        Arc<Mutex<Vec<Vec<u8>>>>,
        Arc<AtomicUsize>,
        Arc<Mutex<Option<Vec<u8>>>>,
    ) {
        let tx_received = Arc::new(Mutex::new(Vec::new()));
        let poll_count = Arc::new(AtomicUsize::new(0));
        let restored_state = Arc::new(Mutex::new(None));
        let backend = TrackingNetBackend {
            tx_received: tx_received.clone(),
            poll_delay_value: None,
            poll_count: poll_count.clone(),
            snapshot_data,
            restored_state: restored_state.clone(),
        };
        (backend, tx_received, poll_count, restored_state)
    }
}

impl AsyncNetBackend for TrackingNetBackend {
    fn handle_guest_tx(&mut self, packet: &[u8]) {
        self.tx_received.lock().unwrap().push(packet.to_vec());
    }

    fn poll(&mut self) {
        self.poll_count.fetch_add(1, Ordering::SeqCst);
    }

    fn poll_delay(&self) -> Option<Duration> {
        self.poll_delay_value
    }

    fn save_snapshot_state(&self) -> Option<Vec<u8>> {
        self.snapshot_data.clone()
    }

    fn restore_snapshot_state(&mut self, data: &[u8]) {
        *self.restored_state.lock().unwrap() = Some(data.to_vec());
    }

    fn on_exit(&mut self) {}
}
```

Also add a `TrackingNetBackendFactory` that creates a `NetBackendHandle` with a `TrackingNetBackend`:

```rust
struct TrackingNetBackendFactory {
    backend: Option<TrackingNetBackend>,
    rx_sender: mpsc::Sender<Bytes>,
    wake_sender: Option<mpsc::Sender<()>>,
}
```

The factory should implement `AsyncNetBackendFactory`, returning a `NetBackendHandle` constructed with the `TrackingNetBackend`, the `mpsc::Receiver<Bytes>` end, and the `mpsc::Receiver<()>` end for wake. Study the existing `DummyNetBackendFactory` for the exact trait method signatures.

**Verification:**

Run: `cargo build -p devices`
Expected: Compiles without errors. No tests need to pass yet.

**Commit:** `test(net): add TrackingNetBackend mock for async worker unit tests`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add `read_tx_packet` unit tests (AC3.1–AC3.4)

**Verifies:** test-coverage.AC3.1, test-coverage.AC3.2, test-coverage.AC3.3, test-coverage.AC3.4

**Files:**
- Modify: `src/devices/src/virtio/net/async_worker.rs` — add test functions to the test module

**Implementation:**

These tests call `read_tx_packet` directly (accessible within the test module since it is in the same file). Each test builds a fake descriptor chain in guest memory using the same approach as block tests:

1. Allocate a `GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)])`.
2. Pick addresses for: descriptor table (`0x1000`), available ring (`0x2000`), used ring (`0x3000`), and packet data (`0x4000`).
3. Write virtio descriptors at the descriptor table address (`mem.write_obj(desc, GuestAddress(...))`).
4. Write packet payload data at the data address.
5. Set up the available ring (`flags=0`, `idx=1`, `ring[0]=0`).
6. Configure `Queue::new(256)` with `q.desc_table`, `q.avail_ring`, `q.used_ring`.
7. Call `queue.pop(&mem).unwrap()` to get the `DescriptorChain`.
8. Allocate `buf = vec![0u8; 65535 + VIRTIO_NET_HDR_SIZE]`.
9. Call `read_tx_packet(&mem, &chain, &mut buf)`.

Study the helper around lines 2575–2651 of `src/devices/src/virtio/block/async_worker.rs` for the exact descriptor layout (16 bytes per descriptor: 8-byte addr, 4-byte len, 2-byte flags, 2-byte next). Pay attention to the descriptor `flags` field: `0x0` = last, `0x1` = NEXT (chain continues).

Test functions:

- **AC3.1 — empty packet:** Write `VIRTIO_NET_HDR_SIZE` bytes (all zeros) as a single descriptor. Call `read_tx_packet`. Assert result is `Some(0)` (zero-byte payload).

- **AC3.2 — max-size packet:** Write `VIRTIO_NET_HDR_SIZE + 65535` bytes. Fill payload with a recognizable pattern (e.g., `0xAB` repeated). Call `read_tx_packet`. Assert `Some(65535)`. Assert `buf[..65535]` matches the pattern.

- **AC3.3 — multi-descriptor packet:** Split a 100-byte payload across 2 descriptors: first descriptor = `VIRTIO_NET_HDR_SIZE` bytes (header only, `flags = NEXT`, `next = 1`), second descriptor = 100 bytes payload (`flags = 0`). Assert `read_tx_packet` returns `Some(100)` and `buf[..100]` matches the payload. For the multi-descriptor descriptor chain, set descriptor 0's `flags = 0x1` (NEXT bit) and `next = 1`.

- **AC3.4 — truncated header:** Write a descriptor with `len = VIRTIO_NET_HDR_SIZE - 1` bytes (smaller than the header). Assert `read_tx_packet` returns `None`.

**Verification:**

Run: `cargo test -p devices -- net::async_worker::tests`
Expected: All 4 new tests pass.

**Commit:** `test(net): add read_tx_packet unit tests for AC3.1-AC3.4`
<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-5) -->

<!-- START_TASK_3 -->
### Task 3: Add RX path and snapshot state tests (AC3.5–AC3.7)

**Verifies:** test-coverage.AC3.5, test-coverage.AC3.6, test-coverage.AC3.7

**Files:**
- Modify: `src/devices/src/virtio/net/async_worker.rs` — add test functions to the test module

**Implementation:**

These tests spawn a full async worker (like `test_quiesce_ack_resume`) and interact with it via its channels. Study the existing `test_quiesce_ack_resume` test (~line 704) for the pattern of spawning the worker and sending signals.

To spawn the worker, you need:
- `Arc<Mutex<Queue>>` instances (RX queue and TX queue)
- EventFds for queue events
- `interrupt_status`, `interrupt_evt`
- A `NetBackendHandle` constructed from `TrackingNetBackendFactory`
- A `GuestMemoryMmap` for the guest

For RX tests (AC3.5/3.6), the guest RX queue needs to have available RX buffers pre-populated (or not, for AC3.6). To pre-populate RX buffers: write guest-side descriptors into memory and put them in the available ring of the RX queue before starting the worker.

- **AC3.5 — RX delivery:** Pre-populate one RX buffer in the guest RX queue. Start the worker. Send a packet via `rx_sender.send(Bytes::from("hello"))`. Wait briefly. Assert the packet data appears in the used ring of the RX queue (check used ring `idx` incremented and the buffer contains the packet payload with a prepended virtio-net header).

- **AC3.6 — RX drop (no buffers):** Start the worker with an empty RX queue (no guest buffers). Send a packet via `rx_sender.send(Bytes::from("hello"))`. Assert no panic; worker continues running. (The packet is silently dropped.)

- **AC3.7 — snapshot state roundtrip:** Create a `TrackingNetBackend` with `snapshot_data = Some(b"state-bytes".to_vec())`. Spawn the worker. Trigger a quiesce (following the pattern from `test_quiesce_ack_resume`). The worker should call `save_snapshot_state()` and store the result. Then resync: the worker should call `restore_snapshot_state(&data)`. Assert `restored_state` (from the `Arc<Mutex<Option<Vec<u8>>>>` handle) equals `b"state-bytes"`.

**Verification:**

Run: `cargo test -p devices -- net::async_worker::tests`
Expected: All 3 new tests pass.

**Commit:** `test(net): add RX path and snapshot state roundtrip tests for AC3.5-AC3.7`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Add wake_rx and poll_delay tests (AC3.8–AC3.9)

**Verifies:** test-coverage.AC3.8, test-coverage.AC3.9

**Files:**
- Modify: `src/devices/src/virtio/net/async_worker.rs` — add test functions to the test module

**Implementation:**

Both tests spawn a full async worker with a `TrackingNetBackend` that records `poll()` calls via `poll_count: Arc<AtomicUsize>`.

- **AC3.8 — wake_rx triggers poll:** Construct the `NetBackendHandle` with `wake_rx = Some(rx_end)`, keeping `wake_sender = Some(tx_end)`. Start the worker. Send one wake signal: `wake_sender.send(()).await.unwrap()`. Wait a short time (e.g., 100 ms). Assert `poll_count.load(Ordering::SeqCst) >= 1`.

- **AC3.9 — poll_delay timer:** Create a `TrackingNetBackend` with `poll_delay_value = Some(Duration::from_millis(50))`. Start the worker (with `wake_rx = None` to avoid other poll triggers). Wait 200 ms. Assert `poll_count.load(Ordering::SeqCst) >= 1`. (The timer fires within 50 ms, triggering at least one `poll()` call.)

Use `tokio::time::sleep` for waits. Wrap each test with `#[tokio::test]` if the existing quiesce test uses that attribute, otherwise follow whatever runtime setup that test uses.

**Verification:**

Run: `cargo test -p devices -- net::async_worker::tests`
Expected: Both new tests pass.

**Commit:** `test(net): add wake_rx and poll_delay timer tests for AC3.8-AC3.9`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Run full devices test suite and verify

**Files:** None

**Step 1: Run the full devices test suite**

Run: `cargo test -p devices`
Expected: All tests pass. Verify presence of:
- `net::async_worker::tests::test_empty_tx_packet` (or similar — AC3.1)
- `net::async_worker::tests::test_max_size_tx_packet` (AC3.2)
- `net::async_worker::tests::test_multi_descriptor_tx` (AC3.3)
- `net::async_worker::tests::test_truncated_header_returns_none` (AC3.4)
- `net::async_worker::tests::test_rx_packet_delivered` (AC3.5)
- `net::async_worker::tests::test_rx_drop_no_buffers` (AC3.6)
- `net::async_worker::tests::test_snapshot_state_survives_quiesce_resync` (AC3.7)
- `net::async_worker::tests::test_wake_rx_triggers_poll` (AC3.8)
- `net::async_worker::tests::test_poll_delay_timer` (AC3.9)
- Existing tests unchanged: `test_quiesce_ack_resume`, `test_dup_fd`, `test_virtio_net_hdr_size`

**Step 2: Commit if anything was left uncommitted**

Ensure all test code is committed.
<!-- END_TASK_5 -->

<!-- END_SUBCOMPONENT_B -->
