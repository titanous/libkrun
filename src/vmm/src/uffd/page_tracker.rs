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
