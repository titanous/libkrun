//! Restore a sealed common template into an already parked Nitro receiver.
use crate::template::Template;
use std::{io, os::unix::fs::FileExt};

/// Validate all refusable manifest checks before touching receiver memory.
pub fn validate(
    template: &Template,
    vcpus: u8,
    ram_mib: u64,
    topology: &[u8],
    compatibility: &[u8],
) -> io::Result<()> {
    if template.vcpus != vcpus
        || template.ram_mib != ram_mib
        || template.topology != topology
        || template.compatibility != compatibility
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Nitro template shape/device/kernel identity mismatch",
        ));
    }
    let expected = if ram_mib <= 1024 { 1 } else { 2 };
    if template.regions.len() != expected {
        return Err(invalid());
    }
    for (index, &(gpa, len, _)) in template.regions.iter().enumerate() {
        let start = if index == 0 { 0x40000000 } else { 0x400000000 };
        let max = if index == 0 {
            ram_mib.min(1024) << 20
        } else {
            (ram_mib - 1024) << 20
        };
        if gpa != start
            || (index == 0 && len < 0x30200000)
            || len == 0
            || len as u64 > max
            || (index == 0 && ram_mib > 1024 && len as u64 != max)
        {
            return Err(invalid());
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn template() -> Template {
        Template {
            file: crate::template_memory::create(4096).unwrap(),
            control: 0x40001000,
            vcpus: 4,
            ram_mib: 1024,
            clock_ns: 0,
            regions: vec![(0x40000000, 0x3f600000, 24576)],
            topology: b"devices".to_vec(),
            compatibility: b"kernel-cpu".to_vec(),
        }
    }
    #[test]
    fn refuses_manifest_mismatches_before_restore() {
        let t = template();
        assert!(validate(&t, 4, 1024, b"devices", b"kernel-cpu").is_ok());
        assert!(validate(&t, 1, 1024, b"devices", b"kernel-cpu").is_err());
        assert!(validate(&t, 4, 4096, b"devices", b"kernel-cpu").is_err());
        assert!(validate(&t, 4, 1024, b"different", b"kernel-cpu").is_err());
        assert!(validate(&t, 4, 1024, b"devices", b"other-kernel").is_err());
    }
    #[test]
    fn refuses_layout_outside_receiver_backing() {
        for (gpa, len) in [
            (0, 0x3f600000),
            (0x40000000, 0x50000000),
            (0x40000000, 0x1000),
        ] {
            let mut t = template();
            t.regions[0] = (gpa, len, 24576);
            assert!(validate(&t, 4, 1024, b"devices", b"kernel-cpu").is_err());
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid Nitro template RAM layout",
    )
}

/// Copy a validated template into a live, parked receiver mapping.
///
/// # Safety
/// `base` must be an exclusive writable driver RAM mapping of `ram_mib` MiB.
/// Receiver must have published READY and all backends must be fresh and idle.
/// Caller validates shape, topology and kernel identity with `validate` first.
/// The mapping and all device backends must remain alive through receiver close.
pub unsafe fn copy_and_release(
    template: &Template,
    base: *mut u8,
    ram_mib: u64,
    seed: [u8; 32],
    realtime: u64,
) -> io::Result<()> {
    use nitro_sandbox::{cooperative::ResumeEntry, receiver};
    if base.is_null() || ram_mib != template.ram_mib || !(1024..=4096).contains(&ram_mib) {
        return Err(invalid());
    }
    validate(
        template,
        template.vcpus,
        ram_mib,
        &template.topology,
        &template.compatibility,
    )?;
    let control = unsafe { base.add(receiver::CONTROL_OFFSET) } as *mut u64;
    let mut state = [0u64; 6];
    for (index, word) in state.iter_mut().enumerate() {
        *word = unsafe { control.add(index).read_volatile() };
    }
    receiver::validate_ready(&state)?;
    let mut page = [0u8; 4096];
    let &(gpa, len, file_offset) = template
        .regions
        .iter()
        .find(|&&(gpa, len, _)| {
            template.control >= gpa && template.control + 4096 <= gpa + len as u64
        })
        .ok_or_else(invalid)?;
    template
        .file
        .read_exact_at(&mut page, file_offset + template.control - gpa)?;
    let entry = ResumeEntry::parse(&page, |a, n| {
        template.regions.iter().any(|&(gpa, len, _)| {
            a >= gpa && a.checked_add(n).is_some_and(|end| end <= gpa + len as u64)
        })
    })?;
    let _ = len;
    let mut buffer = vec![0u8; 1024 * 1024];
    for &(gpa, len, file_offset) in &template.regions {
        let offset = if gpa == 0x40000000 { 0 } else { 1 << 30 };
        let mut position = 0usize;
        while position < len {
            let count = (len - position).min(buffer.len());
            template
                .file
                .read_exact_at(&mut buffer[..count], file_offset + position as u64)?;
            let at = offset + position;
            if !(0x30000000..0x30200000).contains(&at) {
                unsafe {
                    std::ptr::copy_nonoverlapping(buffer.as_ptr(), base.add(at), count);
                }
            }
            position += count;
        }
    }
    let offset = if template.control < 0x80000000 {
        template.control - 0x40000000
    } else {
        template.control - 0x400000000 + 0x40000000
    };
    let guest = unsafe { base.add(offset as usize) } as *mut u64;
    unsafe {
        guest.add(8).write_volatile(realtime);
        std::ptr::copy_nonoverlapping(seed.as_ptr(), guest.add(10) as *mut u8, 32);
        guest.add(14).write_volatile(1);
        receiver::release(control, entry)?;
    }
    Ok(())
}
