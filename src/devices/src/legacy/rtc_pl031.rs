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

use crate::snapshot::{SnapshotError, Snapshottable};
use crate::BusDevice;
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
        let elapsed = Instant::now().duration_since(self.previous_now).as_nanos() as u64;
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

#[cfg(kani)]
mod verification {
    use super::*;

    // ── Instant::now() stub ───────────────────────────────────────────────────
    //
    // `Instant::now()` reads the wall clock, which is opaque to Kani.
    //
    // Following the Firecracker pattern (rate_limiter/mod.rs), we transmute a
    // repr(C) struct matching the Linux/Rust `Instant` memory layout into an
    // `Instant`.  The stub always returns the epoch (tv_sec=0, tv_nsec=0), so
    // `duration_since(same_stub_value)` yields Duration::ZERO and all elapsed-
    // time arithmetic becomes deterministic.
    //
    // Note: The Rust `Instant` struct is repr(Rust), so transmute is technically
    // unsound in general.  Kani does not run LLVM optimisations; it transpiles
    // unoptimised MIR to goto-programs, so field order is stable and the
    // transmute works correctly in verification context.

    #[repr(C)]
    struct InstantStub {
        tv_sec: i64,
        tv_nsec: u32,
    }

    /// Stub for `Instant::now` — always returns the epoch instant.
    ///
    /// When `#[kani::stub(std::time::Instant::now, mock_instant_now)]` is
    /// applied to a proof harness, every call to `Instant::now()` inside the
    /// harness and all code it invokes will return this fixed value.
    fn mock_instant_now() -> Instant {
        // SAFETY: Kani never optimises MIR; the memory layout of Instant on
        // Linux (libc timespec) matches InstantStub.  Equivalent to the pattern
        // used in Firecracker's rate_limiter verification.
        unsafe {
            std::mem::transmute(InstantStub {
                tv_sec: 0,
                tv_nsec: 0,
            })
        }
    }

    /// Stub for `EventFd::write` — always returns Ok(()).
    ///
    /// `trigger_interrupt` calls `self.interrupt_evt.write(1)`.  In Kani model
    /// checking there is no real OS, so we replace the write with a no-op stub.
    /// The stub path is qualified through the crate that owns EventFd; on Linux
    /// this is `vmm_sys_util`.
    fn mock_eventfd_write(_evt: &utils::eventfd::EventFd, _v: u64) -> std::io::Result<()> {
        Ok(())
    }

    // ── Helper: construct an RTC for Kani ─────────────────────────────────────
    //
    // We build the struct directly to avoid the `EventFd::new` syscall.
    // `previous_now` is set via the stub so that `Instant::now() - previous_now`
    // in `get_time` yields zero elapsed time, making arithmetic fully symbolic.

    fn make_rtc_for_kani() -> RTC {
        // SAFETY: same as mock_instant_now — stable MIR-level transmute.
        let instant_zero: Instant = unsafe {
            std::mem::transmute(InstantStub {
                tv_sec: 0,
                tv_nsec: 0,
            })
        };

        // Build a minimal EventFd-like wrapper.  We give it fd=3 (stdin is 0,
        // stdout 1, stderr 2); the fd is never actually used in these proofs
        // because trigger_interrupt is either stubbed or not called.
        // SAFETY: from_raw_fd is unsafe because closing fd=3 would be wrong at
        // runtime, but in Kani's model-checking execution nothing closes fds.
        let evt = unsafe {
            use std::os::unix::io::FromRawFd;
            utils::eventfd::EventFd::from_raw_fd(3)
        };

        RTC {
            previous_now: instant_zero,
            tick_offset: kani::any(),
            match_value: kani::any(),
            load: kani::any(),
            imsc: kani::any(),
            ris: kani::any(),
            interrupt_evt: evt,
        }
    }

    // ── Register write semantics ──────────────────────────────────────────────

    /// Proof: writing to RTCMR stores the value unchanged.
    ///
    /// Spec: PL031 §3.3.2 — match register is written directly.
    /// RTCMR does not touch Instant::now() or the EventFd.
    #[kani::proof]
    fn proof_rtcmr_write_stores_value() {
        let mut rtc = make_rtc_for_kani();
        let val: u32 = kani::any();
        let result = rtc.handle_write(RTCMR, val);
        kani::assert(result.is_ok(), "RTCMR write must succeed");
        kani::assert(rtc.match_value == val, "RTCMR write must store value");
        kani::cover!(true, "RTCMR write path reachable");
    }

