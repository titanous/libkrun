//! Guest-published cooperative freeze control page (little-endian x86-64).

use std::io;

pub const FROZEN_MAGIC: u64 = 0x4652_4f5a_454e_3031;
pub const FREEZE_PORT: u16 = 0xe5;
pub const CONTROL_SIZE: usize = 4096;

/// State required before the guest's x86 continuation can execute.
#[derive(Clone, Debug)]
pub struct ResumeEntry {
    pub stack: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub rip: u64,
    pub cr0: u64,
    pub efer: u64,
    pub gs_base: u64,
    pub gdt_base: u64,
    pub gdt_limit: u16,
    pub idt_base: u64,
    pub idt_limit: u16,
}

impl ResumeEntry {
    /// The exit signal alone is insufficient: RAM must contain a complete,
    /// supported publication. This does not replace topology/CPUID validation.
    pub fn parse(page: &[u8; CONTROL_SIZE]) -> io::Result<Self> {
        let word = |n: usize| u64::from_le_bytes(page[n * 8..n * 8 + 8].try_into().unwrap());
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid frozen x86 control page",
            )
        };
        if word(0) != FROZEN_MAGIC || word(1) != 0 || word(19) != 1 {
            return Err(invalid());
        }
        let canonical = |v: u64| (v as i64 >> 47) == 0 || (v as i64 >> 47) == -1;
        if word(2) == 0
            || word(5) == 0
            || !canonical(word(2))
            || !canonical(word(5))
            || word(3) == 0
            || word(3) & 4095 != 0
            || word(20) & 0x8000_0001 != 0x8000_0001
            || word(4) & (1 << 5) == 0
            || word(21) & 0x500 != 0x500
            || word(24) > u16::MAX as u64
            || word(26) > u16::MAX as u64
        {
            return Err(invalid());
        }
        Ok(Self {
            stack: word(2),
            cr3: word(3),
            cr4: word(4),
            rip: word(5),
            cr0: word(20),
            efer: word(21),
            gs_base: word(22),
            gdt_base: word(23),
            gdt_limit: word(24) as u16,
            idt_base: word(25),
            idt_limit: word(26) as u16,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published() -> [u8; CONTROL_SIZE] {
        let mut page = [0; CONTROL_SIZE];
        for (n, v) in [
            (0, FROZEN_MAGIC),
            (2, 0xffff_8880_0010_0000),
            (3, 0x2000),
            (4, 0x20),
            (5, 0xffff_ffff_8100_0000),
            (19, 1),
            (20, 0x8000_0011),
            (21, 0x500),
        ] {
            page[n * 8..n * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        page
    }

    #[test]
    fn accepts_published_long_mode_entry() {
        let entry = ResumeEntry::parse(&published()).unwrap();
        assert_eq!(entry.cr3, 0x2000);
        assert_eq!(entry.rip, 0xffff_ffff_8100_0000);
    }

    #[test]
    fn rejects_unfrozen_and_invalid_entry() {
        for (word, value) in [
            (0, 0u64),
            (1, 1),
            (19, 2),
            (2, 0),
            (3, 0x2001),
            (4, 0),
            (20, 1),
            (21, 0),
            (24, 65536),
        ] {
            let mut page = published();
            page[word * 8..word * 8 + 8].copy_from_slice(&value.to_le_bytes());
            assert!(ResumeEntry::parse(&page).is_err(), "word {word}");
        }
    }
}
