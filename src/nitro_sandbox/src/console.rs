//! Bounded guest diagnostics. Excess output is dropped, never buffered.
use std::io::{self, Write};
use std::time::{Duration, Instant};

/// Nonblocking callback over raw console bytes, before diagnostic loss.
pub type RawObserver = Box<dyn FnMut(&[u8]) + Send>;

pub struct Console {
    sink: Option<Box<dyn Write + Send>>,
    observer: Option<RawObserver>,
    total_limit: usize,
    rate_limit: usize,
    total: usize,
    interval_bytes: usize,
    interval: Instant,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    discovery: Option<crate::freeze_discovery::Discovery>,
}
impl Default for Console {
    fn default() -> Self {
        Self::new(None, 0, 0)
    }
}
impl Console {
    pub fn set_sink(&mut self, sink: Box<dyn Write + Send>, total_limit: usize, rate_limit: usize) {
        self.sink = Some(sink);
        self.total_limit = total_limit;
        self.rate_limit = rate_limit;
    }
    pub fn set_observer(&mut self, observer: RawObserver) {
        self.observer = Some(observer);
    }
    pub fn dropped_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.dropped.clone()
    }
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Sink writes must be nonblocking. Budgets bound accepted bytes, not the
    /// runtime of an arbitrary embedder callback. Bytes are raw guest output:
    /// the embedder must escape terminal controls and encode invalid UTF-8.
    pub fn new(sink: Option<Box<dyn Write + Send>>, total_limit: usize, rate_limit: usize) -> Self {
        Self {
            sink,
            observer: None,
            total_limit,
            rate_limit,
            total: 0,
            interval_bytes: 0,
            interval: Instant::now(),
            dropped: Default::default(),
            discovery: None,
        }
    }
}
impl Console {
    pub fn enable_freeze_discovery(&mut self) {
        self.discovery = Some(crate::freeze_discovery::Discovery::default());
    }
    pub fn freeze_discovery(&self) -> Option<&crate::freeze_discovery::Discovery> {
        self.discovery.as_ref()
    }
}
impl Write for Console {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Copy-control discovery first, on raw bytes, before any budget. The
        // embedder's raw observer still sees these bytes when discovery fails,
        // so a guest's stop text is never hidden by a discovery error.
        let discovered = match &mut self.discovery {
            Some(discovery) => discovery.observe(bytes),
            None => Ok(()),
        };
        if let Some(observer) = &mut self.observer {
            observer(bytes);
        }
        discovered?;
        if self.interval.elapsed() >= Duration::from_secs(1) {
            self.interval = Instant::now();
            self.interval_bytes = 0;
        }
        let allowed = bytes
            .len()
            .min(self.total_limit.saturating_sub(self.total))
            .min(self.rate_limit.saturating_sub(self.interval_bytes));
        let written = if let Some(sink) = &mut self.sink {
            match sink.write(&bytes[..allowed]) {
                Ok(written) => {
                    self.total += written;
                    self.interval_bytes += written;
                    written
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => 0,
                Err(error) => {
                    self.dropped
                        .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    return Err(error);
                }
            }
        } else {
            0
        };
        self.dropped.fetch_add(
            bytes.len().saturating_sub(written) as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
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
    #[test]
    fn freeze_discovery_is_explicit_and_independent_of_output_budget() {
        let mut console = Console::default();
        console
            .write_all(b"NITRO_FREEZE_CONTROL phys=0x40001000\n")
            .unwrap();
        assert!(console.freeze_discovery().is_none());
        console.enable_freeze_discovery();
        console
            .write_all(b"NITRO_FREEZE_CONTROL phys=0x40002000\n")
            .unwrap();
        assert_eq!(
            console.freeze_discovery().unwrap().control,
            Some(0x40002000)
        );
    }
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
    fn observer_sees_every_byte_before_budget_loss() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let observed = captured.clone();
        let mut console = Console::new(None, 0, 0);
        console.set_observer(Box::new(move |bytes| {
            observed.lock().unwrap().extend_from_slice(bytes)
        }));
        let bytes = b"[    1.234567] reboot: HOLODECK GUEST STOP panic\n";
        console.write_all(&bytes[..17]).unwrap();
        console.write_all(&bytes[17..]).unwrap();
        assert_eq!(&*captured.lock().unwrap(), bytes);
        assert_eq!(console.dropped_bytes(), bytes.len() as u64);
    }

    #[test]
    fn sink_receives_raw_bytes_and_default_discards() {
        let bytes = b"\x1b[2J\xffforged\n";
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut console = Console::new(Some(Box::new(Sink(captured.clone()))), 1024, 1024);
        console.write_all(bytes).unwrap();
        assert_eq!(&*captured.lock().unwrap(), bytes);
        let mut discard = Console::default();
        discard.write_all(bytes).unwrap();
        assert!(discard.sink.is_none());
        assert_eq!(discard.total, 0);
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
        assert_eq!(console.dropped_bytes(), 29988);
    }
}
