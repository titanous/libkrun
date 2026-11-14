//! Bounded guest diagnostics. Excess output is dropped, never buffered.
use std::io::{self, Write};
use std::time::{Duration, Instant};

/// Sees every console byte the guest wrote, before the byte budgets (see
/// `Console::set_observer`). Same name and shape as on fork branch nitro-stop2.
pub type RawObserver = Box<dyn FnMut(&[u8]) + Send>;

/// The longest console line the panic matcher keeps (a stop line is about 60 bytes; the
/// matcher keeps the END of a longer line, where the marker must be).
const LINE_KEEP: usize = 512;
const MARKER: &[u8] = b"HOLODECK GUEST STOP ";

pub struct Console {
    sink: Option<Box<dyn Write + Send>>,
    observer: Option<RawObserver>,
    total_limit: usize,
    rate_limit: usize,
    total: usize,
    interval_bytes: usize,
    interval: Instant,
    /// The current console line (its last `LINE_KEEP` bytes).
    line: Vec<u8>,
    panic_seen: bool,
    discovery: Option<crate::freeze_discovery::Discovery>,
}
impl Default for Console {
    fn default() -> Self {
        Self::new(None, 0, 0)
    }
}
impl Console {
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
            line: Vec::new(),
            panic_seen: false,
            discovery: None,
        }
    }
    /// Give every byte the guest writes to `observer` as well, BEFORE the byte budgets: a
    /// stop marker printed after the guest flooded its console still reaches it. It runs on the
    /// console thread and must not block.
    pub fn set_observer(&mut self, observer: RawObserver) {
        self.observer = Some(observer);
    }
    pub fn enable_freeze_discovery(&mut self) {
        self.discovery = Some(crate::freeze_discovery::Discovery::default());
    }
    pub fn freeze_discovery(&self) -> Option<&crate::freeze_discovery::Discovery> {
        self.discovery.as_ref()
    }
    /// Whether the kernel printed its panic stop line. Only the kernel's line shape counts:
    /// a printk timestamp `[ secs.micros] `, optionally `reboot: `, then
    /// `HOLODECK GUEST STOP panic` and the end of the line (the same rule as the holodeck
    /// VMM's other stop reasons). The match sees every byte, including bytes over the budgets.
    pub fn panic_seen(&self) -> bool {
        self.panic_seen
    }
}

/// Whether `before` ends with a printk timestamp `[ secs.micros] `, optionally followed by
/// `reboot: `.
fn kernel_prefix(before: &[u8]) -> bool {
    let before = before.strip_suffix(b"reboot: ").unwrap_or(before);
    let Some(before) = before.strip_suffix(b"] ") else {
        return false;
    };
    let Some(dot) = before.iter().rposition(|&b| b == b'.') else {
        return false;
    };
    let micros = &before[dot + 1..];
    if micros.len() != 6 || !micros.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let head = &before[..dot];
    let digits = head.iter().rev().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let head = &head[..head.len() - digits];
    let spaces = head.iter().rev().take_while(|&&b| b == b' ').count();
    head[..head.len() - spaces].ends_with(b"[")
}

/// Whether a complete console line (without its newline) is the kernel's stop line for
/// `reason`. The marker may follow unterminated output of another console writer.
fn stop_line(line: &[u8], reason: &[u8]) -> bool {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(before) = line.strip_suffix(reason) else {
        return false;
    };
    let Some(before) = before.strip_suffix(MARKER) else {
        return false;
    };
    kernel_prefix(before)
}

impl Write for Console {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(observer) = &mut self.observer {
            observer(bytes);
        }
        if let Some(discovery) = &mut self.discovery {
            discovery.observe(bytes)?;
        }
        for &byte in bytes {
            if byte == b'\n' {
                if stop_line(&self.line, b"panic") {
                    self.panic_seen = true;
                }
                self.line.clear();
            } else {
                if self.line.len() == LINE_KEEP {
                    self.line.drain(..LINE_KEEP / 2);
                }
                self.line.push(byte);
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
    const PANIC_LINE: &[u8] = b"[   12.345678] HOLODECK GUEST STOP panic\n";
    #[test]
    fn panic_marker_survives_every_split_and_discarded_output() {
        for split in 0..=PANIC_LINE.len() {
            // Budgets of zero: everything is discarded, the match still sees it.
            let mut console = Console::default();
            console.write_all(b"boot noise\n").unwrap();
            console.write_all(&PANIC_LINE[..split]).unwrap();
            console.write_all(&PANIC_LINE[split..]).unwrap();
            assert!(console.panic_seen(), "split {split}");
        }
        for shaped in [
            &b"[    0.000001] reboot: HOLODECK GUEST STOP panic\r\n"[..],
            b"unterminated other writer[ 3.000000] HOLODECK GUEST STOP panic\n",
        ] {
            let mut console = Console::default();
            console.write_all(shaped).unwrap();
            assert!(console.panic_seen(), "{}", String::from_utf8_lossy(shaped));
        }
    }
    #[test]
    fn only_the_kernel_line_shape_counts_as_a_panic() {
        // Review daemon2-opus S1: the marker anywhere in the stream used to count.
        for unshaped in [
            &b"HOLODECK GUEST STOP panic\n"[..],
            b"echo HOLODECK GUEST STOP panic\n",
            b"[timestamp] reboot: HOLODECK GUEST STOP panic\n",
            b"[   12.345678] HOLODECK GUEST STOP panic and more\n",
            b"[   12.345678] HOLODECK GUEST STOP panicked\n",
            // No end of line yet: not a line.
            b"[   12.345678] HOLODECK GUEST STOP panic",
        ] {
            let mut console = Console::default();
            console.write_all(unshaped).unwrap();
            assert!(
                !console.panic_seen(),
                "{}",
                String::from_utf8_lossy(unshaped)
            );
        }
    }
    #[test]
    fn a_marker_after_a_flood_beyond_the_budget_is_still_seen() {
        // Review daemon2-opus S2: bytes over the budget were dropped before any scan.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut console = Console::new(Some(Box::new(Sink(captured.clone()))), 64, 64);
        let seen = observed.clone();
        console.set_observer(Box::new(move |bytes| {
            seen.lock().unwrap().extend_from_slice(bytes)
        }));
        console.write_all(&[b'x'; 10000]).unwrap();
        console.write_all(b"\n").unwrap();
        console.write_all(PANIC_LINE).unwrap();
        assert_eq!(captured.lock().unwrap().len(), 64, "the sink is bounded");
        assert!(console.panic_seen(), "the match saw bytes over the budget");
        assert_eq!(
            observed.lock().unwrap().len(),
            10000 + 1 + PANIC_LINE.len(),
            "the observer sees every byte"
        );
        assert!(observed.lock().unwrap().ends_with(PANIC_LINE));
    }
    #[test]
    fn a_very_long_line_keeps_its_end() {
        let mut console = Console::default();
        console.write_all(&[b'y'; 5000]).unwrap();
        console.write_all(PANIC_LINE).unwrap();
        assert!(console.panic_seen());
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
    }
}
