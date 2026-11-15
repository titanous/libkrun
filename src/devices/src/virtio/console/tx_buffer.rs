//! Bounded host-owned console bytes; guest descriptors never reach the writer.
use super::{port_io::PortOutput, ConsoleTxState};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(super) const CAPACITY: usize = 4 * 1024 * 1024;
const FULL_WAIT: Duration = Duration::from_millis(10);
const FINAL_DRAIN: Duration = Duration::from_millis(100);
const CHUNK: usize = 16 * 1024;

enum Item {
    Data(Vec<u8>),
    Loss(u64),
}
#[derive(Default)]
struct Inner {
    chunks: VecDeque<Item>,
    bytes: usize,
    lost: u64,
    finished: Option<Instant>,
    /// The writer was given up (`finish_and_join`); it must not account anything any more.
    abandoned: bool,
}

pub(super) struct Buffer {
    inner: Mutex<Inner>,
    changed: Condvar,
    state: Arc<ConsoleTxState>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}
impl Buffer {
    #[cfg(test)]
    pub fn new(state: Arc<ConsoleTxState>) -> Self {
        Self::with_stop(state, Arc::new(std::sync::atomic::AtomicBool::new(false)))
    }
    pub fn with_stop(state: Arc<ConsoleTxState>, stop: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            changed: Condvar::new(),
            state,
            stop,
        }
    }
    /// Drop incoming bytes, preserving already accepted stream order.
    #[cfg(test)]
    pub fn push(&self, bytes: &[u8]) {
        self.push_until(bytes, Instant::now() + FULL_WAIT);
    }
    fn push_until(&self, bytes: &[u8], deadline: Instant) {
        let mut inner = self.inner.lock().unwrap();
        loop {
            let stopped = self.stop.load(Ordering::Acquire);
            if inner.finished.is_none() && bytes.len() <= CAPACITY.saturating_sub(inner.bytes) {
                break;
            }
            if stopped || inner.finished.is_some() || Instant::now() >= deadline {
                inner.lost += bytes.len() as u64;
                self.state
                    .dropped
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                self.changed.notify_all();
                return;
            }
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(1));
            inner = self.changed.wait_timeout(inner, wait).unwrap().0;
        }
        if inner.lost != 0 {
            let lost = std::mem::take(&mut inner.lost);
            inner.chunks.push_back(Item::Loss(lost));
        }
        inner.bytes += bytes.len();
        inner.chunks.push_back(Item::Data(bytes.to_vec()));
        self.changed.notify_all();
    }
    pub fn finish(&self) {
        self.inner.lock().unwrap().finished = Some(Instant::now());
        self.changed.notify_all();
    }
    /// Count every byte not yet written as dropped (once) and stop all accounting by the writer.
    fn abandon(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.abandoned {
            return;
        }
        inner.abandoned = true;
        let dropped = std::mem::take(&mut inner.bytes) as u64;
        inner.chunks.clear();
        self.state.dropped.fetch_add(dropped, Ordering::Relaxed);
        self.changed.notify_all();
    }
    pub fn write(&self, output: &mut (dyn PortOutput + Send)) {
        loop {
            let (mut bytes, counted) = {
                let mut inner = self.inner.lock().unwrap();
                loop {
                    if inner.finished.is_some_and(|at| at.elapsed() >= FINAL_DRAIN) {
                        let dropped = inner.bytes as u64;
                        inner.chunks.clear();
                        inner.bytes = 0;
                        self.state.dropped.fetch_add(dropped, Ordering::Relaxed);
                        return;
                    }
                    if let Some(item) = inner.chunks.pop_front() {
                        break match item {
                            Item::Data(bytes) => (bytes, true),
                            Item::Loss(lost) => (
                                format!("\n[console: {lost} bytes dropped]\n").into_bytes(),
                                false,
                            ),
                        };
                    }
                    if inner.lost != 0 {
                        let lost = std::mem::take(&mut inner.lost);
                        self.changed.notify_all();
                        break (
                            format!("\n[console: {lost} bytes dropped]\n").into_bytes(),
                            false,
                        );
                    }
                    if inner.finished.is_some() {
                        return;
                    }
                    inner = self.changed.wait(inner).unwrap();
                }
            };
            let mut offset = 0;
            while offset < bytes.len() {
                if self
                    .inner
                    .lock()
                    .unwrap()
                    .finished
                    .is_some_and(|at| at.elapsed() >= FINAL_DRAIN)
                {
                    if counted {
                        self.state
                            .dropped
                            .fetch_add((bytes.len() - offset) as u64, Ordering::Relaxed);
                    }
                    break;
                }
                let end = (offset + CHUNK).min(bytes.len());
                // SAFETY: host allocation is exclusively owned by this writer.
                let slice = unsafe {
                    vm_memory::VolatileSlice::new(bytes[offset..end].as_mut_ptr(), end - offset)
                };
                let result = output.write_volatile(&slice);
                if self.inner.lock().unwrap().abandoned {
                    // Given up while blocked in that write: everything was counted already.
                    return;
                }
                match result {
                    Ok(n) if n > 0 => {
                        offset += n;
                        if counted {
                            let mut inner = self.inner.lock().unwrap();
                            inner.bytes -= n;
                            self.changed.notify_all();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        output.wait_until_writable()
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    _ => {
                        if counted {
                            self.state
                                .dropped
                                .fetch_add((bytes.len() - offset) as u64, Ordering::Relaxed);
                        }
                        break;
                    }
                }
            }
            if counted && offset < bytes.len() {
                let mut inner = self.inner.lock().unwrap();
                inner.bytes -= bytes.len() - offset;
                inner.lost += (bytes.len() - offset) as u64;
                self.changed.notify_all();
            }
        }
    }
}

/// How long the writer may take after `finish` before it is abandoned: the final drain plus
/// time to notice it.
const WRITER_JOIN: Duration = Duration::from_millis(200);

/// After the copier has ended: finish the buffer and wait for its writer thread, at most
/// `WRITER_JOIN`. A writer blocked inside one write on a stalled blocking descriptor (a character
/// device that can only be shared through the caller's description, `port_io`) cannot see the
/// drain deadline; it is abandoned: its unwritten bytes are counted as dropped once and the
/// thread, which writes only from its own copies and never guest memory, ends with the process.
/// Returns false when the writer panicked or was abandoned.
pub(super) fn finish_and_join(buffer: &Buffer, writer: std::thread::JoinHandle<()>) -> bool {
    buffer.finish();
    let deadline = Instant::now() + WRITER_JOIN;
    while !writer.is_finished() {
        if Instant::now() >= deadline {
            buffer.abandon();
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    writer.join().is_ok()
}

pub(super) struct BufferedOutput(pub Arc<Buffer>);
impl PortOutput for BufferedOutput {
    fn write_volatile(&mut self, source: &vm_memory::VolatileSlice) -> std::io::Result<usize> {
        // Copy in bounded chunks even if a malicious descriptor spans all guest RAM.
        let deadline = Instant::now() + FULL_WAIT;
        let mut offset = 0;
        while offset < source.len() {
            let count = CHUNK.min(source.len() - offset);
            let mut bytes = vec![0; count];
            source
                .offset(offset)
                .map_err(std::io::Error::other)?
                .copy_to(&mut bytes);
            self.0.push_until(&bytes, deadline);
            offset += count;
        }
        Ok(source.len())
    }
    fn wait_until_writable(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Sink(Vec<u8>);
    impl PortOutput for Sink {
        fn write_volatile(&mut self, bytes: &vm_memory::VolatileSlice) -> std::io::Result<usize> {
            let mut copy = vec![0; bytes.len()];
            bytes.copy_to(&mut copy);
            self.0.extend(copy);
            Ok(bytes.len())
        }
        fn wait_until_writable(&self) {}
    }
    #[test]
    fn below_bound_is_lossless_and_overflow_has_marker() {
        let state = Arc::new(ConsoleTxState::default());
        let buffer = Buffer::new(state.clone());
        buffer.push(&vec![b'a'; CAPACITY]);
        buffer.push(b"dropped");
        buffer.finish();
        let mut sink = Sink(Vec::new());
        buffer.write(&mut sink);
        assert_eq!(&sink.0[..CAPACITY], vec![b'a'; CAPACITY]);
        assert_eq!(&sink.0[CAPACITY..], b"\n[console: 7 bytes dropped]\n");
        assert_eq!(state.dropped_bytes(), 7);
        assert_eq!(buffer.inner.lock().unwrap().bytes, 0);
    }
    struct Stalled;
    impl PortOutput for Stalled {
        fn write_volatile(&mut self, _: &vm_memory::VolatileSlice) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
        fn wait_until_writable(&self) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    /// A blocking write that does not return until released (a stalled character device shared
    /// through the caller's blocking description).
    struct BlockedWrite(std::sync::mpsc::Receiver<()>);
    impl PortOutput for BlockedWrite {
        fn write_volatile(&mut self, bytes: &vm_memory::VolatileSlice) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(bytes.len())
        }
        fn wait_until_writable(&self) {}
    }
    #[test]
    fn a_writer_blocked_in_a_write_cannot_hold_the_vmm_exit() {
        let state = Arc::new(ConsoleTxState::default());
        let buffer = Arc::new(Buffer::new(state.clone()));
        let (release, blocked) = std::sync::mpsc::channel();
        let writer_buffer = buffer.clone();
        let writer = std::thread::spawn(move || writer_buffer.write(&mut BlockedWrite(blocked)));
        buffer.push(&[7; 1000]);
        std::thread::sleep(Duration::from_millis(20));
        let (done, joined) = std::sync::mpsc::channel();
        let joiner_buffer = buffer.clone();
        std::thread::spawn(move || {
            let _ = done.send(finish_and_join(&joiner_buffer, writer));
        });
        let began = Instant::now();
        let finished = joined.recv_timeout(Duration::from_secs(2));
        assert!(
            finished.is_ok(),
            "the join waited for a write that never returns"
        );
        assert!(began.elapsed() < Duration::from_millis(500));
        // The unwritten bytes are counted once, also after the stuck write finally returns.
        assert_eq!(state.dropped_bytes(), 1000);
        let _ = release.send(());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(state.dropped_bytes(), 1000);
    }
    #[test]
    fn reset_finishes_with_no_bytes_to_inherit_and_counts_once() {
        let state = Arc::new(ConsoleTxState::default());
        let buffer = Buffer::new(state.clone());
        buffer.push(&[7; 1000]);
        buffer.finish();
        buffer.write(&mut Stalled);
        assert_eq!(state.dropped_bytes(), 1000);
        assert_eq!(buffer.inner.lock().unwrap().bytes, 0);
        assert!(buffer.inner.lock().unwrap().chunks.is_empty());
    }
    #[test]
    fn pending_loss_is_reported_before_accepting_later_bytes() {
        let state = Arc::new(ConsoleTxState::default());
        let buffer = Buffer::new(state.clone());
        {
            let mut inner = buffer.inner.lock().unwrap();
            inner.lost = 7;
        }
        buffer.push(b"later");
        let inner = buffer.inner.lock().unwrap();
        assert!(matches!(inner.chunks.front(), Some(Item::Loss(7))));
        assert!(matches!(inner.chunks.back(), Some(Item::Data(bytes)) if bytes == b"later"));
        assert_eq!(inner.lost, 0);
        assert_eq!(state.dropped_bytes(), 0);
    }
    #[test]
    fn progressing_reader_does_not_extend_guest_write_budget() {
        let state = Arc::new(ConsoleTxState::default());
        let buffer = Arc::new(Buffer::new(state));
        buffer.push(&vec![1; CAPACITY]);
        let progress = buffer.clone();
        let reader = std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_millis(100);
            while Instant::now() < until {
                let inner = progress.inner.lock().unwrap();
                progress.changed.notify_all();
                drop(inner);
                std::thread::yield_now();
            }
        });
        let began = Instant::now();
        buffer.push(&vec![2; CHUNK]);
        let elapsed = began.elapsed();
        reader.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(50),
            "guest write waited {elapsed:?}"
        );
    }

    #[test]
    fn loss_does_not_refuse_input_when_room_is_available() {
        let state = Arc::new(ConsoleTxState::default());
        let buffer = Buffer::new(state);
        buffer.inner.lock().unwrap().lost = 7;
        buffer.push(b"later");
        assert!(buffer
            .inner
            .lock()
            .unwrap()
            .chunks
            .iter()
            .any(|chunk| matches!(chunk, Item::Data(bytes) if bytes == b"later")));
    }

    #[test]
    fn finish_releases_a_blocked_guest_write_without_accepting_more() {
        let buffer = Arc::new(Buffer::new(Arc::new(ConsoleTxState::default())));
        buffer.push(&vec![1; CAPACITY]);
        buffer.finish();
        let began = Instant::now();
        buffer.push_until(b"stop", Instant::now() + Duration::from_secs(1));
        assert!(began.elapsed() < Duration::from_millis(20));
        let mut inner = buffer.inner.lock().unwrap();
        inner.bytes = 0;
        inner.chunks.clear();
        drop(inner);
        buffer.push(b"after-stop");
        assert!(
            buffer.inner.lock().unwrap().chunks.is_empty(),
            "finished buffer accepted guest bytes"
        );
    }

    #[test]
    fn stop_releases_waiting_producer_even_with_a_long_deadline() {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let buffer = Arc::new(Buffer::with_stop(
            Arc::new(ConsoleTxState::default()),
            stop.clone(),
        ));
        buffer.push(&vec![1; CAPACITY]);
        let producer_buffer = buffer.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            producer_buffer.push_until(b"blocked", Instant::now() + Duration::from_secs(1));
            sent.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(5));
        let began = Instant::now();
        stop.store(true, Ordering::Release);
        assert!(received.recv_timeout(Duration::from_millis(50)).is_ok());
        producer.join().unwrap();
        assert!(began.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn incoming_overflow_is_bounded_and_counted_once() {
        let state = Arc::new(ConsoleTxState::default());
        let buffer = Buffer::new(state.clone());
        buffer.push(&vec![1; CAPACITY]);
        buffer.push(&[2; 17]);
        buffer.push(&[3; 19]);
        let inner = buffer.inner.lock().unwrap();
        assert_eq!(inner.bytes, CAPACITY);
        assert_eq!(inner.lost, 36);
        assert_eq!(state.dropped_bytes(), 36);
        assert!(matches!(inner.chunks.front(), Some(Item::Data(bytes)) if bytes[0] == 1));
    }
}
