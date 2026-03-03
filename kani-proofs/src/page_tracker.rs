// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Kani proofs for PageTracker deduplication invariant.
//!
//! PageTracker::mark_loaded uses fetch_or to deduplicate: marking the same
//! page twice must increment the counter exactly once, regardless of source.

// Post-Phase-2 path (PageTracker in uffd/page_tracker.rs):
use vmm::uffd::page_tracker::{LoadSource, PageTracker};
// If Phase 2 has not run, use instead:
// use vmm::uffd::{LoadSource, PageTracker};

/// Proof: marking same page twice with same source increments counter exactly once.
///
/// Bound: total_pages <= 128.
#[kani::proof]
fn proof_mark_loaded_same_source_dedup() {
    let total_pages: usize = kani::any();
    kani::assume(total_pages > 0 && total_pages <= 128);

    let tracker = PageTracker::new(total_pages);

    let page_index: usize = kani::any();
    kani::assume(page_index < total_pages);

    // Mark the same page twice with the same source.
    tracker.mark_loaded(page_index, LoadSource::Preload);
    tracker.mark_loaded(page_index, LoadSource::Preload);

    let stats = tracker.stats();

    // loaded_pages counts set bits in the bitmap — must be exactly 1.
    kani::assert(
        stats.loaded_pages == 1,
        "loaded_pages must be 1 after marking same page twice",
    );
    // preload_count must also be 1 (not 2).
    kani::assert(
        stats.preload_pages == 1,
        "preload_pages must be 1 — counter must not double-increment",
    );
    // Total source counts must equal loaded_pages.
    kani::assert(
        stats.preload_pages + stats.fault_pages + stats.zero_pages == stats.loaded_pages,
        "sum of source counts must equal loaded_pages",
    );
}

/// Proof: marking same page with different sources still counts exactly once.
///
/// Preload marks the page first, then Fault tries to mark the same page.
/// Only the Preload counter should increment. loaded_pages must be 1.
///
/// Bound: total_pages <= 128.
#[kani::proof]
fn proof_mark_loaded_different_source_dedup() {
    let total_pages: usize = kani::any();
    kani::assume(total_pages > 0 && total_pages <= 128);

    let tracker = PageTracker::new(total_pages);

    let page_index: usize = kani::any();
    kani::assume(page_index < total_pages);

    // Preload marks it first.
    tracker.mark_loaded(page_index, LoadSource::Preload);
    // Fault handler tries to mark the same page (simulating EEXIST race in sequential order).
    tracker.mark_loaded(page_index, LoadSource::Fault);

    let stats = tracker.stats();

    kani::assert(
        stats.loaded_pages == 1,
        "loaded_pages must be 1 regardless of source order",
    );
    // Preload was first — preload_count incremented, fault_count did not.
    kani::assert(
        stats.preload_pages == 1,
        "preload_pages must be 1 (preload was first)",
    );
    kani::assert(
        stats.fault_pages == 0,
        "fault_pages must be 0 (bit was already set when fault tried)",
    );
}

/// Proof: out-of-bounds page_index is ignored — counters stay at 0.
///
/// Bound: total_pages <= 128.
#[kani::proof]
fn proof_mark_loaded_out_of_bounds_ignored() {
    let total_pages: usize = kani::any();
    kani::assume(total_pages > 0 && total_pages <= 128);

    let tracker = PageTracker::new(total_pages);

    // page_index at or beyond total_pages.
    let page_index: usize = kani::any();
    kani::assume(page_index >= total_pages);

    // This must not panic.
    tracker.mark_loaded(page_index, LoadSource::Preload);

    let stats = tracker.stats();
    kani::assert(
        stats.loaded_pages == 0,
        "out-of-bounds mark_loaded must not increment loaded_pages",
    );
    kani::assert(
        stats.preload_pages == 0,
        "out-of-bounds mark_loaded must not increment preload_pages",
    );
}

/// Proof: marking two distinct pages gives loaded_pages == 2.
///
/// Regression guard: ensures deduplication only applies to the same index.
///
/// Bound: total_pages <= 128, at least 2 pages.
#[kani::proof]
fn proof_mark_two_distinct_pages() {
    let total_pages: usize = kani::any();
    kani::assume(total_pages >= 2 && total_pages <= 128);

    let tracker = PageTracker::new(total_pages);

    let page_a: usize = kani::any();
    let page_b: usize = kani::any();
    kani::assume(page_a < total_pages);
    kani::assume(page_b < total_pages);
    kani::assume(page_a != page_b);

    tracker.mark_loaded(page_a, LoadSource::Preload);
    tracker.mark_loaded(page_b, LoadSource::Fault);

    let stats = tracker.stats();
    kani::assert(
        stats.loaded_pages == 2,
        "two distinct pages must give loaded_pages == 2",
    );
    kani::assert(
        stats.preload_pages == 1 && stats.fault_pages == 1,
        "each source must have count 1",
    );
}
