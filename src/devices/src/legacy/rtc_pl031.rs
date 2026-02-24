// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! ARM PL031 Real Time Clock
//!
//! This module implements a PL031 Real Time Clock (RTC) that provides to provides long time base counter.
//! This is achieved by generating an interrupt signal after counting for a programmed number of cycles of
//! a real-time clock input.
//!

use std::fmt;
use std::time::{Duration, Instant};
use std::{io, result};

use crate::BusDevice;
use crate::snapshot::{SnapshotError, Snapshottable};
use utils::byte_order;
use utils::eventfd::EventFd;
//use bus::Error;

// As you can see in https://static.docs.arm.com/ddi0224/c/real_time_clock_pl031_r1p3_technical_reference_manual_DDI0224C.pdf
// at section 3.2 Summary of RTC registers, the total size occupied by this device is 0x000 -> 0xFFC + 4 = 0x1000.
// From 0x0 to 0x1C we have following registers:
const RTCDR: u64 = 0x0; // Data Register.
const RTCMR: u64 = 0x4; // Match Register.
const RTCLR: u64 = 0x8; // Load Regiser.
const RTCCR: u64 = 0xc; // Control Register.
const RTCIMSC: u64 = 0x10; // Interrupt Mask Set or Clear Register.
const RTCRIS: u64 = 0x14; // Raw Interrupt Status.
const RTCMIS: u64 = 0x18; // Masked Interrupt Status.
const RTCICR: u64 = 0x1c; // Interrupt Clear Register.
                          // From 0x020 to 0xFDC => reserved space.
                          // From 0xFE0 to 0x1000 => Peripheral and PrimeCell Identification Registers which are Read Only registers.
                          // AMBA standard devices have CIDs (Cell IDs) and PIDs (Peripheral IDs). The linux kernel will look for these in order to assert the identity
                          // of these devices (i.e look at the `amba_device_try_add` function).
                          // We are putting the expected values (look at 'Reset value' column from above mentioned document) in an array.
const PL031_ID: [u8; 8] = [0x31, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];
// We are only interested in the margins.
const AMBA_ID_LOW: u64 = 0xFE0;
const AMBA_ID_HIGH: u64 = 0x1000;

#[derive(Debug)]
pub enum Error {
    BadWriteOffset(u64),
    InterruptFailure(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::BadWriteOffset(offset) => write!(f, "Bad Write Offset: {offset}"),
            Error::InterruptFailure(e) => write!(f, "Failed to trigger interrupt: {e}"),
        }
    }
}
type Result<T> = result::Result<T, Error>;

#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
struct RtcState {
    tick_offset: i64,
    previous_now_elapsed_nanos: u64,
    load: u32,
    match_value: u32,
    imsc: u32,
    ris: u32,
}

/// A RTC device following the PL031 specification..
pub struct RTC {
    previous_now: Instant,
    tick_offset: i64,
    // This is used for implementing the RTC alarm. However, in Firecracker we do not need it.
    match_value: u32,
    // Writes to this register load an update value into the RTC.
    load: u32,
    imsc: u32,
    ris: u32,
    interrupt_evt: EventFd,
}

impl RTC {
    /// Constructs an AMBA PL031 RTC device.
    pub fn new(interrupt_evt: EventFd) -> RTC {
        RTC {
            // This is used only for duration measuring purposes.
            previous_now: Instant::now(),
            tick_offset: utils::time::get_time(utils::time::ClockType::Real) as i64,
            match_value: 0,
            load: 0,
            imsc: 0,
            ris: 0,
            interrupt_evt,
        }
    }

    fn trigger_interrupt(&mut self) -> Result<()> {
        self.interrupt_evt.write(1).map_err(Error::InterruptFailure)
    }

    fn get_time(&self) -> u32 {
        let ts = (self.tick_offset as i128)
            + (Instant::now().duration_since(self.previous_now).as_nanos() as i128);
        (ts / utils::time::NANOS_PER_SECOND as i128) as u32
    }

