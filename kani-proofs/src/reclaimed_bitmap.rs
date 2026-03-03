// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Kani proofs for ReclaimedBitmap correctness.
//!
//! Verifies: mark/is_set round-trip, clear/is_set round-trip, count consistency.

use devices::virtio::balloon::ReclaimedBitmap;

/// Proof: mark(pfn) followed by is_set(pfn) returns true.
///
/// Bound: 256 pages; pfn is any valid index in [0, 255].
#[kani::proof]
fn proof_mark_then_is_set() {
    // Symbolic number of pages: [1, 256]
    let num_pages: usize = kani::any();
    kani::assume(num_pages > 0 && num_pages <= 256);

    let bitmap = ReclaimedBitmap::new(num_pages);

    // Symbolic PFN: valid index.
    let pfn: u32 = kani::any();
    kani::assume((pfn as usize) < num_pages);

    bitmap.mark(pfn);

    kani::assert(bitmap.is_set(pfn), "mark(pfn) must make is_set(pfn) return true");
}

/// Proof: mark(pfn) then clear(pfn) makes is_set(pfn) return false.
///
/// Bound: 256 pages.
#[kani::proof]
fn proof_clear_then_not_is_set() {
    let num_pages: usize = kani::any();
    kani::assume(num_pages > 0 && num_pages <= 256);

    let bitmap = ReclaimedBitmap::new(num_pages);

    let pfn: u32 = kani::any();
    kani::assume((pfn as usize) < num_pages);

    bitmap.mark(pfn);
    bitmap.clear(pfn);

    kani::assert(!bitmap.is_set(pfn), "clear(pfn) must make is_set(pfn) return false");
}

/// Proof: count() equals the number of set bits (popcount invariant).
///
/// After marking exactly one page, count() must return 1.
/// After clearing it, count() must return 0.
///
/// Bound: 256 pages. Testing single-page mark/clear keeps state small
/// while still verifying the count arithmetic path.
#[kani::proof]
fn proof_count_equals_popcount() {
    let num_pages: usize = kani::any();
    kani::assume(num_pages > 0 && num_pages <= 256);

    let bitmap = ReclaimedBitmap::new(num_pages);

    // Initially empty: count must be 0.
    kani::assert(bitmap.count() == 0, "fresh bitmap must have count 0");

    let pfn: u32 = kani::any();
    kani::assume((pfn as usize) < num_pages);

    // After marking one page: count must be 1.
    bitmap.mark(pfn);
    kani::assert(bitmap.count() == 1, "count must be 1 after marking one page");

    // After clearing: count must be 0 again.
    bitmap.clear(pfn);
    kani::assert(bitmap.count() == 0, "count must be 0 after clearing the only marked page");
}

/// Proof: out-of-bounds pfn is silently ignored by mark and clear.
///
/// A PFN equal to num_pages is out of bounds. mark and clear must not panic.
#[kani::proof]
fn proof_out_of_bounds_pfn_ignored() {
    let num_pages: usize = kani::any();
    kani::assume(num_pages > 0 && num_pages <= 255); // leave room for pfn = num_pages

    let bitmap = ReclaimedBitmap::new(num_pages);

    // PFN exactly at the boundary (out of bounds).
    let pfn = num_pages as u32;

    // These must not panic.
    bitmap.mark(pfn);
    bitmap.clear(pfn);

    // Bitmap must remain empty.
    kani::assert(bitmap.count() == 0, "out-of-bounds pfn must not affect count");
    kani::assert(!bitmap.is_set(pfn), "out-of-bounds pfn must not appear as set");
}
