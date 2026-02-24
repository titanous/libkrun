use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{atomic::AtomicU64, OnceLock};
use std::sync::{Arc, Mutex};
use std::{io, thread};

use vm_memory::{GuestMemory, GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion};

use crate::virtio::console::port_io::PortOutput;
use crate::virtio::{DescriptorChain, InterruptTransport, Queue};

static CONSOLE_TX_DIAG_ENABLED: OnceLock<bool> = OnceLock::new();
static CONSOLE_TX_DIAG_MAX_LOGS: OnceLock<u64> = OnceLock::new();
static CONSOLE_TX_DIAG_EMITTED: AtomicU64 = AtomicU64::new(0);
static CONSOLE_TX_DIAG_SEQ: AtomicU64 = AtomicU64::new(1);

const CONSOLE_TX_DIAG_BASELINE_LOGS: u64 = 8;
const CONSOLE_TX_DIAG_SAMPLE_LIMIT: usize = 64 * 1024;

#[derive(Debug)]
struct TxSliceDiagnostics {
    original_len: usize,
    sampled_len: usize,
    nul_bytes: usize,
    control_bytes: usize,
    high_bytes: usize,
    invalid_utf8_bytes: usize,
    elf_markers: usize,
    hash64: u64,
    head_hex: String,
    tail_hex: String,
}

impl TxSliceDiagnostics {
    fn suspicious(&self) -> bool {
        self.nul_bytes > 0 || self.invalid_utf8_bytes > 0 || self.elf_markers > 0
    }
}

pub(crate) fn process_tx(
    port_id: u32,
    mem: GuestMemoryMmap,
    mut queue: Queue,
    interrupt: InterruptTransport,
    output: Arc<Mutex<Box<dyn PortOutput + Send>>>,
    stop: Arc<AtomicBool>,
) {
    loop {
        let Some(head) = pop_head_blocking(&mut queue, &mem, &interrupt, &stop) else {
            return;
        };

        let head_index = head.index;
        let mut bytes_written = 0;

        for (desc_ordinal, desc) in head.into_iter().readable().enumerate() {
            let desc_len = desc.len as usize;
            match write_desc_to_output(
                desc,
                output.lock().unwrap().as_mut(),
                &interrupt,
                port_id,
                head_index,
                desc_ordinal,
            ) {
                Ok(0) => {
                    break;
                }
                Ok(n) => {
                    assert_eq!(n, desc_len);
                    bytes_written += n;
                }
                Err(e) => {
                    log::error!("Failed to write output: {e}");
                }
            }
        }

        if bytes_written == 0 {
            log::trace!("Tx Add used {bytes_written}");
            queue.undo_pop();
        } else {
            log::trace!("Tx add used {bytes_written}");
            if let Err(e) = queue.add_used(&mem, head_index, bytes_written as u32) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }
    }
}

fn pop_head_blocking<'mem>(
    queue: &mut Queue,
    mem: &'mem GuestMemoryMmap,
    interrupt: &InterruptTransport,
    stop: &AtomicBool,
) -> Option<DescriptorChain<'mem>> {
    loop {
        match queue.pop(mem) {
            Some(descriptor) => break Some(descriptor),
            None => {
                interrupt.signal_used_queue();
                thread::park();
                if stop.load(Ordering::Acquire) {
                    break None;
                }
                log::trace!("tx unparked, queue len {}", queue.len(mem))
            }
        }
    }
}

