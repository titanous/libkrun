// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Pure page tracking logic: bitmap, statistics, and address translation.
//! No syscalls; testable under Miri and loom.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::snapshot_store::system_page_size;
use userfaultfd;

/// Represents a guest memory region registered with UFFD.
#[derive(Clone, Debug)]
pub struct UffdRegion {
    /// Guest physical address
    pub guest_addr: u64,
    /// Host virtual address (mmap'd)
    pub host_addr: u64,
    /// Size in bytes
    pub size: u64,
    /// Offset into the global page index bitmap for this region (sum of pages in prior regions)
    pub page_offset: usize,
}

/// Translate a guest address to a host address using the registered regions.
///
/// Returns `None` if the guest address is not found in any region.
pub fn guest_to_host(regions: &[UffdRegion], guest_addr: u64) -> Option<u64> {
    for region in regions {
        if guest_addr >= region.guest_addr && guest_addr < region.guest_addr + region.size {
            return Some(region.host_addr + (guest_addr - region.guest_addr));
        }
    }
    None
}

/// Translate a guest address to a page index in the global page bitmap.
///
/// Uses region-relative indexing: finds the region containing the guest address,
/// calculates the page offset within that region, and adds the region's base page offset.
///
/// Returns `None` if the guest address is not found in any region.
pub fn guest_addr_to_page_index(regions: &[UffdRegion], guest_addr: u64) -> Option<usize> {
    for region in regions {
        if guest_addr >= region.guest_addr && guest_addr < region.guest_addr + region.size {
            let region_offset = guest_addr - region.guest_addr;
            let page_in_region = (region_offset / system_page_size()) as usize;
            return Some(region.page_offset + page_in_region);
        }
    }
    None
}

/// Translate a host address to a guest address using the registered regions.
///
/// # Panics
/// Panics if the host address is not found in any registered region.
/// This should never happen with valid UFFD faults from registered memory.
pub fn host_to_guest(regions: &[UffdRegion], host_addr: u64) -> u64 {
    for region in regions {
        if host_addr >= region.host_addr && host_addr < region.host_addr + region.size {
            return region.guest_addr + (host_addr - region.host_addr);
        }
    }
    panic!(
        "UFFD fault at host address 0x{:x} not found in any registered region",
        host_addr
    );
}

/// Check if an error represents EEXIST (page already mapped).
pub fn is_eexist(e: &userfaultfd::Error) -> bool {
    match e {
        userfaultfd::Error::CopyFailed(errno) if *errno as i32 == libc::EEXIST => true,
        userfaultfd::Error::ZeropageFailed(errno) if *errno as i32 == libc::EEXIST => true,
        _ => false,
    }
}

/// Source of page load: preload, demand fault, or zero-fill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoadSource {
    /// Page loaded via preload stream
    Preload,
    /// Page loaded via fault handler
    Fault,
    /// Page zero-filled via uffd.zeropage()
    Zero,
}

/// Statistics snapshot for restore progress monitoring.
#[derive(Debug, Clone)]
pub struct PageTrackerStats {
    /// Total number of pages tracked
    pub total_pages: usize,
    /// Number of pages that have been loaded (set bits in bitmap)
    pub loaded_pages: usize,
    /// Pages loaded via preload
    pub preload_pages: usize,
    /// Pages loaded via fault handler
    pub fault_pages: usize,
    /// Pages zero-filled via zeropage
    pub zero_pages: usize,
    /// Total fault events received (including EEXIST races)
    pub total_faults: usize,
    /// Progress percentage (loaded_pages / total_pages * 100.0)
    pub progress_pct: f64,
}

/// Atomic bitmap for tracking which guest pages have been loaded during restore.
///
/// Uses `AtomicU64` words to enable lock-free updates from concurrent fault handler
/// and preload tasks. One bit per page, packed into u64 words.
///
/// Thread-safe: `mark_loaded` uses atomic OR and can be called from concurrent tasks.
pub struct PageTracker {
    /// Total number of pages tracked
    total_pages: usize,
    /// Bitmap stored as AtomicU64 words (each covers 64 pages)
    bitmap: Vec<AtomicU64>,
    /// Number of pages loaded via preload stream
    preload_count: AtomicUsize,
    /// Number of pages loaded via fault handler
    fault_count: AtomicUsize,
    /// Number of pages zero-filled via zeropage
    zero_count: AtomicUsize,
    /// Total faults received (including EEXIST)
    total_faults: AtomicUsize,
}

