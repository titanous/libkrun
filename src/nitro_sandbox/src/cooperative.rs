//! Validation of the arm64 kernel's cooperative freeze publication.
//!
//! This is the architecture-specific portion of template capture, not a second
//! public template interface. Callers must first stop all device workers and
//! provide only boot-reported, guest-owned RAM to the range predicate.
use std::io;

pub const FROZEN_MAGIC: u64 = 0x4652_4f5a_454e_3031;
pub const CONTROL_SIZE: u64 = 4096;
pub const MAX_GIC_WRITES: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeEntry {
    pub entry: u64,
    pub gic_list: u64,
    pub gic_count: u64,
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid arm64 cooperative freeze publication",
    )
}

impl ResumeEntry {
    /// Parse a stable copy after observing the frozen magic with acquire ordering.
    /// `contains` must exclude firmware-owned memory and the parked receiver.
    pub fn parse(page: &[u8], contains: impl Fn(u64, u64) -> bool) -> io::Result<Self> {
        if page.len() != CONTROL_SIZE as usize {
            return Err(invalid());
        }
        let word = |n: usize| u64::from_le_bytes(page[n * 8..n * 8 + 8].try_into().unwrap());
        let result = Self {
            entry: word(5),
            gic_count: word(6),
            gic_list: word(7),
        };
        if word(0) != FROZEN_MAGIC
            || word(1) != 0
            || result.entry == 0
            || !result.entry.is_multiple_of(4)
            || !contains(result.entry, 4)
            || result.gic_count == 0
            || result.gic_count > MAX_GIC_WRITES
            || !result.gic_list.is_multiple_of(8)
            || result.gic_list.checked_add(result.gic_count * 16).is_none()
            || !contains(result.gic_list, result.gic_count * 16)
        {
            return Err(invalid());
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> [u8; 4096] {
        let mut page = [0; 4096];
        for (index, value) in [
            (0, FROZEN_MAGIC),
            (5, 0x4000_1000),
            (6, 151),
            (7, 0x4000_2000),
        ] {
            page[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        page
    }
    fn contains(address: u64, len: u64) -> bool {
        address >= 0x4000_0000
            && address
                .checked_add(len)
                .is_some_and(|end| end <= 0x8000_0000)
    }
    #[test]
    fn accepts_published_arm64_entry() {
        let entry = ResumeEntry::parse(&page(), contains).unwrap();
        assert_eq!(entry.entry, 0x4000_1000);
        assert_eq!(entry.gic_count, 151);
    }
    #[test]
    fn refuses_unfrozen_released_and_unowned_entries() {
        for (index, value) in [
            (0, 0),
            (1, 1),
            (5, 0x8000_0000),
            (5, 0x4000_1001),
            (6, 257),
            (6, 0),
            (7, u64::MAX - 7),
            (7, 0x7fff_fff8),
        ] {
            let mut page = page();
            page[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
            assert!(ResumeEntry::parse(&page, contains).is_err(), "word {index}");
        }
    }
    #[test]
    fn short_publications_never_panic() {
        let page = page();
        for len in 0..4096 {
            assert!(ResumeEntry::parse(&page[..len], contains).is_err());
        }
    }
}