fn write_desc_to_output(
    desc: DescriptorChain,
    output: &mut (dyn PortOutput + Send),
    interrupt: &InterruptTransport,
    port_id: u32,
    head_index: u16,
    desc_ordinal: usize,
) -> Result<usize, GuestMemoryError> {
    desc.mem
        .try_access(desc.len as usize, desc.addr, |_, len, addr, region| {
            let src = region.get_slice(addr, len).unwrap();
            let diagnostics = if console_tx_diag_enabled() {
                Some(sample_tx_slice_diagnostics(len, &src))
            } else {
                None
            };

            loop {
                log::trace!("Tx {src:?}, write_volatile {len} bytes");
                match output.write_volatile(&src) {
                    // try_access seem to handle partial write for us (we will be invoked again with an offset)
                    Ok(n) => {
                        if let Some(diag) = diagnostics.as_ref() {
                            let post_diag = if diag.suspicious() || n != len {
                                Some(sample_tx_slice_diagnostics(len, &src))
                            } else {
                                None
                            };
                            let post_changed = post_diag
                                .as_ref()
                                .map(|post| post.hash64 != diag.hash64)
                                .unwrap_or(false);
                            let suspicious = diag.suspicious() || n != len || post_changed;
                            if should_emit_console_tx_diag(suspicious) {
                                let tx_diag_seq = next_console_tx_diag_seq();
                                log::warn!(
                                    "console_tx_diag seq={} port_id={} head_index={} desc_ordinal={} desc_len={} written={} sampled_len={} nul_bytes={} invalid_utf8_bytes={} elf_markers={} control_bytes={} high_bytes={} hash64={:016x} head_hex={} tail_hex={} post_changed={} post_nul_bytes={} post_invalid_utf8_bytes={} post_elf_markers={} post_hash64={:016x}",
                                    tx_diag_seq,
                                    port_id,
                                    head_index,
                                    desc_ordinal,
                                    diag.original_len,
                                    n,
                                    diag.sampled_len,
                                    diag.nul_bytes,
                                    diag.invalid_utf8_bytes,
                                    diag.elf_markers,
                                    diag.control_bytes,
                                    diag.high_bytes,
                                    diag.hash64,
                                    diag.head_hex,
                                    diag.tail_hex,
                                    post_changed,
                                    post_diag.as_ref().map(|post| post.nul_bytes).unwrap_or(0),
                                    post_diag
                                        .as_ref()
                                        .map(|post| post.invalid_utf8_bytes)
                                        .unwrap_or(0),
                                    post_diag.as_ref().map(|post| post.elf_markers).unwrap_or(0),
                                    post_diag.as_ref().map(|post| post.hash64).unwrap_or(0),
                                );
                            }
                        }
                        break Ok(n);
                    }
                    // We can't return an error otherwise we would not know how many bytes were processed before WouldBlock
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        log::trace!("Tx wait for output (would block)");
                        interrupt.signal_used_queue();
                        output.wait_until_writable();
                    }
                    Err(e) => break Err(GuestMemoryError::IOError(e)),
                }
            }
        })
}

fn next_console_tx_diag_seq() -> u64 {
    CONSOLE_TX_DIAG_SEQ.fetch_add(1, Ordering::Relaxed)
}

