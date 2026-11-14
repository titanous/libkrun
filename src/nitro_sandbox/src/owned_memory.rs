//! Guest-owned RAM ranges from the pinned arm64 kernel's early boot report.
//! Never infer ownership from the complete driver allocation or NUMA span.
use std::{collections::BTreeMap, io};

#[derive(Default)]
pub struct BootMemory(BTreeMap<u64, u64>);
impl BootMemory {
    pub fn observe(&mut self, line: &str) -> io::Result<()> {
        if !line.contains("node   0: [mem ") {
            return Ok(());
        }
        let range = line
            .split("[mem ")
            .nth(1)
            .and_then(|s| s.strip_suffix(']'))
            .ok_or_else(invalid)?;
        let (start, end) = range.split_once('-').ok_or_else(invalid)?;
        let parse =
            |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| invalid());
        let start = parse(start)?;
        let end = parse(end)?.checked_add(1).ok_or_else(invalid)?;
        if end <= start || (start | end) & 4095 != 0 {
            return Err(invalid());
        }
        if self.0.insert(start, end).is_some_and(|old| old != end) {
            return Err(invalid());
        }
        Ok(())
    }
    /// Return GPA, host-backing offset and length, after validating the full plan.
    pub fn ranges(&self, memory_size: usize) -> io::Result<Vec<(u64, usize, usize)>> {
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (&start, &end) in &self.0 {
            if let Some(last) = merged.last_mut() {
                if start < last.1 {
                    return Err(invalid());
                }
                if start == last.1 {
                    last.1 = end;
                    continue;
                }
            }
            merged.push((start, end));
        }
        if merged.len() != if memory_size <= 1 << 30 { 1 } else { 2 } {
            return Err(invalid());
        }
        merged
            .into_iter()
            .enumerate()
            .map(|(index, (gpa, end))| {
                let expected = if index == 0 { 0x40000000 } else { 0x400000000 };
                if gpa != expected {
                    return Err(invalid());
                }
                let offset = if index == 0 { 0 } else { 1 << 30 };
                let len = usize::try_from(end - gpa).map_err(|_| invalid())?;
                if offset + len > memory_size
                    || (index == 0 && len > 1 << 30)
                    || (index == 0 && memory_size > 1 << 30 && len != 1 << 30)
                {
                    return Err(invalid());
                }
                Ok((gpa, offset, len))
            })
            .collect()
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid boot-owned Nitro RAM extents",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merges_true_ranges_not_numa_hole() {
        let mut memory = BootMemory::default();
        for line in [
            "node   0: [mem 0x40000000-0x77ffffff]",
            "node   0: [mem 0x78000000-0x7fffffff]",
            "node   0: [mem 0x400000000-0x4bf5fffff]",
            "Initmem setup node 0 [mem 0x40000000-0x4bf5fffff]",
        ] {
            memory.observe(line).unwrap();
        }
        assert_eq!(
            memory.ranges(4 << 30).unwrap(),
            vec![(0x40000000, 0, 1 << 30), (0x400000000, 1 << 30, 0xbf600000)]
        );
        assert!(memory.ranges(1 << 30).is_err());
    }
    #[test]
    fn refuses_overlap_truncation_and_missing_extent() {
        let mut memory = BootMemory::default();
        assert!(memory.ranges(1 << 30).is_err());
        assert!(memory
            .observe("node   0: [mem 0x40000000-0xffffffffffffffff]")
            .is_err());
        memory
            .observe("node   0: [mem 0x40000000-0x7f5fffff]")
            .unwrap();
        assert_eq!(memory.ranges(1 << 30).unwrap()[0].2, 0x3f600000);
        assert!(memory.ranges(4 << 30).is_err());
        memory
            .observe("node   0: [mem 0x50000000-0x7fffffff]")
            .unwrap();
        assert!(memory.ranges(1 << 30).is_err());
    }
}
