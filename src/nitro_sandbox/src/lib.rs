//! Host-side protocol primitives for the Nitro Sandbox hypervisor.
//! This is distinct from running a workload inside a confidential enclave.

#[cfg(target_os = "linux")]
pub mod host;
#[cfg(all(target_os = "linux", feature = "devices"))]
pub mod runtime;
#[cfg(all(target_os = "linux", feature = "devices"))]
pub mod transport;

use std::time::Duration;

pub const INFO_PAGE_SIZE: usize = 2 * 1024 * 1024;
const ENTRY_SIZE: usize = 128 + 2216;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    InvalidHeader,
    InvalidQueue,
    UnsupportedFeatures,
}

fn bytes<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N], Error> {
    data.get(offset..offset.checked_add(N).ok_or(Error::Truncated)?)
        .ok_or(Error::Truncated)?
        .try_into()
        .map_err(|_| Error::Truncated)
}
fn u16_at(data: &[u8], at: usize) -> Result<u16, Error> {
    Ok(u16::from_le_bytes(bytes(data, at)?))
}
fn u32_at(data: &[u8], at: usize) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(bytes(data, at)?))
}
fn u64_at(data: &[u8], at: usize) -> Result<u64, Error> {
    Ok(u64::from_le_bytes(bytes(data, at)?))
}

/// One split virtqueue as published by the hypervisor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Queue {
    pub size: u16,
    pub enabled: bool,
    pub descriptor: u64,
    pub available: u64,
    pub used: u64,
}