fn console_tx_diag_enabled() -> bool {
    *CONSOLE_TX_DIAG_ENABLED.get_or_init(|| {
        std::env::var("WINDSHEAR_VIRTIO_CONSOLE_TX_DIAGNOSTICS")
            .ok()
            .map(|raw| {
                matches!(
                    raw.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

fn console_tx_diag_max_logs() -> u64 {
    *CONSOLE_TX_DIAG_MAX_LOGS.get_or_init(|| {
        std::env::var("WINDSHEAR_VIRTIO_CONSOLE_TX_DIAGNOSTICS_MAX_LOGS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(200)
    })
}

fn should_emit_console_tx_diag(suspicious: bool) -> bool {
    if !console_tx_diag_enabled() {
        return false;
    }

    let emitted = CONSOLE_TX_DIAG_EMITTED.load(Ordering::Relaxed);
    if emitted >= console_tx_diag_max_logs() {
        return false;
    }

    if !suspicious && emitted >= CONSOLE_TX_DIAG_BASELINE_LOGS {
        return false;
    }

    CONSOLE_TX_DIAG_EMITTED
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            (count < console_tx_diag_max_logs()).then_some(count + 1)
        })
        .is_ok()
}

fn sample_tx_slice_diagnostics(len: usize, src: &vm_memory::VolatileSlice) -> TxSliceDiagnostics {
    let sample_len = len.min(CONSOLE_TX_DIAG_SAMPLE_LIMIT);
    let mut sample = vec![0_u8; sample_len];
    let copied = src.copy_to(&mut sample);
    sample.truncate(copied);

    let nul_bytes = sample.iter().filter(|byte| **byte == 0).count();
    let control_bytes = sample
        .iter()
        .filter(|byte| **byte < 0x20 && !matches!(**byte, b'\n' | b'\r' | b'\t'))
        .count();
    let high_bytes = sample.iter().filter(|byte| **byte >= 0x80).count();
    let elf_markers = sample.windows(3).filter(|window| *window == b"ELF").count();

    TxSliceDiagnostics {
        original_len: len,
        sampled_len: sample.len(),
        nul_bytes,
        control_bytes,
        high_bytes,
        invalid_utf8_bytes: invalid_utf8_byte_count(&sample),
        elf_markers,
        hash64: fnv1a64(&sample),
        head_hex: hex_prefix(&sample, 16),
        tail_hex: hex_suffix(&sample, 16),
    }
}

fn invalid_utf8_byte_count(bytes: &[u8]) -> usize {
    let mut invalid = 0_usize;
    let mut offset = 0_usize;

    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(_) => break,
            Err(err) => {
                let valid = err.valid_up_to();
                offset = offset.saturating_add(valid);
                match err.error_len() {
                    Some(error_len) => {
                        invalid = invalid.saturating_add(error_len);
                        offset = offset.saturating_add(error_len);
                    }
                    None => {
                        invalid = invalid.saturating_add(bytes.len().saturating_sub(offset));
                        break;
                    }
                }
            }
        }
    }

    invalid
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3_u64);
    }
    hash
}

fn hex_prefix(bytes: &[u8], count: usize) -> String {
    bytes
        .iter()
        .take(count)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn hex_suffix(bytes: &[u8], count: usize) -> String {
    let start = bytes.len().saturating_sub(count);
    bytes[start..]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::queue::Descriptor;
    use std::sync::atomic::Ordering;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    // Test memory layout constants for virtio queue
    const TEST_QUEUE_SIZE: u16 = 16;
    const DESC_TABLE_ADDR: u64 = 0x0000; // 16*16=256 bytes, 16-byte aligned
    const AVAIL_RING_ADDR: u64 = 0x0100; // 4 + 2*16 + 2 = 38 bytes, 2-byte aligned
    const USED_RING_ADDR: u64 = 0x0200; // 4 + 8*16 + 2 = 134 bytes, 4-byte aligned
    const DATA_AREA_ADDR: u64 = 0x1000; // payload data

    /// RecordingPortOutput records all bytes written to it
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

    /// FailingPortOutput always returns a broken-pipe error
    struct FailingPortOutput;

    impl PortOutput for FailingPortOutput {
        fn write_volatile(&mut self, _buf: &VolatileSlice) -> Result<usize, io::Error> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "port closed"))
        }

        fn wait_until_writable(&self) {}
    }

    /// Helper to create a guest memory region and queue with one descriptor containing the payload
    fn make_mem_and_queue(payload: &[u8]) -> (GuestMemoryMmap, Queue, u64) {
        // Create guest memory: 128KB
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();

        // Initialize avail and used ring headers
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR)).unwrap(); // flags
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 2)).unwrap(); // idx
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR)).unwrap(); // flags
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2)).unwrap(); // idx

        // Write the payload to guest memory
        let payload_addr = DATA_AREA_ADDR;
        if !payload.is_empty() {
            mem.write_slice(payload, GuestAddress(payload_addr)).unwrap();
        }

        // Write descriptor at index 0
        let desc = Descriptor {
            addr: payload_addr,
            len: payload.len() as u32,
            flags: 0, // readable, no NEXT
            next: 0,
        };
        mem.write_obj(desc, GuestAddress(DESC_TABLE_ADDR)).unwrap();

        // Add descriptor 0 to the avail ring
        let ring_entry_addr = AVAIL_RING_ADDR + 4;
        mem.write_obj(0u16, GuestAddress(ring_entry_addr)).unwrap(); // avail ring[0] = 0

        // Bump avail idx to 1
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2)).unwrap();

        // Create and configure the queue
        let q = {
            let mut q = Queue::new(TEST_QUEUE_SIZE);
            q.size = TEST_QUEUE_SIZE;
            q.ready = true;
            q.desc_table = GuestAddress(DESC_TABLE_ADDR);
            q.avail_ring = GuestAddress(AVAIL_RING_ADDR);
            q.used_ring = GuestAddress(USED_RING_ADDR);
            q
        };

        (mem, q, payload_addr)
    }

    /// Helper to create an InterruptTransport using DummyIrqChip
    fn make_interrupt() -> InterruptTransport {
        use crate::legacy::DummyIrqChip;
        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        InterruptTransport::new(irqchip, "test-console".into()).unwrap()
    }

    use vm_memory::VolatileSlice;

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
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Signal the thread to stop (it has parked waiting for more data).
        stop.store(true, Ordering::SeqCst);
        handle.thread().unpark();

        handle.join().unwrap();

        assert_eq!(received.lock().unwrap().as_slice(), payload);
    }

    #[test]
    fn test_tx_closed_port_no_panic() {
        // Test that write_desc_to_output handles IO errors without panicking.
        // We directly test write_desc_to_output with a descriptor and failing output.
        let payload = b"data to broken port";
        let (mem, _queue, _payload_addr) = make_mem_and_queue(payload);
        let interrupt = make_interrupt();

        // Create a descriptor pointing to our payload
        let desc = {
            let mut q = Queue::new(TEST_QUEUE_SIZE);
            q.size = TEST_QUEUE_SIZE;
            q.ready = true;
            q.desc_table = GuestAddress(DESC_TABLE_ADDR);
            q.avail_ring = GuestAddress(AVAIL_RING_ADDR);
            q.used_ring = GuestAddress(USED_RING_ADDR);
            q.pop(&mem).unwrap() // Get the descriptor we set up
        };

        let mut failing_output = FailingPortOutput;

        // Call write_desc_to_output with the failing output.
        // It should return an error, not panic.
        let result = write_desc_to_output(
            desc,
            &mut failing_output,
            &interrupt,
            0,  // port_id
            0,  // head_index
            0,  // desc_ordinal
        );

        // The result should be an error (broken pipe)
        assert!(result.is_err());
    }

    #[test]
    fn test_tx_empty_buffer_no_panic() {
        // Test with an empty queue (no descriptors available).
        // This tests that process_tx handles the empty queue case without panicking.
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();

        // Initialize avail and used ring headers with empty queue
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR)).unwrap(); // flags
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 2)).unwrap(); // idx (empty)
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR)).unwrap(); // flags
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2)).unwrap(); // idx

        // Create and configure the queue (empty, no descriptors)
        let queue = {
            let mut q = Queue::new(TEST_QUEUE_SIZE);
            q.size = TEST_QUEUE_SIZE;
            q.ready = true;
            q.desc_table = GuestAddress(DESC_TABLE_ADDR);
            q.avail_ring = GuestAddress(AVAIL_RING_ADDR);
            q.used_ring = GuestAddress(USED_RING_ADDR);
            q
        };

        let interrupt = make_interrupt();
        let (recording_output, received) = RecordingPortOutput::new();
        let output: Arc<Mutex<Box<dyn PortOutput + Send>>> =
            Arc::new(Mutex::new(Box::new(recording_output)));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let handle = std::thread::spawn(move || {
            process_tx(0, mem, queue, interrupt, output, stop_clone);
        });

        // Give the thread time to enter the parked state (empty queue).
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Signal the thread to stop (it's parked waiting for data in an empty queue).
        stop.store(true, Ordering::SeqCst);
        handle.thread().unpark();
        handle.join().unwrap(); // no panic

        // No bytes should have been forwarded (empty queue).
        assert!(received.lock().unwrap().is_empty());
    }
}
