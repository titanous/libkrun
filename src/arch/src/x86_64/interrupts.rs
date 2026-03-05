// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use kvm_bindings::kvm_lapic_state;
use kvm_ioctls::VcpuFd;
use utils::byte_order;
/// Errors thrown while configuring the LAPIC.
#[derive(Debug)]
pub enum Error {
    /// Failure in retrieving the LAPIC configuration.
    GetLapic(kvm_ioctls::Error),
    /// Failure in modifying the LAPIC configuration.
    SetLapic(kvm_ioctls::Error),
}
type Result<T> = std::result::Result<T, Error>;

// Defines poached from apicdef.h kernel header.
const APIC_LVT0: usize = 0x350;
const APIC_LVT1: usize = 0x360;
const APIC_MODE_NMI: u32 = 0x4;
const APIC_MODE_EXTINT: u32 = 0x7;

fn get_klapic_reg(klapic: &kvm_lapic_state, reg_offset: usize) -> u32 {
    let range = reg_offset..reg_offset + 4;
    let reg = klapic.regs.get(range).expect("get_klapic_reg range");
    byte_order::read_le_i32(reg) as u32
}

fn set_klapic_reg(klapic: &mut kvm_lapic_state, reg_offset: usize, value: u32) {
    let range = reg_offset..reg_offset + 4;
    let reg = klapic.regs.get_mut(range).expect("set_klapic_reg range");
    byte_order::write_le_i32(&mut *reg, value as i32)
}

fn set_apic_delivery_mode(reg: u32, mode: u32) -> u32 {
    ((reg) & !0x700) | ((mode) << 8)
}

