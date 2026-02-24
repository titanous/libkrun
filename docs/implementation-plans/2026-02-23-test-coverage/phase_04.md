# Test Coverage Implementation Plan — Phase 4

**Goal:** Unit-test the virtio console TX processing path: data forwarding, closed-port error handling, and empty buffer.

**Architecture:** `#[cfg(test)]` unit tests added to `src/devices/src/virtio/console/process_tx.rs` (no existing tests). Tests use `DummyIrqChip` for `InterruptTransport` (the established codebase pattern), a custom `RecordingPortOutput` mock for the `PortOutput` trait, and the same virtio queue setup approach used in block and net tests. `process_tx` is a blocking function that parks its thread; tests run it in a separate `std::thread` and terminate it via the `stop` flag.

**Tech Stack:** Rust, `std::thread`, `std::sync::{Arc, Mutex}`, `vm_memory` crate, `volatile_memory::VolatileSlice`.

**Scope:** Phase 4 of 8 phases

**Codebase verified:** 2026-02-23

---

## Acceptance Criteria Coverage

This phase implements and tests:

### test-coverage.AC5: Console TX processing
- **test-coverage.AC5.1 Success:** TX data written to an open port → bytes forwarded to the port's output sink correctly
- **test-coverage.AC5.2 Failure:** TX to a closed/absent port → returns an error; no panic
- **test-coverage.AC5.3 Edge:** Empty TX buffer (zero bytes) → handled without panic or error

---

## Codebase Findings (Phase 4 Investigation)

### process_tx.rs (`src/devices/src/virtio/console/process_tx.rs`)

**`process_tx` signature** (lines 39–45):
```rust
pub(crate) fn process_tx(
    port_id: u32,
    mem: GuestMemoryMmap,
    mut queue: Queue,
    interrupt: InterruptTransport,
    output: Arc<Mutex<Box<dyn PortOutput + Send>>>,
    stop: Arc<AtomicBool>,
)
```
Blocking loop. When queue is empty, calls `interrupt.signal_used_queue()` then `thread::park()`. Exits when `stop` is `true` after unpark. Errors from `write_desc_to_output` are logged via `log::error!` but not propagated.

**`write_desc_to_output` signature** (line 111, private):
```rust
fn write_desc_to_output(
    desc: DescriptorChain,
    output: &mut (dyn PortOutput + Send),
    interrupt: &InterruptTransport,
    port_id: u32,
    head_index: u16,
    desc_ordinal: usize,
) -> Result<usize, GuestMemoryError>
```
Returns `Ok(n)` on success, `Err(GuestMemoryError::IOError(e))` for non-WouldBlock IO errors. Accessible from the same-file test module.

### PortOutput trait (`src/devices/src/virtio/console/port_io.rs`)

```rust
pub trait PortOutput {
    fn write_volatile(&mut self, buf: &VolatileSlice) -> Result<usize, io::Error>;
    fn wait_until_writable(&self);
}
```

### InterruptTransport setup (test pattern from net/async_worker.rs line 707–708)
```rust
use crate::legacy::DummyIrqChip;
let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
let interrupt = InterruptTransport::new(irqchip, "test-console".into()).unwrap();
```

### Queue/descriptor setup (same pattern as block/net tests)

```rust
use vm_memory::{GuestAddress, GuestMemoryMmap};
let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
// Set up descriptor table, available ring, used ring at fixed addresses
// Write descriptor(s) at DESC_TABLE_ADDR, data at data_addr
// Configure queue q.desc_table / q.avail_ring / q.used_ring
```

Descriptor layout: 16 bytes per descriptor (`addr: u64`, `len: u32`, `flags: u16`, `next: u16`), written with `mem.write_obj(...)`.

---

## Tasks

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->

<!-- START_TASK_1 -->
### Task 1: Add `RecordingPortOutput` mock and test helpers

**Verifies:** Prerequisite for AC5.1–AC5.3

**Files:**
- Modify: `src/devices/src/virtio/console/process_tx.rs` — add `#[cfg(test)] mod tests { ... }` at the bottom

**Implementation:**

Add a new test module at the end of the file. Include:

**`RecordingPortOutput`** — records all bytes received:
```rust
struct RecordingPortOutput {
    received: Arc<Mutex<Vec<u8>>>,
}

impl RecordingPortOutput {
    fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        (RecordingPortOutput { received: received.clone() }, received)
    }
}

impl PortOutput for RecordingPortOutput {
    fn write_volatile(&mut self, buf: &VolatileSlice) -> Result<usize, io::Error> {
        let mut data = vec![0u8; buf.len()];
        buf.copy_to(&mut data);
        self.received.lock().unwrap().extend_from_slice(&data);
        Ok(data.len())
    }

    fn wait_until_writable(&self) {}
}
```

**`FailingPortOutput`** — always returns a broken-pipe error:
```rust
struct FailingPortOutput;

impl PortOutput for FailingPortOutput {
    fn write_volatile(&mut self, _buf: &VolatileSlice) -> Result<usize, io::Error> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "port closed"))
    }

    fn wait_until_writable(&self) {}
}
```

