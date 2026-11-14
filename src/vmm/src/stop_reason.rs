// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use devices::BusDevice;

/// Privileged stop-intent channel; writing it never stops a guest.
pub const STOP_REASON_PORT: u64 = 0xea;

#[derive(Default)]
pub struct StopReason(pub AtomicU8);

pub struct StopReasonDevice(pub Arc<StopReason>);

impl BusDevice for StopReasonDevice {
    fn read(&mut self, _vcpuid: u64, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, data: &[u8]) {
        if let [reason @ 0..=4] = data {
            // A panic remains sticky even across a crash-kernel handoff.
            let _ = self
                .0
                 .0
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    (old != 4).then_some(*reason)
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_bytes_clear_and_sticky_panic() {
        let reason = Arc::new(StopReason::default());
        let mut device = StopReasonDevice(reason.clone());
        for byte in [1, 2, 3, 0] {
            device.write(0, 0, &[byte]);
            assert_eq!(reason.0.load(Ordering::Acquire), byte);
        }
        for bytes in [&[255][..], &[1, 2][..], &[][..]] {
            device.write(0, 0, bytes);
            assert_eq!(reason.0.load(Ordering::Acquire), 0);
        }
        device.write(0, 0, &[4]);
        for byte in 0..=4 {
            device.write(0, 0, &[byte]);
            assert_eq!(reason.0.load(Ordering::Acquire), 4);
        }
        let mut read = [255];
        device.read(0, 0, &mut read);
        assert_eq!(read, [0]);
    }
}
