//! Bounded guest diagnostics. Excess output is dropped, never buffered.
use std::io::{self, Write};
use std::time::{Duration, Instant};

pub struct Console {
    sink: Option<Box<dyn Write + Send>>,
    total_limit: usize,
    rate_limit: usize,
    total: usize,
    interval_bytes: usize,
    interval: Instant,
    panic_match: usize,
    panic_seen: bool,
}
impl Default for Console {
    fn default() -> Self {
        Self::new(None, 0, 0)
    }
}
impl Console {
    /// Sink writes must be nonblocking. Budgets bound accepted bytes, not the
    /// runtime of an arbitrary embedder callback.
    pub fn new(sink: Option<Box<dyn Write + Send>>, total_limit: usize, rate_limit: usize) -> Self {
        Self {
            sink,
            total_limit,
            rate_limit,
            total: 0,
            interval_bytes: 0,
            interval: Instant::now(),
            panic_match: 0,
            panic_seen: false,
        }
    }
}
impl Console {
    pub fn panic_seen(&self) -> bool {
        self.panic_seen
    }
}
impl Write for Console {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        const MARKER: &[u8] = b"HOLODECK GUEST STOP panic";
        for &byte in bytes {
            if byte == MARKER[self.panic_match] {
                self.panic_match += 1;
                if self.panic_match == MARKER.len() {
                    self.panic_seen = true;
                    self.panic_match = 0;
                }
            } else {
                self.panic_match = usize::from(byte == MARKER[0]);
            }
        }
        if self.interval.elapsed() >= Duration::from_secs(1) {
            self.interval = Instant::now();
            self.interval_bytes = 0;
        }
        let allowed = bytes
            .len()
            .min(self.total_limit.saturating_sub(self.total))
            .min(self.rate_limit.saturating_sub(self.interval_bytes));
        if let Some(sink) = &mut self.sink {
            match sink.write(&bytes[..allowed]) {
                Ok(written) => {
                    self.total += written;
                    self.interval_bytes += written;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn panic_marker_survives_every_split_and_discarded_output() {
        let marker = b"HOLODECK GUEST STOP panic";
        for split in 0..=marker.len() {
            let mut console = Console::default();
            console.write_all(b"[timestamp] reboot: ").unwrap();
            console.write_all(&marker[..split]).unwrap();
            console.write_all(&marker[split..]).unwrap();
            assert!(console.panic_seen());
        }
    }

    #[test]
    fn flood_is_bounded_in_rate_and_total_without_buffering() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut console = Console::new(Some(Box::new(Sink(captured.clone()))), 12, 8);
        console.write_all(&[b'x'; 10000]).unwrap();
        console.write_all(&[b'x'; 10000]).unwrap();
        assert_eq!(captured.lock().unwrap().len(), 8);
        console.interval = Instant::now() - Duration::from_secs(2);
        console.write_all(&[b'x'; 10000]).unwrap();
        assert_eq!(captured.lock().unwrap().len(), 12);
    }
}
