// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

// For GDT details see arch/x86/include/asm/segment.h

use kvm_bindings::kvm_segment;

/// Constructor for a conventional segment GDT (or LDT) entry. Derived from the kernel's segment.h.
///
/// # Contracts
/// The GDT limit field is 20 bits; the result correctly encodes base, limit, and flags.
/// Only limit values ≤ 0xFFFFF are representable without truncation.
#[cfg_attr(kani, kani::requires(limit <= 0xFFFFF))]
#[cfg_attr(kani, kani::ensures(|&result| {
    // Verify base round-trips: the three base fragments reassemble to the original base.
    let recovered_base =
        ((result & 0xFF00_0000_0000_0000) >> 32)
        | ((result & 0x0000_00FF_0000_0000) >> 16)
        | ((result & 0x0000_0000_FFFF_0000) >> 16);
    recovered_base == u64::from(base)
}))]
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
#[cfg_attr(kani, kani::ensures(|result| {
    // selector must equal table_index * 8
    result.selector == u16::from(table_index) * 8
}))]
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
    /// Bound: no loops; no unwind attribute needed.
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
        kani::cover!(base == 0, "zero base exercised");
        kani::cover!(base == u32::MAX, "max base exercised");
    }

    /// Proof: gdt_entry/kvm_segment_from_gdt round-trip preserves limit.
    ///
    /// For all (flags: u16, base: u32, limit: u32 <= 0xFFFFF), the decoded
    /// kvm_segment.limit must equal limit.
    /// Bound: no loops; no unwind attribute needed.
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
        kani::cover!(limit == 0, "zero limit exercised");
        kani::cover!(limit == 0xFFFFF, "max 20-bit limit exercised");
    }

    /// Proof: table_index is preserved in kvm_segment.selector.
    ///
    /// kvm_segment.selector = table_index * 8. This verifies the selector encoding.
    /// Bound: table_index [0, 255] (u8 full range). No loops; no unwind attribute needed.
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
        kani::cover!(table_index == 0, "null descriptor index exercised");
        kani::cover!(table_index == u8::MAX, "max table index exercised");
    }

    /// Proof: gdt_entry/kvm_segment_from_gdt round-trip preserves single-bit flag fields.
    ///
    /// Verifies that g, db, l, avl, present, and s are each correctly extracted from
    /// the GDT entry flags field. Each is a single bit at a specific position in the
    /// 16-bit flags argument:
    ///   g       = flags[15]
    ///   db      = flags[14]
    ///   l       = flags[13]
    ///   avl     = flags[12]
    ///   present = flags[7]
    ///   s       = flags[4]
    ///
    /// Also verifies that unusable is the complement of present.
    /// Bound: no loops; no unwind attribute needed.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_gdt_single_bit_flags_roundtrip() {
        let flags: u16 = kani::any();
        let base: u32 = kani::any();
        let limit: u32 = kani::any_where(|&l| l <= 0xFFFFF);

        let entry = gdt_entry(flags, base, limit);
        let seg = kvm_segment_from_gdt(entry, 0);

        let expected_g = ((flags >> 15) & 1) as u8;
        let expected_db = ((flags >> 14) & 1) as u8;
        let expected_l = ((flags >> 13) & 1) as u8;
        let expected_avl = ((flags >> 12) & 1) as u8;
        let expected_present = ((flags >> 7) & 1) as u8;
        let expected_s = ((flags >> 4) & 1) as u8;
        let expected_unusable = if expected_present != 0 { 0u8 } else { 1u8 };

        kani::assert(seg.g == expected_g, "g must match flags[15]");
        kani::assert(seg.db == expected_db, "db must match flags[14]");
        kani::assert(seg.l == expected_l, "l must match flags[13]");
        kani::assert(seg.avl == expected_avl, "avl must match flags[12]");
        kani::assert(
            seg.present == expected_present,
            "present must match flags[7]",
        );
        kani::assert(seg.s == expected_s, "s must match flags[4]");
        kani::assert(
            seg.unusable == expected_unusable,
            "unusable must be complement of present",
        );

        kani::cover!(expected_present == 1, "present segment covered");
        kani::cover!(
            expected_present == 0,
            "not-present (unusable) segment covered"
        );
    }

    /// Proof: gdt_entry/kvm_segment_from_gdt round-trip preserves multi-bit flag fields.
    ///
    /// Verifies that dpl and type_ are correctly extracted from the GDT entry flags field:
    ///   dpl   = flags[6:5] (2 bits)
    ///   type_ = flags[3:0] (4 bits)
    /// Bound: no loops; no unwind attribute needed.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_gdt_multi_bit_flags_roundtrip() {
        let flags: u16 = kani::any();
        let base: u32 = kani::any();
        let limit: u32 = kani::any_where(|&l| l <= 0xFFFFF);

        let entry = gdt_entry(flags, base, limit);
        let seg = kvm_segment_from_gdt(entry, 0);

        let expected_dpl = ((flags >> 5) & 0x3) as u8;
        let expected_type = (flags & 0xF) as u8;

        kani::assert(seg.dpl == expected_dpl, "dpl must match flags[6:5]");
        kani::assert(seg.type_ == expected_type, "type_ must match flags[3:0]");

        kani::cover!(expected_dpl == 0, "dpl=0 covered");
        kani::cover!(expected_dpl == 3, "dpl=3 covered");
        kani::cover!(expected_type == 0xF, "type_=0xF covered");
    }

    // ── Contract-based proofs ─────────────────────────────────────────────────

    /// Contract proof: `gdt_entry` encodes base correctly (requires limit <= 0xFFFFF).
    ///
    /// `#[kani::requires]` gates the precondition; `#[kani::ensures]` checks base encoding.
    /// No unwind bound needed — gdt_entry is a purely arithmetic expression.
    #[kani::proof_for_contract(gdt_entry)]
    #[kani::solver(cadical)]
    fn proof_contract_gdt_entry_base_encoding() {
        let flags: u16 = kani::any();
        let base: u32 = kani::any();
        let limit: u32 = kani::any_where(|&l| l <= 0xFFFFF);
        let _ = gdt_entry(flags, base, limit);
    }

    /// Contract proof: `kvm_segment_from_gdt` selector equals table_index * 8.
    ///
    /// Uses `stub_verified(gdt_entry)` so the entry is treated as an arbitrary u64
    /// satisfying gdt_entry's contract, enabling compositional verification.
    #[kani::proof_for_contract(kvm_segment_from_gdt)]
    #[kani::stub_verified(gdt_entry)]
    #[kani::solver(cadical)]
    fn proof_contract_kvm_segment_selector() {
        let flags: u16 = kani::any();
        let base: u32 = kani::any();
        let limit: u32 = kani::any_where(|&l| l <= 0xFFFFF);
        let entry = gdt_entry(flags, base, limit);
        let table_index: u8 = kani::any();
        let _ = kvm_segment_from_gdt(entry, table_index);
    }
}
