//! Host protocol and build assets for the kernel-matched low-RAM parked receiver.
//!
//! Assemble `STUB_SOURCE` for AArch64, link at 0x70000000 and extract the raw
//! binary, then run `IMAGE_INJECTOR` with Image, stub binary, output and the
//! vmlinux-derived build-id file. The injector verifies that exact identity in
//! the Image and binds it into the stub before READY can be published.
pub const STUB_SOURCE: &str = include_str!("receiver_image/receiver.S");
pub const IMAGE_INJECTOR: &str = include_str!("receiver_image/receiver-image.py");
use crate::cooperative::ResumeEntry;
use std::{
    io,
    sync::atomic::{fence, Ordering},
};
pub const RESERVATION_GPA: u64 = 0x70000000;
pub const RESERVATION_SIZE: usize = 2 * 1024 * 1024;
pub const CONTROL_OFFSET: usize = 0x30002000;
pub const READY: u64 = 0x5944414552564352;

/// Check a stable receiver control publication before modifying its RAM.
pub fn validate_ready(words: &[u64]) -> io::Result<()> {
    if words.len() < 6 || words[0] != READY || words[1] != 0 || words[5] != 0 {
        return Err(invalid());
    }
    Ok(())
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid parked Nitro receiver state",
    )
}

/// Publish a validated kernel entry to the parked receiver.
///
/// # Safety
/// `control` must address six aligned, live u64 words in the receiver reservation.
/// Only the receiver and this host thread may access them; receiver must be parked.
/// All template RAM and fresh device backends must be installed before this call.
pub unsafe fn release(control: *mut u64, entry: ResumeEntry) -> io::Result<()> {
    if control.is_null()
        || !(control as usize).is_multiple_of(8)
        || entry.entry == 0
        || !entry.entry.is_multiple_of(4)
        || entry.gic_count == 0
        || entry.gic_count > crate::cooperative::MAX_GIC_WRITES
        || !entry.gic_list.is_multiple_of(8)
    {
        return Err(invalid());
    }
    let mut state = [0; 6];
    for (index, value) in state.iter_mut().enumerate() {
        *value = unsafe { control.add(index).read_volatile() };
    }
    fence(Ordering::Acquire);
    validate_ready(&state)?;
    unsafe {
        control.add(2).write_volatile(entry.entry);
        control.add(3).write_volatile(entry.gic_list);
        control.add(4).write_volatile(entry.gic_count);
    }
    fence(Ordering::Release);
    unsafe {
        control.add(1).write_volatile(1);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_is_one_shot_and_invalid_state_unchanged() {
        let mut words = [READY, 0, 0, 0, 0, 0];
        let entry = ResumeEntry {
            entry: 0x40001000,
            gic_list: 0x40002000,
            gic_count: 151,
        };
        unsafe { release(words.as_mut_ptr(), entry) }.unwrap();
        assert_eq!(words, [READY, 1, entry.entry, entry.gic_list, 151, 0]);
        assert!(unsafe { release(words.as_mut_ptr(), entry) }.is_err());
        let before = words;
        assert!(unsafe { release(words.as_mut_ptr(), ResumeEntry { entry: 1, ..entry }) }.is_err());
        assert_eq!(words, before);
    }
    #[test]
    fn refuses_progress_and_missing_ready() {
        for state in [[0; 6], [READY, 0, 0, 0, 0, 1], [READY, 1, 0, 0, 0, 0]] {
            assert!(validate_ready(&state).is_err());
        }
    }
}
