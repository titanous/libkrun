// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use devices::BusDevice;

use crate::vm_exit::VmExit;

/// Dedicated privileged guest stop-intent channel; not a stop command.
pub const STOP_REASON_PORT: u64 = 0xe8;

#[derive(Default)]
pub struct StopReason(pub AtomicU8);

impl StopReason {
    pub fn classify(&self, fallback: VmExit) -> VmExit {
        if matches!(fallback, VmExit::Error { .. }) {
            return fallback;
        }
        match self.0.load(Ordering::Acquire) {
            1 => VmExit::PowerOffRequested,
            2 => VmExit::RebootRequested,
            3 => VmExit::Halted,
            4 => VmExit::Panic,
            _ => fallback,
        }
    }
}

pub struct StopReasonDevice(pub Arc<StopReason>);

/// Read-only duplicate of a vCPU descriptor; never used for KVM_RUN.
pub struct CpuState(pub std::os::fd::OwnedFd);

pub fn halt_timer() -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: timerfd_create returns a new owned descriptor on success.
    let raw = unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
    let interval = libc::timespec {
        tv_sec: 0,
        tv_nsec: 10_000_000,
    };
    let timer = libc::itimerspec {
        it_interval: interval,
        it_value: interval,
    };
    // SAFETY: timer is initialized and the descriptor is a live timerfd.
    if unsafe { libc::timerfd_settime(fd.as_raw_fd(), 0, &timer, std::ptr::null_mut()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

impl CpuState {
    pub fn terminally_halted(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut state = kvm_bindings::kvm_mp_state::default();
        let mut regs = kvm_bindings::kvm_regs::default();
        // SAFETY: both requests receive writable structures of the UAPI size.
        // The duplicate descriptor stays owned for the duration of each ioctl.
        unsafe {
            libc::ioctl(self.0.as_raw_fd(), 0x8004_ae98u32 as _, &mut state) == 0
                && state.mp_state == kvm_bindings::KVM_MP_STATE_HALTED
                && libc::ioctl(self.0.as_raw_fd(), 0x8090_ae81u32 as _, &mut regs) == 0
                && regs.rflags & (1 << 9) == 0
        }
    }
}

impl BusDevice for StopReasonDevice {
    fn read(&mut self, _vcpuid: u64, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, data: &[u8]) {
        if let [reason @ 0..=4] = data {
            self.0 .0.store(*reason, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_accepts_only_valid_byte_writes() {
        let reason = Arc::new(StopReason::default());
        let mut device = StopReasonDevice(reason.clone());
        device.write(0, 0, &[4]);
        device.write(0, 0, &[9]);
        device.write(0, 0, &[1, 2]);
        device.write(0, 0, &[]);
        assert_eq!(reason.classify(VmExit::Fault), VmExit::Panic);
        device.write(0, 0, &[2]);
        assert_eq!(reason.classify(VmExit::Fault), VmExit::RebootRequested);
        let mut data = [255];
        device.read(0, 0, &mut data);
        assert_eq!(data, [0]);
        let error = VmExit::Error {
            message: "host failure".into(),
        };
        assert_eq!(reason.classify(error.clone()), error);
    }

    #[test]
    fn classifies_last_reason_only_when_stopped() {
        let reason = StopReason::default();
        let fault = VmExit::Fault;
        assert_eq!(reason.classify(fault.clone()), fault);
        for (code, expected) in [
            (1, VmExit::PowerOffRequested),
            (2, VmExit::RebootRequested),
            (3, VmExit::Halted),
            (4, VmExit::Panic),
            (0, VmExit::Fault),
        ] {
            reason.0.store(code, Ordering::Release);
            assert_eq!(reason.classify(VmExit::Fault), expected);
        }
    }
}