    /// Proof: writing to RTCIMSC masks the value to bit 0.
    ///
    /// Spec: PL031 §3.3.5 — only bit 0 of RTCIMSC is implemented; upper bits
    /// are read-as-zero.  The mask `val & 1` enforces this invariant.
    /// The stub replaces EventFd::write so trigger_interrupt can complete.
    #[kani::proof]
    #[kani::stub(vmm_sys_util::eventfd::EventFd::write, mock_eventfd_write)]
    fn proof_rtcimsc_masks_to_bit0() {
        let mut rtc = make_rtc_for_kani();
        let val: u32 = kani::any();
        let _ = rtc.handle_write(RTCIMSC, val);
        kani::assert(rtc.imsc == val & 1, "RTCIMSC must be masked to bit 0");
        kani::cover!(true, "RTCIMSC mask path reachable");
    }

    /// Proof: writing to RTCICR clears ris to zero.
    ///
    /// Spec: PL031 §3.3.8 — writing any value to ICR clears the interrupt.
    #[kani::proof]
    #[kani::stub(vmm_sys_util::eventfd::EventFd::write, mock_eventfd_write)]
    fn proof_rtcicr_clears_ris() {
        let mut rtc = make_rtc_for_kani();
        let val: u32 = kani::any();
        let _ = rtc.handle_write(RTCICR, val);
        kani::assert(rtc.ris == 0, "RTCICR write must clear ris to zero");
        kani::cover!(true, "RTCICR clear path reachable");
    }

    /// Proof: writing to RTCCR is a no-op (RTC is always enabled).
    ///
    /// Spec: PL031 §3.3.4 — the RTC cannot be disabled; RTCCR writes are ignored.
    #[kani::proof]
    fn proof_rtccr_write_is_noop() {
        let mut rtc = make_rtc_for_kani();
        let before_match = rtc.match_value;
        let before_load = rtc.load;
        let before_imsc = rtc.imsc;
        let before_ris = rtc.ris;
        let val: u32 = kani::any();
        let result = rtc.handle_write(RTCCR, val);
        kani::assert(result.is_ok(), "RTCCR write must succeed");
        kani::assert(
            rtc.match_value == before_match,
            "match_value unchanged by RTCCR",
        );
        kani::assert(rtc.load == before_load, "load unchanged by RTCCR");
        kani::assert(rtc.imsc == before_imsc, "imsc unchanged by RTCCR");
        kani::assert(rtc.ris == before_ris, "ris unchanged by RTCCR");
        kani::cover!(true, "RTCCR noop path reachable");
    }

    /// Proof: writing to an unrecognized offset returns BadWriteOffset.
    ///
    /// Valid write offsets: RTCMR(0x4), RTCLR(0x8), RTCIMSC(0x10),
    /// RTCICR(0x1c), RTCCR(0xc).  RTCLR is excluded here because it also
    /// calls `Instant::now()` (verified separately).  Any other offset must
    /// return `Err(BadWriteOffset(_))`.
    #[kani::proof]
    fn proof_unknown_write_offset_returns_error() {
        let mut rtc = make_rtc_for_kani();
        let offset: u64 = kani::any_where(|&o| {
            o != RTCMR && o != RTCLR && o != RTCIMSC && o != RTCICR && o != RTCCR
        });
        let val: u32 = kani::any();
        let result = rtc.handle_write(offset, val);
        kani::assert(result.is_err(), "unknown offset must return an error");
        kani::assert(
            matches!(result, Err(Error::BadWriteOffset(_))),
            "error kind must be BadWriteOffset",
        );
        kani::cover!(true, "bad write offset path reachable");
    }

    /// Proof: RTCLR write stores the load value and updates tick_offset.
    ///
    /// The RTCLR branch calls `Instant::now()` (stubbed) and
    /// `seconds_to_nanoseconds(i64::from(val)).unwrap()`.  Because `val` is a
    /// `u32`, the value is always in [0, u32::MAX] ≤ 4.29e9 seconds, well below
    /// the overflow threshold (~9.22e9 s), so the `unwrap()` never panics.
    #[kani::proof]
    #[kani::stub(std::time::Instant::now, mock_instant_now)]
    fn proof_rtclr_write_stores_load_and_sets_tick_offset() {
        let mut rtc = make_rtc_for_kani();
        let val: u32 = kani::any();
        let result = rtc.handle_write(RTCLR, val);
        kani::assert(result.is_ok(), "RTCLR write must succeed");
        kani::assert(rtc.load == val, "RTCLR must store val in load field");
        // tick_offset = seconds_to_nanoseconds(val as i64).unwrap()
        // = val * 1_000_000_000 (always fits in i64 for u32 values)
        let expected_tick_offset = i64::from(val) * (utils::time::NANOS_PER_SECOND as i64);
        kani::assert(
            rtc.tick_offset == expected_tick_offset,
            "RTCLR must set tick_offset to val * NANOS_PER_SECOND",
        );
        kani::cover!(true, "RTCLR write path reachable");
    }

