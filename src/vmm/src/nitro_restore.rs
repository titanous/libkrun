//! Restore a sealed common template into an already parked Nitro receiver.
use crate::template::Template;
use std::{io, os::unix::fs::FileExt};

pub struct NitroRestore {
    pub template: Template,
    pub vcpus: u8,
    pub ram_mib: u64,
    pub topology: Vec<u8>,
    pub compatibility: Vec<u8>,
}
impl nitro_sandbox::runtime::RestoreHandler for NitroRestore {
    fn restore(self: Box<Self>, sandbox: &nitro_sandbox::host::Sandbox) -> io::Result<()> {
        use std::io::Read;
        if sandbox.memory_size() as u64 != self.ram_mib << 20 {
            return Err(invalid());
        }
        validate(
            &self.template,
            self.vcpus,
            self.ram_mib,
            &self.topology,
            &self.compatibility,
        )?;
        validate_parked_receiver(&self.template, sandbox)?;
        let mut seed = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
        let realtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos() as u64;
        // SAFETY: runtime invokes this only after parked receiver READY; source
        // manifest was validated and all fresh backends are idle before release.
        unsafe {
            copy_and_release(
                &self.template,
                sandbox.ram.address() as *mut u8,
                self.ram_mib,
                seed,
                realtime,
            )
        }
    }
}

/// Check the parked receiver's independently injected kernel identity before copy.
pub fn validate_parked_receiver(
    template: &Template,
    sandbox: &nitro_sandbox::host::Sandbox,
) -> io::Result<()> {
    if sandbox.memory_size() < 1 << 30 {
        return Err(invalid());
    }
    let control =
        (sandbox.ram.address() as usize + nitro_sandbox::receiver::CONTROL_OFFSET) as *const u64;
    // SAFETY: aligned control page lies in the live >=1GiB sandbox allocation.
    let identity = unsafe {
        if control.read_volatile() != nitro_sandbox::receiver::READY {
            return Err(invalid());
        }
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        if control.add(1).read_volatile() != 0 || control.add(9).read_volatile() != 20 {
            return Err(invalid());
        }
        let mut identity = [0u8; 20];
        for (index, byte) in identity.iter_mut().enumerate() {
            *byte = control.cast::<u8>().add(48 + index).read_volatile();
        }
        identity
    };
    validate_receiver_identity(&template.compatibility, &identity)
}

fn validate_receiver_identity(compatibility: &[u8], identity: &[u8; 20]) -> io::Result<()> {
    const PREFIX: &[u8] = b"nitro-arm64-v1";
    if !identity.iter().any(|byte| *byte != 0)
        || compatibility.len() != PREFIX.len() + identity.len()
        || !compatibility.starts_with(PREFIX)
        || &compatibility[PREFIX.len()..] != identity
    {
        return Err(invalid());
    }
    Ok(())
}

/// Validate all refusable manifest checks before touching receiver memory.
pub fn validate(
    template: &Template,
    vcpus: u8,
    ram_mib: u64,
    topology: &[u8],
    compatibility: &[u8],
) -> io::Result<()> {
    if template.architecture != crate::template::Architecture::Aarch64
        || template.vcpus != vcpus
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
    #[test]
    fn receiver_identity_must_match_running_source_kernel() {
        let mut compatibility = b"nitro-arm64-v1".to_vec();
        compatibility.extend_from_slice(&[7; 20]);
        assert!(super::validate_receiver_identity(&compatibility, &[7; 20]).is_ok());
        assert!(super::validate_receiver_identity(&compatibility, &[8; 20]).is_err());
        assert!(super::validate_receiver_identity(&compatibility, &[0; 20]).is_err());
        assert!(super::validate_receiver_identity(&compatibility[..13], &[7; 20]).is_err());
    }

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
            template.control_page_address >= gpa
                && template.control_page_address + 4096 <= gpa + len as u64
        })
        .ok_or_else(invalid)?;
    template
        .file
        .read_exact_at(&mut page, file_offset + template.control_page_address - gpa)?;
    let entry = ResumeEntry::parse(&page, |a, n| {
        template.regions.iter().any(|&(gpa, len, _)| {
            a >= gpa && a.checked_add(n).is_some_and(|end| end <= gpa + len as u64)
        })
    })?;
    let _ = len;
    let copy_started = std::time::Instant::now();
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
    if std::env::var_os("NITRO_COPY_TIMINGS").is_some() {
        eprintln!(
            "NITRO_IMPORT_TIMING copy_us={} ram_mib={}",
            copy_started.elapsed().as_micros(),
            ram_mib
        );
    }
    let offset = if template.control_page_address < 0x80000000 {
        template.control_page_address - 0x40000000
    } else {
        template.control_page_address - 0x400000000 + 0x40000000
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
