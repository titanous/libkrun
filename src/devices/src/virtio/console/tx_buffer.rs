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

#[derive(Default)]
struct Inner {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    lost: u64,
    full_since: Option<Instant>,
    finished: Option<Instant>,
}

pub(super) struct Buffer {
    inner: Mutex<Inner>,
    changed: Condvar,
    state: Arc<ConsoleTxState>,
}
impl Buffer {
    pub fn new(state: Arc<ConsoleTxState>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            changed: Condvar::new(),
            state,
        }
    }
    /// Drop incoming bytes, preserving already accepted stream order.
    pub fn push(&self, bytes: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        while bytes.len() > CAPACITY.saturating_sub(inner.bytes) {
            let since = *inner.full_since.get_or_insert_with(Instant::now);
            if inner.finished.is_some() || since.elapsed() >= FULL_WAIT {
                inner.lost += bytes.len() as u64;
                self.state
                    .dropped
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                self.changed.notify_all();
                return;
            }
            let wait = FULL_WAIT.saturating_sub(since.elapsed());
            inner = self.changed.wait_timeout(inner, wait).unwrap().0;
        }
        inner.bytes += bytes.len();
        inner.chunks.push_back(bytes.to_vec());
        self.changed.notify_all();
    }
    pub fn finish(&self) {
        self.inner.lock().unwrap().finished = Some(Instant::now());
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
                    if let Some(bytes) = inner.chunks.pop_front() {
                        break (bytes, true);
                    }
                    if inner.lost != 0 {
                        let lost = std::mem::take(&mut inner.lost);
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
                match output.write_volatile(&slice) {
                    Ok(n) if n > 0 => {
                        offset += n;
                        if counted {
                            let mut inner = self.inner.lock().unwrap();
                            inner.bytes -= n;
                            inner.full_since = None;
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

pub(super) struct BufferedOutput(pub Arc<Buffer>);
impl PortOutput for BufferedOutput {
    fn write_volatile(&mut self, source: &vm_memory::VolatileSlice) -> std::io::Result<usize> {
        // Copy in bounded chunks even if a malicious descriptor spans all guest RAM.
        let mut offset = 0;
        while offset < source.len() {
            let count = CHUNK.min(source.len() - offset);
            let mut bytes = vec![0; count];
            source
                .offset(offset)
                .map_err(std::io::Error::other)?
                .copy_to(&mut bytes);
            self.0.push(&bytes);
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
        assert_eq!(inner.chunks.front().unwrap()[0], 1);
    }
}
