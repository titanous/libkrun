//! Kernel terminal-reason byte with allocation-free virtio notification.
use crate::host::{DeviceEvents, Sandbox};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
// Experimental virtio device ID, recognized only by the maintained guest driver.
pub const DEVICE_ID: u32 = 63;
pub struct StopReason {
    events: DeviceEvents,
    address: u64,
}
impl StopReason {
    pub fn register(sandbox: &mut Sandbox) -> io::Result<Self> {
        let region = sandbox.allocate_region(0, crate::INFO_PAGE_SIZE)?;
        let address = region.mapping.address();
        let events = sandbox.add_owned_device_regions(
            DEVICE_ID,
            1,
            1 << 32,
            b"HOLODECK-STOP-v1",
            vec![region],
        )?;
        Ok(Self { events, address })
    }
    pub fn fd(&self) -> RawFd {
        self.events.kicks[0].as_raw_fd()
    }
    pub fn observation_micros(&self) -> Option<u64> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
            return None;
        }
        Some(time.tv_sec as u64 * 1_000_000 + time.tv_nsec as u64 / 1000)
    }
    pub fn reason(&self) -> Option<u8> {
        // Acquire the guest's ordered byte store before its queue notification.
        let reason = unsafe { std::ptr::read_volatile(self.address as *const u8) };
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        (reason != 0).then_some(reason)
    }
}
