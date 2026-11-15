//! Guest-published cooperative freeze control page (little-endian x86-64).

use std::io;

pub const FROZEN_MAGIC: u64 = 0x4652_4f5a_454e_3031;
pub const FREEZE_PORT: u16 = 0xe5;
pub const MARKER_PORT: u16 = 0xe9;
pub const CONTROL_SIZE: usize = 4096;

#[derive(Debug)]
pub struct TemplateMarkerMissing;
impl std::fmt::Display for TemplateMarkerMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "guest kernel did not confirm cooperative_template")
    }
}
impl std::error::Error for TemplateMarkerMissing {}

pub fn require_template_marker(page: &[u8; CONTROL_SIZE]) -> io::Result<()> {
    if u64::from_le_bytes(
        page[word::MARKER_ACTIVE * 8..(word::MARKER_ACTIVE + 1) * 8]
            .try_into()
            .unwrap(),
    ) != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            TemplateMarkerMissing,
        ));
    }
    Ok(())
}

/// Test-only (feature `copy-fault-injection`, never in a product build): with
/// `KRUN_TEST_OMIT_COPY_SEED` set, a copy starts without the host's fresh random seed and
/// seed-ready word, so tests can show that the guest refuses such a copy instead of handing out
/// its template's random state. Always false without the feature.
pub fn omit_copy_seed_for_test() -> bool {
    #[cfg(feature = "copy-fault-injection")]
    if std::env::var_os("KRUN_TEST_OMIT_COPY_SEED").is_some() {
        log::warn!("test: copy starts WITHOUT a fresh random seed (KRUN_TEST_OMIT_COPY_SEED)");
        return true;
    }
    false
}

/// Word offsets shared with the carried cooperative kernel protocol.
pub mod word {
    pub const MAGIC: usize = 0;
    pub const STATUS: usize = 1;
    pub const STACK: usize = 2;
    pub const CR3: usize = 3;
    pub const CR4: usize = 4;
    pub const RIP: usize = 5;
    pub const REALTIME: usize = 8;
    pub const RANDOM_START: usize = 9;
    pub const RANDOM_END: usize = 14;
    pub const SEED_READY: usize = 14;
    pub const VERSION: usize = 19;
    pub const CR0: usize = 20;
    pub const EFER: usize = 21;
    pub const GS_BASE: usize = 22;
    pub const GDT_BASE: usize = 23;
    pub const GDT_LIMIT: usize = 24;
    pub const IDT_BASE: usize = 25;
    pub const IDT_LIMIT: usize = 26;
    pub const KERNEL_BUILD_ID: usize = 27;
    pub const KERNEL_BUILD_ID_LEN: usize = 30;
    pub const TSC: usize = 31;
    pub const MARKER_ACTIVE: usize = 32;
}

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
    pub tsc: u64,
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
        if word(word::MAGIC) != FROZEN_MAGIC || word(word::STATUS) != 0 || word(word::VERSION) != 1
        {
            return Err(invalid());
        }
        let canonical = |v: u64| (v as i64 >> 47) == 0 || (v as i64 >> 47) == -1;
        if word(word::STACK) == 0
            || word(word::RIP) == 0
            || !canonical(word(word::STACK))
            || !canonical(word(word::RIP))
            || word(word::CR3) == 0
            // CR3 carries a PCID in bits 11:0 when CR4.PCIDE is set.
            // Preserve it: the continuation restores the same address space.
            || (word(word::CR4) & (1 << 17) == 0 && word(word::CR3) & 4095 != 0)
            || word(word::CR0) & 0x8000_0001 != 0x8000_0001
            || word(word::CR4) & (1 << 5) == 0
            || word(word::EFER) & 0x500 != 0x500
            || word(word::GDT_LIMIT) > u16::MAX as u64
            || word(word::IDT_LIMIT) > u16::MAX as u64
        {
            return Err(invalid());
        }
        Ok(Self {
            stack: word(word::STACK),
            cr3: word(word::CR3),
            cr4: word(word::CR4),
            rip: word(word::RIP),
            cr0: word(word::CR0),
            efer: word(word::EFER),
            gs_base: word(word::GS_BASE),
            gdt_base: word(word::GDT_BASE),
            gdt_limit: word(word::GDT_LIMIT) as u16,
            idt_base: word(word::IDT_BASE),
            idt_limit: word(word::IDT_LIMIT) as u16,
            tsc: word(word::TSC),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_requires_kernel_marker_confirmation() {
        let mut page = [0; CONTROL_SIZE];
        let error = require_template_marker(&page).unwrap_err();
        assert!(error.get_ref().unwrap().is::<TemplateMarkerMissing>());
        page[word::MARKER_ACTIVE * 8] = 1;
        require_template_marker(&page).unwrap();
    }

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
    #[cfg(target_arch = "x86_64")]
    fn reports_kvm_pcid_capability() {
        let kvm = kvm_ioctls::Kvm::new().unwrap();
        let cpuid = kvm
            .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
            .unwrap();
        let pcid = cpuid
            .as_slice()
            .iter()
            .any(|entry| entry.function == 1 && entry.ecx & (1 << 17) != 0);
        println!("KVM supported PCID={pcid}");
    }

    #[test]
    fn accepts_published_long_mode_entry() {
        let entry = ResumeEntry::parse(&published()).unwrap();
        assert_eq!(entry.cr3, 0x2000);
        assert_eq!(entry.rip, 0xffff_ffff_8100_0000);
    }

    #[test]
    fn accepts_pcid_when_enabled_and_preserves_identifier() {
        let mut page = published();
        page[3 * 8..4 * 8].copy_from_slice(&0x2123u64.to_le_bytes());
        page[4 * 8..5 * 8].copy_from_slice(&(0x20u64 | (1 << 17)).to_le_bytes());
        assert_eq!(ResumeEntry::parse(&page).unwrap().cr3, 0x2123);
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
