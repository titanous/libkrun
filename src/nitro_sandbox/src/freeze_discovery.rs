//! Bounded boot-only discovery for the pinned cooperative arm64 kernel.
//! The final marker is a late_initcall publication before any userspace starts.
use crate::owned_memory::BootMemory;
use std::io;
#[derive(Default)]
pub struct Discovery {
    line: Vec<u8>,
    dropping: bool,
    finished: bool,
    pub disabled: bool,
    pub memory: BootMemory,
    pub control: Option<u64>,
    pub receiver_reserved: bool,
    pub marker: Option<bool>,
}
impl Discovery {
    /// Runs before console budgets/lossy sinks. After the kernel's final marker
    /// or init-exec announcement, no byte (including kmsg replay) is interpreted.
    pub fn observe(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.finished || self.disabled {
            return Ok(());
        }
        for &byte in bytes {
            if byte == b'\n' {
                if self.dropping || self.complete().is_err() {
                    self.disabled = true;
                    self.finished = true;
                    self.marker = None;
                }
                self.line.clear();
                self.dropping = false;
                if self.finished || self.disabled {
                    break;
                }
            } else if self.line.len() < 4096 && !self.dropping {
                self.line.push(byte);
            } else {
                self.dropping = true;
                self.line.clear();
            }
        }
        Ok(())
    }
    fn complete(&mut self) -> io::Result<()> {
        let line = String::from_utf8_lossy(&self.line);
        let line = line.trim_end_matches('\r');
        // The kernel prints this immediately before exec of PID 1. If the
        // marker was absent/lost, never permit a userspace imitation to arm it.
        if line.contains("Run ") && line.contains(" as init process") {
            self.disabled = true;
            self.finished = true;
            return Ok(());
        }
        self.memory.observe(line)?;
        if line.contains("NITRO_RECEIVER_RANGE") {
            if self.receiver_reserved
                || !line.ends_with("NITRO_RECEIVER_RANGE reserved [0x70000000-0x701fffff]")
            {
                return Err(invalid());
            }
            self.receiver_reserved = true;
        }
        if let Some(value) = line.split("NITRO_FREEZE_CONTROL phys=").nth(1) {
            if self.control.is_some() {
                return Err(invalid());
            }
            let address = u64::from_str_radix(value.trim().trim_start_matches("0x"), 16)
                .map_err(|_| invalid())?;
            if address & 4095 != 0 {
                return Err(invalid());
            }
            self.control = Some(address);
        }
        if let Some(value) = line.split("NITRO_TEMPLATE_MARKER active=").nth(1) {
            if self.marker.is_some() {
                return Err(invalid());
            }
            let active = match value.trim() {
                "0" => false,
                "1" => true,
                _ => return Err(invalid()),
            };
            self.marker = Some(active);
            self.finished = true;
        }
        Ok(())
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid boot capture publication",
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boot_publications_freeze_before_userspace_and_ignore_flood() {
        let mut d = Discovery::default();
        d.observe(b"node   0: [mem 0x40000000-0x7f5fffff]\r\nNITRO_RECEIVER_RANGE reserved [0x70000000-0x701fffff]\nNITRO_FREEZE_CONTROL phys=0x77119000\nNITRO_TEMPLATE_MARKER active=1\n").unwrap();
        for _ in 0..10000 {
            d.observe(b"NITRO_FREEZE_CONTROL phys=0x40000001\nnode   0: [mem 0x7f600000-0x7fffffff]\nNITRO_TEMPLATE_MARKER active=0\n").unwrap();
        }
        assert_eq!(d.control, Some(0x77119000));
        assert_eq!(d.marker, Some(true));
        assert!(!d.disabled);
        assert!(d.line.is_empty());
        assert_eq!(d.memory.ranges(1 << 30).unwrap()[0].2, 0x3f600000);
    }
    #[test]
    fn repeated_or_malformed_boot_publication_disables_without_io_error() {
        for input in [
            b"NITRO_FREEZE_CONTROL phys=0x40000001\n".as_slice(),
            b"NITRO_FREEZE_CONTROL phys=0x40001000\nNITRO_FREEZE_CONTROL phys=0x40001000\n",
            b"NITRO_TEMPLATE_MARKER active=2\n",
        ] {
            let mut d = Discovery::default();
            d.observe(input).unwrap();
            assert!(d.disabled);
            d.observe(b"NITRO_TEMPLATE_MARKER active=1\n").unwrap();
            assert_eq!(d.marker, None);
        }
    }
    #[test]
    fn missing_marker_is_closed_at_init_exec() {
        let mut d = Discovery::default();
        d.observe(b"Run /init as init process\nNITRO_TEMPLATE_MARKER active=1\n")
            .unwrap();
        assert!(d.disabled);
        assert_eq!(d.marker, None);
    }
    #[test]
    fn fragmented_and_oversize_lines_are_bounded() {
        let mut d = Discovery::default();
        for b in b"NITRO_FREEZE_CONTROL phys=0x77119000\r\n" {
            d.observe(&[*b]).unwrap();
        }
        assert_eq!(d.control, Some(0x77119000));
        d.observe(&vec![b'x'; 100000]).unwrap();
        assert!(d.line.len() <= 4096);
        d.observe(b"\n").unwrap();
        assert!(d.disabled);
    }
}
