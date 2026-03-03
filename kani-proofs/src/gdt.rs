// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Kani proofs for GDT encoding/decoding round-trip correctness.
//!
//! gdt_entry encodes (flags, base, limit) into a u64 GDT descriptor.
//! kvm_segment_from_gdt decodes it back. The round-trip must preserve base and limit.

use arch::x86_64::{gdt_entry, kvm_segment_from_gdt};

/// Proof: gdt_entry/kvm_segment_from_gdt round-trip preserves base.
///
/// For all (flags: u16, base: u32, limit: u32 <= 0xFFFFF), the decoded
/// kvm_segment.base must equal base as u64.
///
/// This is exhaustive over all 32-bit base values — no bounds needed.
#[kani::proof]
fn proof_gdt_base_roundtrip() {
    let flags: u16 = kani::any();
    let base: u32 = kani::any();
    let limit: u32 = kani::any();
    // GDT limit field is 20 bits. Values above 0xFFFFF would be truncated;
    // the proof verifies round-trip only for representable values.
    kani::assume(limit <= 0xFFFFF);

    let entry = gdt_entry(flags, base, limit);
    let seg = kvm_segment_from_gdt(entry, 0);

    kani::assert(
        seg.base == base as u64,
        "decoded base must equal original base",
    );
}

/// Proof: gdt_entry/kvm_segment_from_gdt round-trip preserves limit.
///
/// For all (flags: u16, base: u32, limit: u32 <= 0xFFFFF), the decoded
/// kvm_segment.limit must equal limit.
#[kani::proof]
fn proof_gdt_limit_roundtrip() {
    let flags: u16 = kani::any();
    let base: u32 = kani::any();
    let limit: u32 = kani::any();
    kani::assume(limit <= 0xFFFFF);

    let entry = gdt_entry(flags, base, limit);
    let seg = kvm_segment_from_gdt(entry, 0);

    kani::assert(
        seg.limit == limit,
        "decoded limit must equal original limit",
    );
}

/// Proof: table_index is preserved in kvm_segment.selector.
///
/// kvm_segment.selector = table_index * 8. This verifies the selector encoding.
/// Bound: table_index [0, 255] (u8 full range).
#[kani::proof]
fn proof_gdt_selector_encoding() {
    let flags: u16 = kani::any();
    let base: u32 = kani::any();
    let limit: u32 = kani::any();
    kani::assume(limit <= 0xFFFFF);
    let table_index: u8 = kani::any();

    let entry = gdt_entry(flags, base, limit);
    let seg = kvm_segment_from_gdt(entry, table_index);

    kani::assert(
        seg.selector == u16::from(table_index) * 8,
        "selector must equal table_index * 8",
    );
}