    // ── Register read semantics ───────────────────────────────────────────────

    /// Proof: RTCMIS = ris & imsc (masked interrupt status).
    ///
    /// Spec: PL031 §3.3.7 — the MIS register is the bitwise AND of RIS and IMSC.
    /// Verifies that masking preserves only the bits enabled by IMSC.
    #[kani::proof]
    fn proof_rtcmis_equals_ris_and_imsc() {
        let ris: u32 = kani::any();
        let imsc: u32 = kani::any();
        let mis = ris & imsc;
        // Any bit set in MIS must also be set in both RIS and IMSC.
        kani::assert(mis & !ris == 0, "MIS must not have bits absent from RIS");
        kani::assert(mis & !imsc == 0, "MIS must not have bits absent from IMSC");
        // If both RIS and IMSC have a bit set, MIS must too.
        kani::assert(mis == ris & imsc, "MIS must equal bitwise AND");
        kani::cover!(mis != 0, "non-zero MIS reachable");
        kani::cover!(mis == 0 && ris != 0, "masked-out interrupt reachable");
    }

    /// Proof: AMBA ID reads are always in-bounds.
    ///
    /// For any offset in [AMBA_ID_LOW, AMBA_ID_HIGH), the computed index is
    /// `(offset - AMBA_ID_LOW) >> 2`, which must be < 8 (= PL031_ID.len()).
    ///
    /// AMBA_ID_LOW = 0xFE0, AMBA_ID_HIGH = 0x1000 → 32 bytes / 4 = 8 slots.
    /// Maximum offset = 0xFFC → index = (0xFFC - 0xFE0) >> 2 = 0x1C >> 2 = 7.
    #[kani::proof]
    fn proof_amba_id_index_in_bounds() {
        let offset: u64 = kani::any_where(|&o| o >= AMBA_ID_LOW && o < AMBA_ID_HIGH);
        let index = ((offset - AMBA_ID_LOW) >> 2) as usize;
        kani::assert(index < PL031_ID.len(), "AMBA ID index must be < 8");
        let _ = PL031_ID[index]; // must not panic
        kani::cover!(true, "AMBA ID read path reachable");
    }

    // ── get_time arithmetic ───────────────────────────────────────────────────

    /// Proof: get_time arithmetic never panics for any tick_offset value.
    ///
    /// With the Instant stub, `Instant::now() - previous_now` yields
    /// Duration::ZERO, so the arithmetic simplifies to:
    ///   ts = tick_offset as i128
    ///   result = (ts / NANOS_PER_SECOND) as u32
    /// Both the i128 cast and the `as u32` truncation are infallible in Rust.
    #[kani::proof]
    #[kani::stub(std::time::Instant::now, mock_instant_now)]
    fn proof_get_time_no_panic() {
        let mut rtc = make_rtc_for_kani();
        // get_time() uses Instant::now() (stubbed) and tick_offset (symbolic).
        // With stub returning the same instant as previous_now, elapsed = 0.
        let _ = rtc.get_time();
        kani::cover!(true, "get_time no-panic path reachable");
    }

    // ── seconds_to_nanoseconds overflow proof ─────────────────────────────────

    /// Proof: `seconds_to_nanoseconds(i64::from(u32))` never overflows.
    ///
    /// The RTCLR write path calls `.unwrap()` on the result.  This proof
    /// establishes that for any `u32` input, the product always fits in `i64`.
    ///
    /// Arithmetic: u32::MAX * 1_000_000_000 = 4_294_967_295_000_000_000
    ///             i64::MAX                  = 9_223_372_036_854_775_807
    /// The product is less than i64::MAX, so `checked_mul` always returns Some.
    #[kani::proof]
    fn proof_seconds_to_nanoseconds_u32_never_overflows() {
        let val: u32 = kani::any();
        let result = utils::time::seconds_to_nanoseconds(i64::from(val));
        kani::assert(
            result.is_some(),
            "seconds_to_nanoseconds(u32 as i64) must never overflow",
        );
        kani::cover!(true, "no-overflow path reachable");
    }
}