impl PageTracker {
    /// Create a new page tracker for the given number of pages.
    ///
    /// Allocates and zeroes the bitmap.
    pub fn new(total_pages: usize) -> Self {
        let num_words = total_pages.div_ceil(64);
        let bitmap: Vec<AtomicU64> = (0..num_words).map(|_| AtomicU64::new(0)).collect();

        PageTracker {
            total_pages,
            bitmap,
            preload_count: AtomicUsize::new(0),
            fault_count: AtomicUsize::new(0),
            zero_count: AtomicUsize::new(0),
            total_faults: AtomicUsize::new(0),
        }
    }

    /// Mark a page as loaded from the given source.
    ///
    /// Uses atomic OR to set the bit. If the bit was already set (page previously loaded),
    /// the counter is not incremented. This handles EEXIST races where both preload and
    /// fault handler might try to load the same page.
    pub fn mark_loaded(&self, page_index: usize, source: LoadSource) {
        if page_index >= self.total_pages {
            return;
        }

        let word_idx = page_index / 64;
        let bit_idx = page_index % 64;

        // Use fetch_or to atomically set the bit. It returns the old value.
        let old_word = self.bitmap[word_idx].fetch_or(1u64 << bit_idx, Ordering::Relaxed);

        // Only increment counter if bit was not already set
        if (old_word >> bit_idx) & 1 == 0 {
            match source {
                LoadSource::Preload => {
                    self.preload_count.fetch_add(1, Ordering::Relaxed);
                }
                LoadSource::Fault => {
                    self.fault_count.fetch_add(1, Ordering::Relaxed);
                }
                LoadSource::Zero => {
                    self.zero_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Record that a fault event was received.
    ///
    /// Called on every fault event, regardless of outcome (including EEXIST).
    pub fn record_fault(&self) {
        self.total_faults.fetch_add(1, Ordering::Relaxed);
    }

    /// Check whether a page has been loaded.
    pub fn is_loaded(&self, page_index: usize) -> bool {
        if page_index >= self.total_pages {
            return false;
        }

        let word_idx = page_index / 64;
        let bit_idx = page_index % 64;

        (self.bitmap[word_idx].load(Ordering::Relaxed) >> bit_idx) & 1 != 0
    }

    /// Get a snapshot of current statistics.
    ///
    /// This counts all set bits in the bitmap (O(n/64) where n = total_pages).
    pub fn stats(&self) -> PageTrackerStats {
        // Count set bits across all words
        let mut loaded_pages = 0;
        for word in &self.bitmap {
            let w = word.load(Ordering::Relaxed);
            loaded_pages += w.count_ones() as usize;
        }

        let preload_pages = self.preload_count.load(Ordering::Relaxed);
        let fault_pages = self.fault_count.load(Ordering::Relaxed);
        let zero_pages = self.zero_count.load(Ordering::Relaxed);
        let total_faults = self.total_faults.load(Ordering::Relaxed);

        let progress_pct = if self.total_pages > 0 {
            (loaded_pages as f64 / self.total_pages as f64) * 100.0
        } else {
            0.0
        };

        PageTrackerStats {
            total_pages: self.total_pages,
            loaded_pages,
            preload_pages,
            fault_pages,
            zero_pages,
            total_faults,
            progress_pct,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guest_to_host() {
        let regions = vec![
            UffdRegion {
                guest_addr: 0x1000,
                host_addr: 0x2000,
                size: 0x1000,
                page_offset: 0,
            },
            UffdRegion {
                guest_addr: 0x3000,
                host_addr: 0x4000,
                size: 0x1000,
                page_offset: 1,
            },
        ];

        assert_eq!(guest_to_host(&regions, 0x1000), Some(0x2000));
        assert_eq!(guest_to_host(&regions, 0x1500), Some(0x2500));
        assert_eq!(guest_to_host(&regions, 0x3000), Some(0x4000));
        assert_eq!(guest_to_host(&regions, 0x5000), None);
    }

    #[test]
    fn test_guest_addr_to_page_index() {
        let regions = vec![UffdRegion {
            guest_addr: 0x0,
            host_addr: 0x0,
            size: 0x10000,
            page_offset: 0,
        }];

        // Assuming 4096 byte pages
        let page_size = 4096u64;
        assert_eq!(guest_addr_to_page_index(&regions, 0x0), Some(0));
        assert_eq!(guest_addr_to_page_index(&regions, page_size), Some(1));
    }

    #[test]
    fn test_host_to_guest() {
        let regions = vec![UffdRegion {
            guest_addr: 0x1000,
            host_addr: 0x2000,
            size: 0x1000,
            page_offset: 0,
        }];

        assert_eq!(host_to_guest(&regions, 0x2000), 0x1000);
        assert_eq!(host_to_guest(&regions, 0x2500), 0x1500);
    }

    #[test]
    #[should_panic(expected = "not found in any registered region")]
    fn test_host_to_guest_panic() {
        let regions = vec![UffdRegion {
            guest_addr: 0x1000,
            host_addr: 0x2000,
            size: 0x1000,
            page_offset: 0,
        }];

        host_to_guest(&regions, 0x5000);
    }

    #[test]
    fn test_page_tracker_new() {
        let tracker = PageTracker::new(100);
        let stats = tracker.stats();
        assert_eq!(stats.total_pages, 100);
        assert_eq!(stats.loaded_pages, 0);
        assert_eq!(stats.progress_pct, 0.0);
    }

    #[test]
    fn test_page_tracker_mark_loaded() {
        let tracker = PageTracker::new(100);
        tracker.mark_loaded(0, LoadSource::Preload);
        tracker.mark_loaded(1, LoadSource::Fault);
        tracker.mark_loaded(2, LoadSource::Zero);

        let stats = tracker.stats();
        assert_eq!(stats.loaded_pages, 3);
        assert_eq!(stats.preload_pages, 1);
        assert_eq!(stats.fault_pages, 1);
        assert_eq!(stats.zero_pages, 1);
    }

    #[test]
    fn test_page_tracker_deduplication() {
        let tracker = PageTracker::new(100);
        tracker.mark_loaded(0, LoadSource::Preload);
        tracker.mark_loaded(0, LoadSource::Fault);

        let stats = tracker.stats();
        assert_eq!(stats.loaded_pages, 1);
        assert_eq!(stats.preload_pages, 1);
        assert_eq!(stats.fault_pages, 0); // Not incremented due to deduplication
    }

    #[test]
    fn test_page_tracker_out_of_bounds() {
        let tracker = PageTracker::new(100);
        tracker.mark_loaded(100, LoadSource::Preload);
        tracker.mark_loaded(1000, LoadSource::Preload);

        let stats = tracker.stats();
        assert_eq!(stats.loaded_pages, 0);
        assert_eq!(stats.preload_pages, 0);
    }

    #[test]
    fn test_page_tracker_record_fault() {
        let tracker = PageTracker::new(100);
        tracker.record_fault();
        tracker.record_fault();

        let stats = tracker.stats();
        assert_eq!(stats.total_faults, 2);
    }

    #[test]
    fn test_is_loaded() {
        let tracker = PageTracker::new(100);
        assert!(!tracker.is_loaded(0));
        tracker.mark_loaded(0, LoadSource::Preload);
        assert!(tracker.is_loaded(0));
        assert!(!tracker.is_loaded(1));
    }

    #[test]
    fn test_progress_percentage() {
        let tracker = PageTracker::new(100);
        tracker.mark_loaded(0, LoadSource::Preload);
        tracker.mark_loaded(1, LoadSource::Preload);

        let stats = tracker.stats();
        assert_eq!(stats.progress_pct, 2.0);
    }

    #[cfg(not(loom))]
    mod proptest_tests {
        use super::*;
        use proptest::prelude::*;

        fn arb_region() -> impl Strategy<Value = UffdRegion> {
            (
                0u64..0x8000_0000, // guest_addr (up to 2GB)
                1u64..0x1000_0000, // size (up to 256MB, must be > 0)
                0u64..0x8000_0000, // host_addr
            )
                .prop_map(|(guest_addr, size, host_addr)| UffdRegion {
                    guest_addr,
                    host_addr,
                    size,
                    page_offset: 0,
                })
        }

        proptest! {
            /// guest_to_host returns Some for addresses inside the region.
            #[test]
            fn prop_guest_to_host_in_range(
                region in arb_region(),
                offset in 0u64..0x1000_0000u64,
            ) {
                let addr = region.guest_addr.saturating_add(offset % region.size);
                let regions = vec![region.clone()];
                let result = guest_to_host(&regions, addr);
                prop_assert!(result.is_some(), "expected Some for addr={addr:#x} in region [{:#x},{:#x})", region.guest_addr, region.guest_addr + region.size);
            }

            /// guest_to_host returns None for addresses before the region.
            #[test]
            fn prop_guest_to_host_before_region(region in arb_region()) {
                // Only test if there's address space before the region
                prop_assume!(region.guest_addr > 0);
                let addr = region.guest_addr - 1;
                let regions = vec![region];
                let result = guest_to_host(&regions, addr);
                prop_assert!(result.is_none());
            }

            /// guest_to_host returns None for addresses after the region.
            #[test]
            fn prop_guest_to_host_after_region(region in arb_region()) {
                let addr = region.guest_addr.saturating_add(region.size);
                // Skip if overflow (saturating_add would wrap to a valid address)
                prop_assume!(addr > region.guest_addr);
                let regions = vec![region];
                let result = guest_to_host(&regions, addr);
                prop_assert!(result.is_none());
            }

            /// PageTracker mark_loaded deduplication: marking same page twice doesn't double-count.
            #[test]
            fn prop_mark_loaded_deduplication(
                total_pages in 1usize..256,
                page_index in 0usize..256,
            ) {
                prop_assume!(page_index < total_pages);
                let tracker = PageTracker::new(total_pages);
                tracker.mark_loaded(page_index, LoadSource::Preload);
                tracker.mark_loaded(page_index, LoadSource::Preload);
                // Count should be 1, not 2
                let stats = tracker.stats();
                prop_assert_eq!(stats.preload_pages, 1);
                prop_assert_eq!(stats.loaded_pages, 1);
            }
        }
    }

    #[cfg(loom)]
    mod loom_tests {
        use super::*;
        use loom::sync::Arc;
        use loom::thread;

        /// Concurrent Preload + Fault mark_loaded on same page: exactly one counter increment.
        ///
        /// Two threads both call mark_loaded for page_index=0 from different LoadSources.
        /// Because mark_loaded uses fetch_or + conditional counter increment, exactly one
        /// should increment its counter, and loaded_pages must be 1 (not 2).
        #[test]
        fn loom_mark_loaded_dedup_concurrent() {
            loom::model(|| {
                let tracker = Arc::new(PageTracker::new(64));

                let t1 = Arc::clone(&tracker);
                let preloader = thread::spawn(move || {
                    t1.mark_loaded(0, LoadSource::Preload);
                });

                let t2 = Arc::clone(&tracker);
                let fault_handler = thread::spawn(move || {
                    t2.mark_loaded(0, LoadSource::Fault);
                });

                preloader.join().unwrap();
                fault_handler.join().unwrap();

                let stats = tracker.stats();
                // loaded_pages must be exactly 1 (deduplication via fetch_or)
                assert_eq!(
                    stats.loaded_pages, 1,
                    "expected exactly 1 loaded page, got {} (preload={}, fault={})",
                    stats.loaded_pages, stats.preload_pages, stats.fault_pages
                );
                // The total counter (preload + fault) must also be 1
                assert_eq!(
                    stats.preload_pages + stats.fault_pages,
                    1,
                    "preload={} fault={} — should sum to 1",
                    stats.preload_pages,
                    stats.fault_pages
                );
            });
        }

        /// Concurrent marks on different pages: both must be tracked.
        #[test]
        fn loom_mark_loaded_different_pages() {
            loom::model(|| {
                let tracker = Arc::new(PageTracker::new(64));

                let t1 = Arc::clone(&tracker);
                let t_a = thread::spawn(move || {
                    t1.mark_loaded(0, LoadSource::Preload);
                });

                let t2 = Arc::clone(&tracker);
                let t_b = thread::spawn(move || {
                    t2.mark_loaded(1, LoadSource::Fault);
                });

                t_a.join().unwrap();
                t_b.join().unwrap();

                let stats = tracker.stats();
                assert_eq!(stats.loaded_pages, 2);
                assert_eq!(stats.preload_pages, 1);
                assert_eq!(stats.fault_pages, 1);
            });
        }
    }
}

#[cfg(kani)]
impl kani::Arbitrary for UffdRegion {
    fn any() -> Self {
        let guest_addr: u64 = kani::any();
        let host_addr: u64 = kani::any();
        let size: u64 = kani::any_where(|&s| s > 0);
        kani::assume(guest_addr.checked_add(size).is_some());
        kani::assume(host_addr.checked_add(size).is_some());
        UffdRegion {
            guest_addr,
            host_addr,
            size,
            page_offset: 0,
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    // ── PageTracker proofs ────────────────────────────────────────────────────

    /// Proof: marking same page twice with same source increments counter exactly once.
    ///
    /// Bound: total_pages <= 128. stats() iterates ceil(128/64)=2 words → unwind(3).
    #[kani::proof]
    #[kani::unwind(3)]
    fn proof_mark_loaded_same_source_dedup() {
        let total_pages: usize = kani::any_where(|&n| n > 0 && n <= 128);
        let tracker = PageTracker::new(total_pages);

        let page_index: usize = kani::any_where(|&p| p < total_pages);

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
        kani::cover!(true, "same-source dedup path reachable");
    }

    /// Proof: marking same page with different sources still counts exactly once.
    ///
    /// Preload marks the page first, then Fault tries to mark the same page.
    /// Only the Preload counter should increment. loaded_pages must be 1.
    ///
    /// Bound: total_pages <= 128. stats() iterates ceil(128/64)=2 words → unwind(3).
    #[kani::proof]
    #[kani::unwind(3)]
    fn proof_mark_loaded_different_source_dedup() {
        let total_pages: usize = kani::any_where(|&n| n > 0 && n <= 128);
        let tracker = PageTracker::new(total_pages);

        let page_index: usize = kani::any_where(|&p| p < total_pages);

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
        kani::cover!(true, "different-source dedup path reachable");
    }

    /// Proof: out-of-bounds page_index is ignored — counters stay at 0.
    ///
    /// Bound: total_pages <= 128. stats() iterates ceil(128/64)=2 words → unwind(3).
    #[kani::proof]
    #[kani::unwind(3)]
    fn proof_mark_loaded_out_of_bounds_ignored() {
        let total_pages: usize = kani::any_where(|&n| n > 0 && n <= 128);
        let tracker = PageTracker::new(total_pages);

        // page_index at or beyond total_pages.
        let page_index: usize = kani::any_where(|&p: &usize| p >= total_pages);

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
        kani::cover!(true, "out-of-bounds ignored path reachable");
    }

    /// Proof: marking two distinct pages gives loaded_pages == 2.
    ///
    /// Regression guard: ensures deduplication only applies to the same index.
    ///
    /// Bound: total_pages <= 128, at least 2 pages.
    /// stats() iterates ceil(128/64)=2 words → unwind(3).
    #[kani::proof]
    #[kani::unwind(3)]
    fn proof_mark_two_distinct_pages() {
        let total_pages: usize = kani::any_where(|&n| n >= 2 && n <= 128);
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
        kani::cover!(true, "two-distinct-pages path reachable");
    }

    /// Proof: all LoadSource variants are accepted by mark_loaded.
    /// PageTracker::new creates Vec of ceil(total_pages/64) AtomicU64 words.
    /// With total_pages <= 128: up to 2 words → Vec init loop needs unwind(3).
    #[kani::proof]
    #[kani::unwind(3)]
    fn proof_mark_loaded_all_sources_accepted() {
        let total_pages: usize = kani::any_where(|&n| n > 0 && n <= 128);
        let page_index: usize = kani::any_where(|&p| p < total_pages);

        let tracker = PageTracker::new(total_pages);
        tracker.mark_loaded(page_index, LoadSource::Preload);
        kani::cover!(true, "Preload source accepted");

        let tracker2 = PageTracker::new(total_pages);
        tracker2.mark_loaded(page_index, LoadSource::Fault);
        kani::cover!(true, "Fault source accepted");

        let tracker3 = PageTracker::new(total_pages);
        tracker3.mark_loaded(page_index, LoadSource::Zero);
        kani::cover!(true, "Zero source accepted");
    }

    // ── Address translation proofs ────────────────────────────────────────────

    /// Proof: guest_to_host returns Some with correct offset for in-range addresses.
    ///
    /// For a single region, any address in [guest_addr, guest_addr + size) must map
    /// to Some(host_addr + (addr - guest_addr)).
    ///
    /// Bound: 1 region (the correctness of the loop is the same for N regions).
    /// guest_to_host iterates 1 region → unwind(2).
    #[kani::proof]
    #[kani::unwind(2)]
    #[kani::solver(cadical)]
    fn proof_guest_to_host_in_range_correct() {
        // Symbolic region parameters.
        let region_guest: u64 = kani::any();
        let region_host: u64 = kani::any();
        let region_size: u64 = kani::any_where(|&s| s > 0);
        // Avoid overflow in guest_addr + size and host_addr + size.
        kani::assume(region_guest.checked_add(region_size).is_some());
        kani::assume(region_host.checked_add(region_size).is_some());

        let region = UffdRegion {
            guest_addr: region_guest,
            host_addr: region_host,
            size: region_size,
            page_offset: 0,
        };

        // Symbolic in-range address.
        let addr: u64 = kani::any();
        kani::assume(addr >= region_guest);
        kani::assume(addr < region_guest + region_size);

        let result = guest_to_host(&[region.clone()], addr);

        kani::assert(result.is_some(), "in-range address must produce Some");
        let expected_host = region_host + (addr - region_guest);
        kani::assert(
            result == Some(expected_host),
            "host address must be region.host_addr + offset",
        );
        kani::cover!(true, "in-range Some path reachable");
    }

    /// Proof: guest_to_host returns None for addresses before any region.
    ///
    /// Bound: 1 region. guest_to_host iterates 1 region → unwind(2).
    #[kani::proof]
    #[kani::unwind(2)]
    #[kani::solver(cadical)]
    fn proof_guest_to_host_before_region_is_none() {
        let region_guest: u64 = kani::any_where(|&g| g > 0);
        let region_host: u64 = kani::any();
        let region_size: u64 = kani::any_where(|&s| s > 0);
        kani::assume(region_guest.checked_add(region_size).is_some());

        let region = UffdRegion {
            guest_addr: region_guest,
            host_addr: region_host,
            size: region_size,
            page_offset: 0,
        };

        // Symbolic address strictly before the region.
        let addr: u64 = kani::any_where(|&a| a < region_guest);

        let result = guest_to_host(&[region], addr);
        kani::assert(result.is_none(), "address before region must produce None");
        kani::cover!(true, "before-region None path reachable");
    }

    /// Proof: guest_to_host returns None for addresses at or after region end.
    ///
    /// Bound: 1 region. guest_to_host iterates 1 region → unwind(2).
    #[kani::proof]
    #[kani::unwind(2)]
    #[kani::solver(cadical)]
    fn proof_guest_to_host_after_region_is_none() {
        let region_guest: u64 = kani::any();
        let region_host: u64 = kani::any();
        let region_size: u64 = kani::any_where(|&s| s > 0);
        // Ensure region_guest + region_size does not overflow.
        kani::assume(region_guest.checked_add(region_size).is_some());

        let region = UffdRegion {
            guest_addr: region_guest,
            host_addr: region_host,
            size: region_size,
            page_offset: 0,
        };

        // Address at or after the region end.
        let addr: u64 = kani::any_where(|&a| a >= region_guest + region_size);

        let result = guest_to_host(&[region], addr);
        kani::assert(
            result.is_none(),
            "address at or after region end must produce None",
        );
        kani::cover!(true, "after-region None path reachable");
    }

    /// Proof: host_to_guest is the left inverse of guest_to_host for a single region.
    ///
    /// For any in-range guest address `ga`, translating to a host address via
    /// `guest_to_host` and then back via `host_to_guest` must recover `ga`.
    ///
    /// Bound: 1 region → unwind(2) for each of the two single-region iterating functions.
    #[kani::proof]
    #[kani::unwind(2)]
    #[kani::solver(cadical)]
    fn proof_host_to_guest_inverse_of_guest_to_host() {
        // Symbolic region parameters — no overflow on either end.
        let guest_addr: u64 = kani::any();
        let host_addr: u64 = kani::any();
        let size: u64 = kani::any_where(|&s| s > 0 && s <= 4096);
        kani::assume(guest_addr.checked_add(size).is_some());
        kani::assume(host_addr.checked_add(size).is_some());

        let regions = [UffdRegion {
            guest_addr,
            host_addr,
            size,
            page_offset: 0,
        }];

        // Symbolic in-range guest address.
        let ga: u64 = kani::any_where(|&a| a >= guest_addr && a < guest_addr + size);

        let ha = guest_to_host(&regions, ga).expect("in-range address must produce Some");
        let result = host_to_guest(&regions, ha);

        kani::assert(
            result == ga,
            "host_to_guest must be the inverse of guest_to_host",
        );
        kani::cover!(true, "host_to_guest inverse roundtrip reachable");
    }

    /// Proof: guest_addr_to_page_index arithmetic is correct for a page-aligned address.
    ///
    /// For a region with `page_offset = 0` and a concrete page size of 4096 bytes,
    /// an address at byte offset `k * 4096` within the region must yield page index `k`.
    ///
    /// `system_page_size()` calls `libc::sysconf` at runtime, which is unavailable in
    /// Kani's model.  We therefore verify the underlying arithmetic directly — the same
    /// computation performed inside `guest_addr_to_page_index` — using a concrete
    /// page-size constant matching the x86_64 Linux default (4096 bytes).  Constraining
    /// the symbolic address to be page-aligned ensures the integer division is exact.
    ///
    /// Bound: 1 region → unwind(2).
    #[kani::proof]
    #[kani::unwind(2)]
    #[kani::solver(cadical)]
    fn proof_guest_addr_to_page_index_correct() {
        const PAGE_SIZE: u64 = 4096;

        // Symbolic region: guest_addr must be page-aligned; up to 16 pages; no overflow.
        let guest_addr: u64 = kani::any_where(|&g| g % PAGE_SIZE == 0);
        let num_pages: u64 = kani::any_where(|&n: &u64| n > 0 && n <= 16);
        let size = num_pages * PAGE_SIZE;
        kani::assume(guest_addr.checked_add(size).is_some());

        let page_offset: usize = kani::any_where(|&o: &usize| o <= 1024);

        // Symbolic in-range page index within the region.
        let page_k: u64 = kani::any_where(|&k| k < num_pages);
        let addr = guest_addr + page_k * PAGE_SIZE;

        // Mirror the arithmetic from guest_addr_to_page_index (with our concrete PAGE_SIZE).
        let region_offset = addr - guest_addr;
        let page_in_region = (region_offset / PAGE_SIZE) as usize;
        let computed_index = page_offset + page_in_region;

        // The computed page-in-region offset must equal page_k exactly.
        kani::assert(
            page_in_region == page_k as usize,
            "page-in-region index must equal (addr - guest_addr) / page_size",
        );
        // The full index (with a symbolic base page_offset) must shift by page_offset.
        kani::assert(
            computed_index == page_offset + page_k as usize,
            "full page index must be page_offset + page_in_region",
        );
        kani::cover!(true, "page index computation reachable");
    }

    /// Proof: guest_to_host with 2 non-overlapping regions returns correct mapping.
    ///
    /// When two regions exist, an address in region 1 maps to region 1's host space,
    /// and an address in region 2 maps to region 2's host space.
    /// Bound: 2 regions. guest_to_host iterates 2 regions → unwind(3).
    #[kani::proof]
    #[kani::unwind(3)]
    #[kani::solver(cadical)]
    fn proof_guest_to_host_two_regions() {
        // Region A.
        let guest_a: u64 = kani::any();
        let host_a: u64 = kani::any();
        let size_a: u64 = kani::any_where(|&s| s > 0);
        kani::assume(guest_a.checked_add(size_a).is_some());
        kani::assume(host_a.checked_add(size_a).is_some());

        // Region B: must start after region A ends (non-overlapping).
        let guest_b: u64 = kani::any();
        let host_b: u64 = kani::any();
        let size_b: u64 = kani::any_where(|&s| s > 0);
        kani::assume(guest_b.checked_add(size_b).is_some());
        kani::assume(host_b.checked_add(size_b).is_some());
        kani::assume(guest_b >= guest_a + size_a); // B starts at or after A ends.

        let regions = [
            UffdRegion {
                guest_addr: guest_a,
                host_addr: host_a,
                size: size_a,
                page_offset: 0,
            },
            UffdRegion {
                guest_addr: guest_b,
                host_addr: host_b,
                size: size_b,
                page_offset: 0,
            },
        ];

        // Address in region A.
        let addr_a: u64 = kani::any();
        kani::assume(addr_a >= guest_a && addr_a < guest_a + size_a);
        let result_a = guest_to_host(&regions, addr_a);
        kani::assert(
            result_a == Some(host_a + (addr_a - guest_a)),
            "address in A maps to A's host",
        );
        kani::cover!(true, "region A mapping verified");

        // Address in region B.
        let addr_b: u64 = kani::any();
        kani::assume(addr_b >= guest_b && addr_b < guest_b + size_b);
        let result_b = guest_to_host(&regions, addr_b);
        kani::assert(
            result_b == Some(host_b + (addr_b - guest_b)),
            "address in B maps to B's host",
        );
        kani::cover!(true, "region B mapping verified");
    }
}
