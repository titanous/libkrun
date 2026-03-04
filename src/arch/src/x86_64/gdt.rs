// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

// For GDT details see arch/x86/include/asm/segment.h

use kvm_bindings::kvm_segment;

/// Constructor for a conventional segment GDT (or LDT) entry. Derived from the kernel's segment.h.
pub fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((u64::from(base) & 0xff00_0000u64) << (56 - 24))
        | ((u64::from(flags) & 0x0000_f0ffu64) << 40)
        | ((u64::from(limit) & 0x000f_0000u64) << (48 - 16))
        | ((u64::from(base) & 0x00ff_ffffu64) << 16)
        | (u64::from(limit) & 0x0000_ffffu64)
}

fn get_base(entry: u64) -> u64 {
    (((entry) & 0xFF00_0000_0000_0000) >> 32)
        | (((entry) & 0x0000_00FF_0000_0000) >> 16)
        | (((entry) & 0x0000_0000_FFFF_0000) >> 16)
}

fn get_limit(entry: u64) -> u32 {
    ((((entry) & 0x000F_0000_0000_0000) >> 32) | ((entry) & 0x0000_0000_0000_FFFF)) as u32
}

fn get_g(entry: u64) -> u8 {
    ((entry & 0x0080_0000_0000_0000) >> 55) as u8
}

fn get_db(entry: u64) -> u8 {
    ((entry & 0x0040_0000_0000_0000) >> 54) as u8
}

fn get_l(entry: u64) -> u8 {
    ((entry & 0x0020_0000_0000_0000) >> 53) as u8
}

fn get_avl(entry: u64) -> u8 {
    ((entry & 0x0010_0000_0000_0000) >> 52) as u8
}

fn get_p(entry: u64) -> u8 {
    ((entry & 0x0000_8000_0000_0000) >> 47) as u8
}

fn get_dpl(entry: u64) -> u8 {
    ((entry & 0x0000_6000_0000_0000) >> 45) as u8
}

fn get_s(entry: u64) -> u8 {
    ((entry & 0x0000_1000_0000_0000) >> 44) as u8
}

fn get_type(entry: u64) -> u8 {
    ((entry & 0x0000_0F00_0000_0000) >> 40) as u8
}

/// Automatically build the kvm struct for SET_SREGS from the kernel bit fields.
///
/// # Arguments
///
/// * `entry` - The gdt entry.
/// * `table_index` - Index of the entry in the gdt table.
pub fn kvm_segment_from_gdt(entry: u64, table_index: u8) -> kvm_segment {
    kvm_segment {
        base: get_base(entry),
        limit: get_limit(entry),
        selector: u16::from(table_index) * 8,
        type_: get_type(entry),
        present: get_p(entry),
        dpl: get_dpl(entry),
        db: get_db(entry),
        s: get_s(entry),
        l: get_l(entry),
        g: get_g(entry),
        avl: get_avl(entry),
        padding: 0,
        unusable: match get_p(entry) {
            0 => 1,
            _ => 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_parse() {
        let gdt = gdt_entry(0xA09B, 0x10_0000, 0xfffff);
        let seg = kvm_segment_from_gdt(gdt, 0);
        // 0xA09B
        // 'A'
        assert_eq!(0x1, seg.g);
        assert_eq!(0x0, seg.db);
        assert_eq!(0x1, seg.l);
        assert_eq!(0x0, seg.avl);
        // '9'
        assert_eq!(0x1, seg.present);
        assert_eq!(0x0, seg.dpl);
        assert_eq!(0x1, seg.s);
        // 'B'
        assert_eq!(0xB, seg.type_);
        // base and limit
        assert_eq!(0x10_0000, seg.base);
        assert_eq!(0xfffff, seg.limit);
        assert_eq!(0x0, seg.unusable);
    }

    #[cfg(not(loom))]
    mod proptest_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// get_base(gdt_entry(flags, base, limit)) == base for all valid inputs.
            ///
            /// GDT base is a 32-bit field embedded across bytes 2, 4, 5 of the 8-byte GDT entry.
            /// base is valid for values 0..=0xFF_FFFF (24-bit embedded portion).
            /// Full 32-bit base is encoded: bits 31-24 in byte 7, bits 23-16 in byte 4, bits 15-0 in bytes 2-3.
            #[test]
            fn prop_gdt_base_roundtrip(
                flags in 0u16..0xFFFF,
                base in 0u32..=u32::MAX,
                limit in 0u32..=0xFFFFF,  // 20-bit limit field
            ) {
                let entry = gdt_entry(flags, base, limit);
                let recovered_base = get_base(entry);
                prop_assert_eq!(recovered_base, base as u64);
            }

            /// kvm_segment_from_gdt preserves base.
            #[test]
            fn prop_kvm_segment_base_preserved(
                flags in 0u16..0xFFFF,
                base in 0u32..=u32::MAX,
                limit in 0u32..=0xFFFFF,
                table_index in 0u8..8,
            ) {
                let entry = gdt_entry(flags, base, limit);
                let seg = kvm_segment_from_gdt(entry, table_index);
                prop_assert_eq!(seg.base, base as u64);
            }
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Proof: gdt_entry/kvm_segment_from_gdt round-trip preserves base.
    ///
    /// For all (flags: u16, base: u32, limit: u32 <= 0xFFFFF), the decoded
    /// kvm_segment.base must equal base as u64.
    ///
    /// This is exhaustive over all 32-bit base values — no bounds needed.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_gdt_base_roundtrip() {
        let flags: u16 = kani::any();
        let base: u32 = kani::any();
        // GDT limit field is 20 bits. Values above 0xFFFFF would be truncated;
        // the proof verifies round-trip only for representable values.
        let limit: u32 = kani::any_where(|&l| l <= 0xFFFFF);

        let entry = gdt_entry(flags, base, limit);
        let seg = kvm_segment_from_gdt(entry, 0);

        kani::assert(
            seg.base == base as u64,
            "decoded base must equal original base",
        );
        kani::cover!(true, "gdt base roundtrip verified");
    }

    /// Proof: gdt_entry/kvm_segment_from_gdt round-trip preserves limit.
    ///
    /// For all (flags: u16, base: u32, limit: u32 <= 0xFFFFF), the decoded
    /// kvm_segment.limit must equal limit.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_gdt_limit_roundtrip() {
        let flags: u16 = kani::any();
        let base: u32 = kani::any();
        let limit: u32 = kani::any_where(|&l| l <= 0xFFFFF);

        let entry = gdt_entry(flags, base, limit);
        let seg = kvm_segment_from_gdt(entry, 0);

        kani::assert(
            seg.limit == limit,
            "decoded limit must equal original limit",
        );
        kani::cover!(true, "gdt limit roundtrip verified");
    }

    /// Proof: table_index is preserved in kvm_segment.selector.
    ///
    /// kvm_segment.selector = table_index * 8. This verifies the selector encoding.
    /// Bound: table_index [0, 255] (u8 full range).
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_gdt_selector_encoding() {
        let flags: u16 = kani::any();
        let base: u32 = kani::any();
        let limit: u32 = kani::any_where(|&l| l <= 0xFFFFF);
        let table_index: u8 = kani::any();

        let entry = gdt_entry(flags, base, limit);
        let seg = kvm_segment_from_gdt(entry, table_index);

        kani::assert(
            seg.selector == u16::from(table_index) * 8,
            "selector must equal table_index * 8",
        );
        kani::cover!(true, "selector encoding verified");
    }
}