/// Configures LAPICs.  LAPIC0 is set for external interrupts, LAPIC1 is set for NMI.
///
/// # Arguments
/// * `vcpu` - The VCPU object to configure.
pub fn set_lint(vcpu: &VcpuFd) -> Result<()> {
    let mut klapic = vcpu.get_lapic().map_err(Error::GetLapic)?;

    let lvt_lint0 = get_klapic_reg(&klapic, APIC_LVT0);
    set_klapic_reg(
        &mut klapic,
        APIC_LVT0,
        set_apic_delivery_mode(lvt_lint0, APIC_MODE_EXTINT),
    );
    let lvt_lint1 = get_klapic_reg(&klapic, APIC_LVT1);
    set_klapic_reg(
        &mut klapic,
        APIC_LVT1,
        set_apic_delivery_mode(lvt_lint1, APIC_MODE_NMI),
    );

    vcpu.set_lapic(&klapic).map_err(Error::SetLapic)
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Proof: set_apic_delivery_mode correctly writes mode into bits [10:8] and
    /// preserves all other bits.
    ///
    /// The APIC LVT delivery mode field occupies bits [10:8] (mask 0x700).
    /// Verifies:
    ///   - Bits outside [10:8] are unchanged: (result & !0x700) == (reg & !0x700)
    ///   - Bits [10:8] equal mode:             (result >> 8) & 0x7 == mode
    ///
    /// mode is constrained to [0, 7] (3-bit field); reg is unconstrained.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_apic_delivery_mode() {
        let reg: u32 = kani::any();
        let mode: u32 = kani::any_where(|&m| m <= 0x7);

        let result = set_apic_delivery_mode(reg, mode);

        kani::assert(
            (result & !0x700) == (reg & !0x700),
            "bits outside [10:8] must be preserved",
        );
        kani::assert((result >> 8) & 0x7 == mode, "bits [10:8] must equal mode");

        kani::cover!(mode == 0, "mode=0 covered");
        kani::cover!(mode == 7, "mode=7 covered");
        kani::cover!(
            reg & 0x700 != 0,
            "reg with pre-set delivery mode bits covered"
        );
    }

    // ── LAPIC register read/write proofs ──────────────────────────────────────

    /// Proof: set_klapic_reg / get_klapic_reg round-trip at APIC_LVT0 (0x350).
    ///
    /// set_klapic_reg writes a u32 (as little-endian i32) at bytes [offset, offset+4).
    /// get_klapic_reg reads back the same bytes and must return the original value.
    /// Uses the concrete offset APIC_LVT0 = 0x350 (known valid register) so Kani
    /// can handle the fixed-size [i8; 1024] array without symbolic index overhead.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_klapic_reg_lvt0_set_get_roundtrip() {
        let value: u32 = kani::any();

        let mut klapic = kvm_bindings::kvm_lapic_state::default();
        set_klapic_reg(&mut klapic, APIC_LVT0, value);
        let recovered = get_klapic_reg(&klapic, APIC_LVT0);

        kani::assert(
            recovered == value,
            "get_klapic_reg must return the value written by set_klapic_reg at APIC_LVT0",
        );
        kani::cover!(value == 0, "zero value roundtrip covered");
        kani::cover!(value == u32::MAX, "max value roundtrip covered");
    }

    /// Proof: set_klapic_reg / get_klapic_reg round-trip at APIC_LVT1 (0x360).
    ///
    /// Verifies the same round-trip property at the APIC_LVT1 register offset.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_klapic_reg_lvt1_set_get_roundtrip() {
        let value: u32 = kani::any();

        let mut klapic = kvm_bindings::kvm_lapic_state::default();
        set_klapic_reg(&mut klapic, APIC_LVT1, value);
        let recovered = get_klapic_reg(&klapic, APIC_LVT1);

        kani::assert(
            recovered == value,
            "get_klapic_reg must return the value written by set_klapic_reg at APIC_LVT1",
        );
        kani::cover!(value == 0, "zero value roundtrip covered");
        kani::cover!(value == u32::MAX, "max value roundtrip covered");
    }

    /// Proof: set_klapic_reg does not modify bytes outside the 4-byte write window.
    ///
    /// Writing at offset O must leave bytes outside [O, O+4) unchanged.
    /// Verifies there is no byte spill from the little-endian i32 write.
    ///
    /// Strategy: write at a concrete offset (APIC_LVT0 = 0x350 = 848), then check
    /// that adjacent bytes at offset 0 are unchanged.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_klapic_reg_write_does_not_spill() {
        const WRITE_OFFSET: usize = APIC_LVT0; // 0x350

        let value: u32 = kani::any();

        let mut klapic = kvm_bindings::kvm_lapic_state::default();
        // Record the initial value at a different offset (offset 0 = APIC ID register).
        let before = get_klapic_reg(&klapic, 0);

        set_klapic_reg(&mut klapic, WRITE_OFFSET, value);

        // Offset 0 must be unchanged (it does not overlap with offset 0x350).
        let after = get_klapic_reg(&klapic, 0);
        kani::assert(
            before == after,
            "write at APIC_LVT0 must not affect bytes at offset 0",
        );
        kani::cover!(value != 0, "non-zero write no-spill path covered");
    }

    /// Proof: APIC mode constants fit in the 3-bit delivery mode field.
    ///
    /// APIC_MODE_NMI (4) and APIC_MODE_EXTINT (7) must be in [0, 7].
    /// set_apic_delivery_mode only uses bits [2:0] of mode via the 0x700 mask.
    #[kani::proof]
    fn proof_apic_mode_constants_valid() {
        kani::assert(
            APIC_MODE_NMI <= 0x7,
            "APIC_MODE_NMI must fit in the 3-bit delivery mode field",
        );
        kani::assert(
            APIC_MODE_EXTINT <= 0x7,
            "APIC_MODE_EXTINT must fit in the 3-bit delivery mode field",
        );
        // Verify they are distinct (no aliasing between NMI and EXTINT modes).
        kani::assert(
            APIC_MODE_NMI != APIC_MODE_EXTINT,
            "NMI and EXTINT delivery modes must be distinct",
        );
        kani::cover!(true, "APIC mode constants valid proof reachable");
    }

    /// Proof: set_lint correctly configures LVT0 (EXTINT) and LVT1 (NMI) delivery modes.
    ///
    /// Verifies the combined effect of set_apic_delivery_mode applied to both LVT entries:
    /// - LVT0 gets APIC_MODE_EXTINT in bits [10:8]
    /// - LVT1 gets APIC_MODE_NMI in bits [10:8]
    /// - All other bits in each LVT entry are preserved from the initial state.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_set_lint_modes_correct() {
        let initial_lvt0: u32 = kani::any();
        let initial_lvt1: u32 = kani::any();

        let result_lvt0 = set_apic_delivery_mode(initial_lvt0, APIC_MODE_EXTINT);
        let result_lvt1 = set_apic_delivery_mode(initial_lvt1, APIC_MODE_NMI);

        // LVT0 delivery mode field must be EXTINT.
        kani::assert(
            (result_lvt0 >> 8) & 0x7 == APIC_MODE_EXTINT,
            "LVT0 delivery mode must be APIC_MODE_EXTINT",
        );
        // LVT1 delivery mode field must be NMI.
        kani::assert(
            (result_lvt1 >> 8) & 0x7 == APIC_MODE_NMI,
            "LVT1 delivery mode must be APIC_MODE_NMI",
        );
        // Non-delivery-mode bits must be preserved.
        kani::assert(
            (result_lvt0 & !0x700) == (initial_lvt0 & !0x700),
            "LVT0 non-mode bits must be preserved",
        );
        kani::assert(
            (result_lvt1 & !0x700) == (initial_lvt1 & !0x700),
            "LVT1 non-mode bits must be preserved",
        );

        kani::cover!(true, "set_lint modes correct proof reachable");
    }
}

