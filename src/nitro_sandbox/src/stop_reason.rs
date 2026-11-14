//! Kernel terminal-reason byte with allocation-free virtio notification.

fn consume_notification(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    let mut value = 0u64;
    let result = unsafe { libc::read(fd, (&mut value as *mut u64).cast(), 8) };
    if result == 8 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if result < 0 && error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(())
    } else if result < 0 {
        Err(error)
    } else {
        Err(std::io::Error::other("short stop notification read"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_without_reason_does_not_remain_readable() {
        let event = utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        event.write(1).unwrap();
        consume_notification(event.as_raw_fd()).unwrap();
        let mut poll = libc::pollfd {
            fd: event.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 0);
        consume_notification(event.as_raw_fd()).unwrap();
    }
}
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
    pub fn consume_notification(&self) -> io::Result<()> {
        consume_notification(self.events.kicks[0].as_raw_fd())
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
        (1..=4).contains(&reason).then_some(reason)
    }
}