    fn handle_write(&mut self, offset: u64, val: u32) -> Result<()> {
        match offset {
            RTCMR => {
                // The MR register is used for implementing the RTC alarm. A real time clock alarm is
                // a feature that can be used to allow a computer to 'wake up' after shut down to execute
                // tasks every day or on a certain day. It can sometimes be found in the 'Power Management'
                // section of a motherboard's BIOS setup. This is functionality that extends beyond
                // Firecracker intended use. However, we increment a metric just in case.
                self.match_value = val;
            }
            RTCLR => {
                self.load = val;
                self.previous_now = Instant::now();
                // If the unwrap fails, then the internal value of the clock has been corrupted and
                // we want to terminate the execution of the process.
                self.tick_offset = utils::time::seconds_to_nanoseconds(i64::from(val)).unwrap();
            }
            RTCIMSC => {
                self.imsc = val & 1;
                self.trigger_interrupt()?;
            }
            RTCICR => {
                // As per above mentioned doc, the interrupt is cleared by writing any data value to
                // the Interrupt Clear Register.
                self.ris = 0;
                self.trigger_interrupt()?;
            }
            RTCCR => (), // ignore attempts to turn off the timer.
            o => {
                return Err(Error::BadWriteOffset(o));
            }
        }
        Ok(())
    }
}

impl BusDevice for RTC {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        let mut read_ok = true;

        let v = if (AMBA_ID_LOW..AMBA_ID_HIGH).contains(&offset) {
            let index = ((offset - AMBA_ID_LOW) >> 2) as usize;
            u32::from(PL031_ID[index])
        } else {
            match offset {
                RTCDR => self.get_time(),
                RTCMR => {
                    // Even though we are not implementing RTC alarm we return the last value
                    self.match_value
                }
                RTCLR => self.load,
                RTCCR => 1, // RTC is always enabled.
                RTCIMSC => self.imsc,
                RTCRIS => self.ris,
                RTCMIS => self.ris & self.imsc,
                _ => {
                    read_ok = false;
                    0
                }
            }
        };
        if read_ok && data.len() <= 4 {
            byte_order::write_le_u32(data, v);
        } else {
            warn!(
                "Invalid RTC PL031 read: offset {}, data length {}",
                offset,
                data.len()
            );
        }
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if data.len() <= 4 {
            let v = byte_order::read_le_u32(data);
            if let Err(e) = self.handle_write(offset, v) {
                warn!("Failed to write to RTC PL031 device: {e}");
            }
        } else {
            warn!(
                "Invalid RTC PL031 write: offset {}, data length {}",
                offset,
                data.len()
            );
        }
    }

    fn as_snapshottable(&self) -> Option<&dyn Snapshottable> {
        Some(self)
    }

    fn as_snapshottable_mut(&mut self) -> Option<&mut dyn Snapshottable> {
        Some(self)
    }
}

impl Snapshottable for RTC {
    fn snapshot_id(&self) -> &str {
        "pl031"
    }

    fn save_state(&self) -> std::result::Result<Vec<u8>, SnapshotError> {
        let elapsed = Instant::now()
            .duration_since(self.previous_now)
            .as_nanos() as u64;
        let state = RtcState {
            tick_offset: self.tick_offset,
            previous_now_elapsed_nanos: elapsed,
            load: self.load,
            match_value: self.match_value,
            imsc: self.imsc,
            ris: self.ris,
        };

        #[cfg(feature = "snapshot")]
        {
            bincode::serialize(&state).map_err(|e| SnapshotError::Serialize(e.to_string()))
        }
        #[cfg(not(feature = "snapshot"))]
        {
            let _ = state;
            Err(SnapshotError::Serialize(
                "snapshot feature not enabled".to_string(),
            ))
        }
    }

