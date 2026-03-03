// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Kani proofs for DirtyBitmap correctness.
//!
//! DirtyBitmap::mark_dirty silently ignores out-of-bounds addresses.
//! These proofs verify: (1) no panic for any u64 input, (2) out-of-bounds
//! addresses do not corrupt the bitmap, (3) in-bounds addresses are recorded.

use vmm::dirty_bitmap::DirtyBitmap;

/// Proof: mark_dirty never panics for any address with bitmap up to 256 pages.
///
/// Kani exhaustively explores all combinations of (guest_addr, size) where
/// size is at most 256 pages worth of bytes. For every combination, mark_dirty
/// must complete without panic or out-of-bounds array access.
///
/// Bound: 256 pages * PAGE_SIZE (16384) = 4,194,304 bytes maximum bitmap size.
#[kani::proof]
fn proof_mark_dirty_no_panic() {
    // Symbolic address: any possible u64 value.
    let guest_addr: u64 = kani::any();

    // Symbolic size: constrain to keep verification tractable.
    // DirtyBitmap::new uses PAGE_SIZE = 16384 internally.
    // 256 pages = 4,194,304 bytes.
    let num_pages: u64 = kani::any();
    kani::assume(num_pages > 0 && num_pages <= 256);
    let size = num_pages * 16384; // PAGE_SIZE = 16384

    // Base address: use 0 for simplicity; the address arithmetic is relative.
    let bitmap = DirtyBitmap::new(0, size);

    // This must not panic regardless of guest_addr value.
    bitmap.mark_dirty(guest_addr);
}

/// Proof: mark_dirty on an in-bounds address is reflected by drain_dirty_pages.
///
/// If guest_addr is within [0, size), then after mark_dirty, drain_dirty_pages
/// must return a non-empty vec containing the marked page.
///
/// Bound: 4 pages (keeps state space small; the bitmap logic is identical for N pages).
#[kani::proof]
fn proof_mark_dirty_in_bounds_recorded() {
    // Use a fixed 4-page bitmap for tractability.
    // PAGE_SIZE = 16384; 4 pages = 65536 bytes.
    let bitmap = DirtyBitmap::new(0, 4 * 16384);

    // Symbolic in-bounds address: within [0, 4 * PAGE_SIZE).
    let page_idx: u64 = kani::any();
    kani::assume(page_idx < 4);
    let guest_addr = page_idx * 16384;

    bitmap.mark_dirty(guest_addr);

    let dirty = bitmap.drain_dirty_pages();
    kani::assert(!dirty.is_empty(), "in-bounds mark_dirty must be recorded");
    kani::assert(dirty.contains(&guest_addr), "drained pages must contain marked address");
}

/// Proof: mark_dirty on an out-of-bounds address leaves the bitmap empty.
///
/// If guest_addr is outside [0, size), then drain_dirty_pages must return empty.
///
/// Bound: 4 pages.
#[kani::proof]
fn proof_mark_dirty_out_of_bounds_no_effect() {
    let bitmap = DirtyBitmap::new(0, 4 * 16384); // 4 pages

    // Symbolic out-of-bounds address: at or beyond 4 * PAGE_SIZE.
    let guest_addr: u64 = kani::any();
    kani::assume(guest_addr >= 4 * 16384);

    bitmap.mark_dirty(guest_addr);

    let dirty = bitmap.drain_dirty_pages();
    kani::assert(dirty.is_empty(), "out-of-bounds mark_dirty must not record anything");
}
