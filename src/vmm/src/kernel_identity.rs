//! GNU build identifier from a little-endian ELF64 kernel image.
use std::io;

pub fn build_id(bytes: &[u8]) -> io::Result<[u8; 20]> {
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "kernel ELF build identifier missing or invalid",
        )
    };
    let u16at = |p: usize| -> Option<u16> {
        Some(u16::from_le_bytes(
            bytes.get(p..p.checked_add(2)?)?.try_into().ok()?,
        ))
    };
    let u32at = |p: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(p..p.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    let u64at = |p: usize| -> Option<u64> {
        Some(u64::from_le_bytes(
            bytes.get(p..p.checked_add(8)?)?.try_into().ok()?,
        ))
    };
    if bytes.get(..6) != Some(&[0x7f, b'E', b'L', b'F', 2, 1]) {
        return Err(bad());
    }
    let phoff = usize::try_from(u64at(32).ok_or_else(bad)?).map_err(|_| bad())?;
    let stride = u16at(54).ok_or_else(bad)? as usize;
    let count = u16at(56).ok_or_else(bad)? as usize;
    if stride < 56 {
        return Err(bad());
    }
    for index in 0..count {
        let p = phoff
            .checked_add(index.checked_mul(stride).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        if u32at(p) != Some(4) {
            continue;
        }
        let mut pos = usize::try_from(u64at(p + 8).ok_or_else(bad)?).map_err(|_| bad())?;
        let size = usize::try_from(u64at(p + 32).ok_or_else(bad)?).map_err(|_| bad())?;
        let end = pos
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(bad)?;
        while pos.checked_add(12).is_some_and(|p| p <= end) {
            let namesz = u32at(pos).ok_or_else(bad)? as usize;
            let descsz = u32at(pos + 4).ok_or_else(bad)? as usize;
            let kind = u32at(pos + 8).ok_or_else(bad)?;
            let name = pos.checked_add(12).ok_or_else(bad)?;
            let desc = name
                .checked_add(namesz.checked_add(3).ok_or_else(bad)? & !3)
                .ok_or_else(bad)?;
            let next = desc
                .checked_add(descsz.checked_add(3).ok_or_else(bad)? & !3)
                .filter(|p| *p <= end)
                .ok_or_else(bad)?;
            if namesz == 4
                && descsz == 20
                && kind == 3
                && bytes.get(name..name + 4) == Some(b"GNU\0")
            {
                return bytes
                    .get(desc..desc + 20)
                    .ok_or_else(bad)?
                    .try_into()
                    .map_err(|_| bad());
            }
            pos = next;
        }
    }
    Err(bad())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_truncated_or_non_elf_input() {
        for size in 0..128 {
            assert!(build_id(&vec![0; size]).is_err());
        }
    }
}