    fn restore_state(&mut self, data: &[u8]) -> std::result::Result<(), SnapshotError> {
        #[cfg(feature = "snapshot")]
        {
            let state: RtcState = bincode::deserialize(data)
                .map_err(|e| SnapshotError::Deserialize(e.to_string()))?;
            self.previous_now =
                Instant::now() - Duration::from_nanos(state.previous_now_elapsed_nanos);
            self.tick_offset = state.tick_offset;
            self.load = state.load;
            self.match_value = state.match_value;
            self.imsc = state.imsc;
            self.ris = state.ris;
            Ok(())
        }
        #[cfg(not(feature = "snapshot"))]
        {
            let _ = data;
            Err(SnapshotError::Deserialize(
                "snapshot feature not enabled".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rtc_read_write_and_event() {
        let mut rtc = RTC::new(EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap());
        let mut data = [0; 4];

        // Read and write to the MR register.
        byte_order::write_le_u32(&mut data, 123);
        rtc.write(0, RTCMR, &mut data);
        rtc.read(0, RTCMR, &mut data);
        let v = byte_order::read_le_u32(&data[..]);
        assert_eq!(v, 123);

        // Read and write to the LR register.
        let v = utils::time::get_time(utils::time::ClockType::Real);
        byte_order::write_le_u32(&mut data, (v / utils::time::NANOS_PER_SECOND) as u32);
        let previous_now_before = rtc.previous_now;
        rtc.write(0, RTCLR, &mut data);

        assert!(rtc.previous_now > previous_now_before);

        rtc.read(0, RTCLR, &mut data);
        let v_read = byte_order::read_le_u32(&data[..]);
        assert_eq!((v / utils::time::NANOS_PER_SECOND) as u32, v_read);

        // Read and write to IMSC register.
        // Test with non zero value.
        let non_zero = 1;
        byte_order::write_le_u32(&mut data, non_zero);
        rtc.write(0, RTCIMSC, &mut data);
        // The interrupt line should be on.
        assert!(rtc.interrupt_evt.read().unwrap() == 1);
        rtc.read(0, RTCIMSC, &mut data);
        let v = byte_order::read_le_u32(&data[..]);
        assert_eq!(non_zero & 1, v);

        // Now test with 0.
        byte_order::write_le_u32(&mut data, 0);
        rtc.write(0, RTCIMSC, &mut data);
        rtc.read(0, RTCIMSC, &mut data);
        let v = byte_order::read_le_u32(&data[..]);
        assert_eq!(0, v);

        // Attempts to turn off the RTC should not go through.
        byte_order::write_le_u32(&mut data, 0);
        rtc.write(0, RTCCR, &mut data);
        rtc.read(0, RTCCR, &mut data);
        let v = byte_order::read_le_u32(&data[..]);
        assert_eq!(v, 1);

        let mut data = [0; 4];
        rtc.read(0, AMBA_ID_LOW, &mut data);
        let index = AMBA_ID_LOW + 3;
        assert_eq!(data[0], PL031_ID[((index - AMBA_ID_LOW) >> 2) as usize]);
    }

    #[test]
    #[cfg(feature = "snapshot")]
    fn test_rtc_snapshot_preserves_registers() {
        let mut rtc = RTC::new(EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap());
        let mut data = [0; 4];

        // Set up non-default register values
        // Write to RTCMR (match value)
        byte_order::write_le_u32(&mut data, 0x12345678);
        rtc.write(0, RTCMR, &data);

        // Write to RTCLR (load value)
        let load_value = 0x11223344u32;
        byte_order::write_le_u32(&mut data, load_value);
        rtc.write(0, RTCLR, &data);

        // Write to RTCIMSC (interrupt mask)
        byte_order::write_le_u32(&mut data, 1);
        rtc.write(0, RTCIMSC, &data);

        // Verify initial state
        rtc.read(0, RTCMR, &mut data);
        let mr = byte_order::read_le_u32(&data);
        assert_eq!(mr, 0x12345678);

        rtc.read(0, RTCLR, &mut data);
        let lr = byte_order::read_le_u32(&data);
        assert_eq!(lr, load_value);

        rtc.read(0, RTCIMSC, &mut data);
        let imsc = byte_order::read_le_u32(&data);
        assert_eq!(imsc, 1);

        // Save state
        let saved_state = rtc.save_state().expect("Failed to save RTC state");

        // Create a fresh RTC and restore
        let mut rtc_restored = RTC::new(EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap());
        rtc_restored
            .restore_state(&saved_state)
            .expect("Failed to restore RTC state");

        // Verify registers after restore
        rtc_restored.read(0, RTCMR, &mut data);
        let mr_restored = byte_order::read_le_u32(&data);
        assert_eq!(mr_restored, 0x12345678, "Match value not preserved");

        rtc_restored.read(0, RTCLR, &mut data);
        let lr_restored = byte_order::read_le_u32(&data);
        assert_eq!(lr_restored, load_value, "Load value not preserved");

        rtc_restored.read(0, RTCIMSC, &mut data);
        let imsc_restored = byte_order::read_le_u32(&data);
        assert_eq!(imsc_restored, 1, "IMSC not preserved");

        // Verify get_time() returns a consistent value (within tolerance)
        let time_before_save = rtc.get_time();
        let time_after_restore = rtc_restored.get_time();
        // Allow some tolerance for test execution time (a few seconds)
        let time_diff = (time_after_restore as i64 - time_before_save as i64).abs();
        assert!(time_diff < 5, "Time difference too large: {}", time_diff);
    }
}