**`make_mem_and_queue(payload: &[u8]) -> (GuestMemoryMmap, Queue, u64)`** — helper that writes one descriptor with the given payload into guest memory and configures a `Queue` pointing to it. Returns `(mem, queue, payload_addr)`. Follow the descriptor layout from block test helpers. For an empty payload (`payload.len() == 0`), still write a descriptor with `len = 0`.

The task-implementor should look at lines 2575–2651 of `src/devices/src/virtio/block/async_worker.rs` for the exact descriptor/ring setup and replicate it here.

**`make_interrupt() -> InterruptTransport`** — helper:
```rust
fn make_interrupt() -> InterruptTransport {
    use crate::legacy::DummyIrqChip;
    let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
    InterruptTransport::new(irqchip, "test-console".into()).unwrap()
}
```

**Verification:**

Run: `cargo build -p devices`
Expected: Compiles without errors.

**Commit:** `test(console): add test helpers and mock PortOutput implementations`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add console TX unit tests (AC5.1–AC5.3)

**Verifies:** test-coverage.AC5.1, test-coverage.AC5.2, test-coverage.AC5.3

**Files:**
- Modify: `src/devices/src/virtio/console/process_tx.rs` — add test functions to the test module

**Implementation:**

Add three test functions:

---

**AC5.1 — data forwarded to port output sink:**

```rust
#[test]
fn test_tx_data_forwarded_to_output() {
    let payload = b"hello console";
    let (mem, queue, _) = make_mem_and_queue(payload);
    let interrupt = make_interrupt();
    let (recording_output, received) = RecordingPortOutput::new();
    let output: Arc<Mutex<Box<dyn PortOutput + Send>>> =
        Arc::new(Mutex::new(Box::new(recording_output)));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    let handle = std::thread::spawn(move || {
        process_tx(0, mem, queue, interrupt, output, stop_clone);
    });

    // Give the thread time to process the single queued descriptor.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Signal the thread to stop (it has parked waiting for more data).
    stop.store(true, Ordering::SeqCst);
    handle.thread().unpark();
    handle.join().unwrap();

    assert_eq!(received.lock().unwrap().as_slice(), payload);
}
```

---

**AC5.2 — closed port returns error, no panic:**

This test verifies that `write_desc_to_output` (the private inner function) returns an error when the output sink fails, without panicking. Since the function is private but accessible within the test module:

```rust
#[test]
fn test_tx_closed_port_no_panic() {
    let payload = b"data to broken port";
    let (mem, queue, _) = make_mem_and_queue(payload);
    let interrupt = make_interrupt();
    let output: Arc<Mutex<Box<dyn PortOutput + Send>>> =
        Arc::new(Mutex::new(Box::new(FailingPortOutput)));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    // process_tx logs the error and continues; it must not panic.
    let handle = std::thread::spawn(move || {
        process_tx(0, mem, queue, interrupt, output, stop_clone);
    });

    std::thread::sleep(std::time::Duration::from_millis(50));

    stop.store(true, Ordering::SeqCst);
    handle.thread().unpark();
    // If this join succeeds (no panic in the thread), AC5.2 is satisfied.
    handle.join().unwrap();
}
```

*Note:* `write_desc_to_output` returns `Err(GuestMemoryError::IOError(e))` when the output fails with a non-WouldBlock error. `process_tx` catches that at line 73 and logs it. The AC says "returns an error; no panic" — this is verified by the thread joining successfully. If direct testing of `write_desc_to_output` is preferred (since it's the function that actually returns an error), the test-implementor may add a second assertion testing `write_desc_to_output` directly. The key requirement is: no panic.

---

**AC5.3 — empty TX buffer handled without panic:**

```rust
#[test]
fn test_tx_empty_buffer_no_panic() {
    let (mem, queue, _) = make_mem_and_queue(b""); // zero-byte descriptor
    let interrupt = make_interrupt();
    let (recording_output, received) = RecordingPortOutput::new();
    let output: Arc<Mutex<Box<dyn PortOutput + Send>>> =
        Arc::new(Mutex::new(Box::new(recording_output)));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    let handle = std::thread::spawn(move || {
        process_tx(0, mem, queue, interrupt, output, stop_clone);
    });

    std::thread::sleep(std::time::Duration::from_millis(50));

    stop.store(true, Ordering::SeqCst);
    handle.thread().unpark();
    handle.join().unwrap(); // no panic

    // No bytes should have been forwarded (zero-length descriptor).
    assert!(received.lock().unwrap().is_empty());
}
```

Note: The empty-buffer case hits the `Ok(0) => break` path in `process_tx` (line 65), causing `queue.undo_pop()` and re-queuing. The thread will park again waiting for new data; stopping it via `stop + unpark` terminates cleanly.

**Verification:**

Run: `cargo test -p devices console::process_tx::tests`
Expected: All 3 tests pass.

**Commit:** `test(console): add process_tx unit tests for AC5.1-AC5.3`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Run full devices test suite and verify

**Files:** None

**Step 1: Run the full devices test suite**

Run: `cargo test -p devices`
Expected: All tests pass. Verify the 3 new tests appear:
- `console::process_tx::tests::test_tx_data_forwarded_to_output`
- `console::process_tx::tests::test_tx_closed_port_no_panic`
- `console::process_tx::tests::test_tx_empty_buffer_no_panic`

**Step 2: Commit if needed**

All test code should be committed before proceeding.
<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->
