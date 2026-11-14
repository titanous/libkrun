// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use devices::BusDevice;

/// Privileged stop-intent channel; writing it never stops a guest.
pub const STOP_REASON_PORT: u64 = 0xea;

#[derive(Default)]
pub struct StopReason(
    pub AtomicU8,
    std::sync::Mutex<Vec<Arc<devices::virtio::console::ConsoleTxState>>>,
);

impl StopReason {
    pub fn register_console(&self, state: Arc<devices::virtio::console::ConsoleTxState>) {
        self.1.lock().unwrap().push(state);
    }
}

pub struct StopReasonDevice(pub Arc<StopReason>);

impl BusDevice for StopReasonDevice {
    fn read(&mut self, _vcpuid: u64, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, data: &[u8]) {
        if data == [5] {
            // Preparation changes console backpressure, never terminal classification.
            for state in self.0 .1.lock().unwrap().iter() {
                state.set_ending(true);
            }
        }
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
        let console = Arc::new(devices::virtio::console::ConsoleTxState::default());
        reason.register_console(console);
        device.write(0, 0, &[5]);
        assert_eq!(reason.0.load(Ordering::Acquire), 0);
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
