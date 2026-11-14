//! Bounded discovery for the pinned cooperative arm64 kernel.
//! Console text is untrusted: addresses must subsequently pass owned-RAM and
//! kernel-identity validation before a capture is accepted.
use crate::owned_memory::BootMemory;
use std::io;
#[derive(Default)]
pub struct Discovery {
    line: Vec<u8>,
    dropping: bool,
    pub memory: BootMemory,
    pub control: Option<u64>,
    pub receiver_reserved: bool,
}
impl Discovery {
    pub fn observe(&mut self, bytes: &[u8]) -> io::Result<()> {
        for &byte in bytes {
            if byte == b'\n' {
                let result = if !self.dropping {
                    self.complete()
                } else {
                    Ok(())
                };
                self.line.clear();
                self.dropping = false;
                result?;
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
        self.memory.observe(&line)?;
        if line.contains("NITRO_RECEIVER_RANGE reserved [0x70000000-0x701fffff]") {
            self.receiver_reserved = true;
        }
        if let Some(value) = line.split("NITRO_FREEZE_CONTROL phys=").nth(1) {
            let address =
                u64::from_str_radix(value.trim().trim_start_matches("0x"), 16).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid freeze control publication",
                    )
                })?;
            if address & 4095 != 0 || self.control.is_some_and(|old| old != address) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "inconsistent freeze control publication",
                ));
            }
            self.control = Some(address);
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_lines_and_oversize_input_are_bounded() {
        let mut discovery = Discovery::default();
        for byte in b"[0] NITRO_FREEZE_CONTROL phys=0x0000000077119000\n" {
            discovery.observe(&[*byte]).unwrap();
        }
        assert_eq!(discovery.control, Some(0x77119000));
        discovery.observe(&vec![b'x'; 100000]).unwrap();
        assert!(discovery.line.len() <= 4096);
        discovery
            .observe(b"\nNITRO_RECEIVER_RANGE reserved [0x70000000-0x701fffff]\n")
            .unwrap();
        assert!(discovery.receiver_reserved);
    }
    #[test]
    fn refuses_conflicting_and_unaligned_control() {
        let mut discovery = Discovery::default();
        assert!(discovery
            .observe(b"NITRO_FREEZE_CONTROL phys=0x40000001\n")
            .is_err());
        discovery
            .observe(b"NITRO_FREEZE_CONTROL phys=0x40001000\n")
            .unwrap();
        assert!(discovery
            .observe(b"NITRO_FREEZE_CONTROL phys=0x40002000\n")
            .is_err());
    }
}