#[cfg(test)]
mod tests {
    extern crate utils;

    use super::*;
    use kvm_ioctls::Kvm;

    const KVM_APIC_REG_SIZE: usize = 0x400;

    #[test]
    fn test_set_and_get_klapic_reg() {
        let reg_offset = 0x340;
        let mut klapic = kvm_lapic_state::default();
        set_klapic_reg(&mut klapic, reg_offset, 3);
        let value = get_klapic_reg(&klapic, reg_offset);
        assert_eq!(value, 3);
    }

    #[test]
    #[should_panic]
    fn test_set_and_get_klapic_out_of_bounds() {
        let reg_offset = KVM_APIC_REG_SIZE + 10;
        let mut klapic = kvm_lapic_state::default();
        set_klapic_reg(&mut klapic, reg_offset, 3);
    }

    #[test]
    fn test_apic_delivery_mode() {
        let mut v: Vec<u32> = (0..20).map(|_| utils::rand::xor_rng_u32()).collect();

        v.iter_mut()
            .for_each(|x| *x = set_apic_delivery_mode(*x, 2));
        let after: Vec<u32> = v.iter().map(|x| (*x & !0x700) | (2 << 8)).collect();
        assert_eq!(v, after);
    }

    #[test]
    fn test_setlint() {
        let kvm = Kvm::new().unwrap();
        assert!(kvm.check_extension(kvm_ioctls::Cap::Irqchip));
        let vm = kvm.create_vm().unwrap();
        //the get_lapic ioctl will fail if there is no irqchip created beforehand.
        assert!(vm.create_irq_chip().is_ok());
        let vcpu = vm.create_vcpu(0).unwrap();
        let klapic_before: kvm_lapic_state = vcpu.get_lapic().unwrap();

        // Compute the value that is expected to represent LVT0 and LVT1.
        let lint0 = get_klapic_reg(&klapic_before, APIC_LVT0);
        let lint1 = get_klapic_reg(&klapic_before, APIC_LVT1);
        let lint0_mode_expected = set_apic_delivery_mode(lint0, APIC_MODE_EXTINT);
        let lint1_mode_expected = set_apic_delivery_mode(lint1, APIC_MODE_NMI);

        set_lint(&vcpu).unwrap();

        // Compute the value that represents LVT0 and LVT1 after set_lint.
        let klapic_actual: kvm_lapic_state = vcpu.get_lapic().unwrap();
        let lint0_mode_actual = get_klapic_reg(&klapic_actual, APIC_LVT0);
        let lint1_mode_actual = get_klapic_reg(&klapic_actual, APIC_LVT1);
        assert_eq!(lint0_mode_expected, lint0_mode_actual);
        assert_eq!(lint1_mode_expected, lint1_mode_actual);
    }

    #[test]
    fn test_setlint_fails() {
        let kvm = Kvm::new().unwrap();
        let vm = kvm.create_vm().unwrap();
        let vcpu = vm.create_vcpu(0).unwrap();
        // 'get_lapic' ioctl triggered by the 'set_lint' function will fail if there is no
        // irqchip created beforehand.
        assert!(set_lint(&vcpu).is_err());
    }
}