impl Queue {
    /// Check geometry before constructing the device's queue. The range predicate
    /// must accept only contiguous guest RAM, never an MMIO or shared-device window.
    pub fn validate(&self, maximum: u16, contains: impl Fn(u64, u64) -> bool) -> Result<(), Error> {
        let n = u64::from(self.size);
        if !self.enabled
            || self.size == 0
            || !self.size.is_power_of_two()
            || self.size > maximum
            || self.descriptor % 16 != 0
            || self.available % 2 != 0
            || self.used % 4 != 0
        {
            return Err(Error::InvalidQueue);
        }
        // Include the optional event fields even when EVENT_IDX is not negotiated.
        for (address, length) in [
            (self.descriptor, 16 * n),
            (self.available, 6 + 2 * n),
            (self.used, 6 + 8 * n),
        ] {
            if address.checked_add(length).is_none() || !contains(address, length) {
                return Err(Error::InvalidQueue);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceSnapshot {
    pub reset_count: u64,
    pub generation: u32,
    pub status: u8,
    pub negotiated_features: u64,
    pub device_type: u16,
    pub uid: u64,
    pub queues: Vec<Queue>,
}

/// Parse a stable copy, not a live hypervisor mapping. Extended negotiated
/// features are rejected because libkrun's device interface supports only 64 bits.
pub fn device_snapshot(page: &[u8], uid: u64) -> Result<Option<DeviceSnapshot>, Error> {
    if page.len() > INFO_PAGE_SIZE || u32_at(page, 0)? != 0x5343_494e {
        return Err(Error::InvalidHeader);
    }
    let count = u32_at(page, 8)? as usize;
    if page.len() < 16 || count > (page.len() - 16) / 48 {
        return Err(Error::InvalidHeader);
    }
    for i in 0..count {
        let toc = 16 + i * 48;
        let offset = u32_at(page, toc + 4)? as usize;
        let size = usize::try_from(u64_at(page, toc + 8)?).map_err(|_| Error::Truncated)?;
        let end = offset.checked_add(size).ok_or(Error::Truncated)?;
        if end > page.len() {
            return Err(Error::Truncated);
        }
        if u16_at(page, toc)? != 2 || u64_at(page, toc + 16)? != uid {
            continue;
        }
        if size < ENTRY_SIZE {
            return Err(Error::Truncated);
        }
        let entry = &page[offset..end];
        let mirror = &entry[128..];
        let n = usize::from(u16_at(mirror, 18)?);
        if n > 64 {
            return Err(Error::InvalidQueue);
        }
        if mirror[2160..2216].iter().any(|b| *b != 0) {
            return Err(Error::UnsupportedFeatures);
        }
        if u64_at(mirror, 2096)? != uid {
            return Err(Error::InvalidHeader);
        }
        let mut queues = Vec::with_capacity(n);
        for q in 0..n {
            let at = 24 + q * 32;
            queues.push(Queue {
                size: u16_at(mirror, at)?,
                enabled: u16_at(mirror, at + 4)? != 0,
                descriptor: u64_at(mirror, at + 8)?,
                available: u64_at(mirror, at + 16)?,
                used: u64_at(mirror, at + 24)?,
            });
        }
        return Ok(Some(DeviceSnapshot {
            reset_count: u64_at(entry, 0)?,
            generation: u32_at(entry, 8)?,
            status: mirror[20],
            negotiated_features: u64_at(mirror, 2080)?,
            device_type: u16_at(mirror, 2088)?,
            uid,
            queues,
        }));
    }
    Ok(None)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Start,
    Stop,
}

/// A reset produces Stop even if the same sample is already DRIVER_OK again.
/// The caller must finish Stop, then observe a fresh sample to restart.
#[derive(Default, Debug)]
pub struct DeviceLifecycle {
    active_reset: Option<u64>,
}
impl DeviceLifecycle {
    pub fn observe(&mut self, snapshot: &DeviceSnapshot, offered: u64) -> Result<Action, Error> {
        let ready = snapshot.status & 15 == 15 && snapshot.status & (64 | 128) == 0;
        if let Some(reset) = self.active_reset {
            if !ready || reset != snapshot.reset_count {
                self.active_reset = None;
                return Ok(Action::Stop);
            }
            return Ok(Action::None);
        }
        if !ready {
            return Ok(Action::None);
        }
        if snapshot.negotiated_features & !offered != 0 {
            return Err(Error::UnsupportedFeatures);
        }
        self.active_reset = Some(snapshot.reset_count);
        Ok(Action::Start)
    }
}

/// Bounds initial boot and the interval after all devices reset. Ordinary idle
/// guests do not expire; the timeout is not a device-I/O inactivity timeout.
#[derive(Debug)]
pub struct Watchdog {
    deadline: Option<Duration>,
    timeout: Duration,
}
impl Watchdog {
    pub fn new(now: Duration, timeout: Duration) -> Self {
        Self {
            deadline: Some(now.saturating_add(timeout)),
            timeout,
        }
    }
    pub fn devices_ready(&mut self) {
        self.deadline = None;
    }
    pub fn devices_reset(&mut self, now: Duration) {
        if self.deadline.is_none() {
            self.deadline = Some(now.saturating_add(self.timeout));
        }
    }
    pub fn expired(&self, now: Duration) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> DeviceSnapshot {
        DeviceSnapshot {
            reset_count: 0,
            generation: 1,
            status: 15,
            negotiated_features: 1 << 32,
            device_type: 2,
            uid: 7,
            queues: vec![],
        }
    }
    #[test]
    fn resets_stop_before_reactivation() {
        let mut state = DeviceLifecycle::default();
        let mut s = snapshot();
        assert_eq!(state.observe(&s, 1 << 32), Ok(Action::Start));
        assert_eq!(state.observe(&s, 1 << 32), Ok(Action::None));
        s.reset_count += 1;
        assert_eq!(state.observe(&s, 1 << 32), Ok(Action::Stop));
        assert_eq!(state.observe(&s, 1 << 32), Ok(Action::Start));
        s.status = 0;
        assert_eq!(state.observe(&s, 1 << 32), Ok(Action::Stop));
        assert_eq!(state.observe(&s, 1 << 32), Ok(Action::None));
    }
    #[test]
    fn rejects_unoffered_features_and_failed_status() {
        let mut state = DeviceLifecycle::default();
        let mut s = snapshot();
        assert_eq!(state.observe(&s, 0), Err(Error::UnsupportedFeatures));
        s.status |= 128;
        assert_eq!(state.observe(&s, 0), Ok(Action::None));
    }
    #[test]
    fn watchdog_boot_reset_and_idle() {
        let mut w = Watchdog::new(Duration::ZERO, Duration::from_secs(5));
        assert!(!w.expired(Duration::from_secs(4)));
        assert!(w.expired(Duration::from_secs(5)));
        w.devices_ready();
        assert!(!w.expired(Duration::from_secs(500)));
        w.devices_reset(Duration::from_secs(500));
        w.devices_reset(Duration::from_secs(504));
        assert!(w.expired(Duration::from_secs(505)));
    }
    #[test]
    fn queue_geometry_and_overflow() {
        let mut q = Queue {
            size: 256,
            enabled: true,
            descriptor: 0x40000000,
            available: 0x40001000,
            used: 0x40002000,
        };
        let contains = |a: u64, n: u64| {
            a >= 0x40000000 && a.checked_add(n).is_some_and(|end| end <= 0x50000000)
        };
        assert_eq!(q.validate(256, contains), Ok(()));
        q.size = 257;
        assert_eq!(q.validate(512, contains), Err(Error::InvalidQueue));
        q.size = 256;
        q.descriptor = u64::MAX - 15;
        assert_eq!(q.validate(256, contains), Err(Error::InvalidQueue));
    }
    #[test]
    fn parses_wire_layout_and_checks_bounds() {
        let mut p = vec![0; 4096];
        p[0..4].copy_from_slice(&0x5343494eu32.to_le_bytes());
        p[8..12].copy_from_slice(&1u32.to_le_bytes());
        p[16..18].copy_from_slice(&2u16.to_le_bytes());
        p[20..24].copy_from_slice(&128u32.to_le_bytes());
        p[24..32].copy_from_slice(&(ENTRY_SIZE as u64).to_le_bytes());
        p[32..40].copy_from_slice(&7u64.to_le_bytes());
        let m = 256;
        p[m + 20] = 15;
        p[m + 2080..m + 2088].copy_from_slice(&(1u64 << 32).to_le_bytes());
        p[m + 2088..m + 2090].copy_from_slice(&2u16.to_le_bytes());
        p[m + 2096..m + 2104].copy_from_slice(&7u64.to_le_bytes());
        let s = device_snapshot(&p, 7).unwrap().unwrap();
        assert_eq!(s.status, 15);
        assert_eq!(s.negotiated_features, 1 << 32);
        assert_eq!(device_snapshot(&p, 8), Ok(None));
        p[m + 2160] = 1;
        assert_eq!(device_snapshot(&p, 7), Err(Error::UnsupportedFeatures));
        p[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(device_snapshot(&p, 7), Err(Error::Truncated));
    }
    #[test]
    fn short_pages_never_panic() {
        for length in 0..4096 {
            let _ = device_snapshot(&vec![0; length], 0);
        }
    }
}
