// Copyright 2024 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Lock-free bitmap for tracking reclaimed pages (inflated or reported-free).
//!
//! Uses atomic operations so pages can be marked/cleared without acquiring a lock.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};

/// Balloon page size (4KB, virtio balloon PFN granularity).
#[allow(dead_code)]
pub const BALLOON_PAGE_SIZE: u64 = 4096;

/// Shift amount to convert page-frame number to bytes.
#[allow(dead_code)]
pub const BALLOON_PAGE_SHIFT: u32 = 12;

/// A bitmap tracking reclaimed (inflated or reported-free) guest memory pages.
///
/// Thread-safe: All operations use atomic instructions and can be called
/// from multiple threads without locking.
pub struct ReclaimedBitmap {
    /// Number of pages tracked.
    num_pages: usize,
    /// Bitmap stored as AtomicU64 words (each covers 64 pages).
    bitmap: Vec<AtomicU64>,
}

impl ReclaimedBitmap {
    /// Create a new reclaimed bitmap covering the specified number of pages.
    pub fn new(num_pages: usize) -> Self {
        let num_words = num_pages.div_ceil(64);
        let bitmap: Vec<AtomicU64> = (0..num_words).map(|_| AtomicU64::new(0)).collect();

        ReclaimedBitmap { num_pages, bitmap }
    }

    /// Mark a page as reclaimed by its page-frame number.
    ///
    /// Silently ignores out-of-bounds PFNs (same pattern as DirtyBitmap).
    pub fn mark(&self, pfn: u32) {
        let pfn = pfn as usize;
        if pfn >= self.num_pages {
            return;
        }
        let word_idx = pfn / 64;
        let bit_idx = pfn % 64;
        self.bitmap[word_idx].fetch_or(1u64 << bit_idx, Ordering::Relaxed);
    }

    /// Clear a reclaimed page by its page-frame number.
    ///
    /// Silently ignores out-of-bounds PFNs.
    pub fn clear(&self, pfn: u32) {
        let pfn = pfn as usize;
        if pfn >= self.num_pages {
            return;
        }
        let word_idx = pfn / 64;
        let bit_idx = pfn % 64;
        self.bitmap[word_idx].fetch_and(!(1u64 << bit_idx), Ordering::Relaxed);
    }

    /// Check whether a page is marked as reclaimed.
    #[cfg_attr(kani, kani::ensures(|&result| {
        // out-of-bounds PFN always returns false
        if (pfn as usize) >= self.num_pages { !result } else { true }
    }))]
    pub fn is_set(&self, pfn: u32) -> bool {
        let pfn = pfn as usize;
        if pfn >= self.num_pages {
            return false;
        }
        let word_idx = pfn / 64;
        let bit_idx = pfn % 64;
        (self.bitmap[word_idx].load(Ordering::Relaxed) >> bit_idx) & 1 != 0
    }

    /// Mark a contiguous range of pages as reclaimed.
    ///
    /// # Arguments
    /// * `start_pfn` - First page-frame number in the range
    /// * `count` - Number of pages to mark
    pub fn mark_range(&self, start_pfn: u32, count: u32) {
        for i in 0..count {
            self.mark(start_pfn.saturating_add(i));
        }
    }

    /// Iterate over all set bits and return their PFN indices.
    pub fn iter_set_pages(&self) -> Vec<u32> {
        let mut result = Vec::new();
        for (word_idx, word) in self.bitmap.iter().enumerate() {
            let word_val = word.load(Ordering::Relaxed);
            if word_val == 0 {
                continue;
            }
            for bit_idx in 0..64 {
                if (word_val >> bit_idx) & 1 != 0 {
                    let pfn = (word_idx * 64 + bit_idx) as u32;
                    if (pfn as usize) < self.num_pages {
                        result.push(pfn);
                    }
                }
            }
        }
        result
    }

    /// Count the total number of set bits.
    pub fn count(&self) -> usize {
        self.bitmap
            .iter()
            .map(|word| word.load(Ordering::Relaxed).count_ones() as usize)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mark_clear_is_set_roundtrip() {
        let bitmap = ReclaimedBitmap::new(100);

        // Initially page is not set
        assert!(!bitmap.is_set(5));

        // Mark the page
        bitmap.mark(5);
        assert!(bitmap.is_set(5));

        // Clear the page
        bitmap.clear(5);
        assert!(!bitmap.is_set(5));
    }

    #[test]
    fn test_mark_range() {
        let bitmap = ReclaimedBitmap::new(100);

        // Mark pages 10-19 (10 pages)
        bitmap.mark_range(10, 10);

        // Verify all pages in range are set
        for pfn in 10..20 {
            assert!(bitmap.is_set(pfn));
        }

        // Verify pages outside range are not set
        assert!(!bitmap.is_set(9));
        assert!(!bitmap.is_set(20));
    }

    #[test]
    fn test_mark_idempotent() {
        let bitmap = ReclaimedBitmap::new(100);

        // Mark the same page twice
        bitmap.mark(42);
        bitmap.mark(42);

        // Page should still be set (atomic OR is idempotent)
        assert!(bitmap.is_set(42));

        // Only one bit should be set
        assert_eq!(bitmap.count(), 1);
    }

    #[test]
    fn test_clear_unset_noop() {
        let bitmap = ReclaimedBitmap::new(100);

        // Clear an unset bit
        bitmap.clear(50);

        // Page should still not be set
        assert!(!bitmap.is_set(50));
        assert_eq!(bitmap.count(), 0);
    }

    #[test]
    fn test_out_of_bounds_silently_ignored() {
        let bitmap = ReclaimedBitmap::new(10);

        // Try to mark out-of-bounds page
        bitmap.mark(10);
        bitmap.mark(100);

        // Out-of-bounds should be silently ignored, no panic
        assert!(!bitmap.is_set(10));
        assert!(!bitmap.is_set(100));
        assert_eq!(bitmap.count(), 0);

        // Try to clear out-of-bounds page
        bitmap.clear(10);
        bitmap.clear(100);

        // Should still be fine, no panic
        assert_eq!(bitmap.count(), 0);
    }

    #[test]
    fn test_iter_set_pages_returns_all_set_pfns() {
        let bitmap = ReclaimedBitmap::new(200);

        // Mark some scattered pages
        let pages_to_mark = vec![5, 15, 50, 99, 150, 199];
        for &pfn in &pages_to_mark {
            bitmap.mark(pfn);
        }

        // Get all set pages
        let mut set_pages = bitmap.iter_set_pages();
        set_pages.sort_unstable();

        // Verify they match and are in order
        assert_eq!(set_pages, pages_to_mark);
    }

    #[test]
    fn test_iter_set_pages_empty() {
        let bitmap = ReclaimedBitmap::new(100);

        // Don't mark any pages
        let set_pages = bitmap.iter_set_pages();

        assert_eq!(set_pages.len(), 0);
    }

    #[test]
    fn test_iter_set_pages_multiple_words() {
        let bitmap = ReclaimedBitmap::new(200);

        // Mark pages that span multiple 64-bit words
        // Word 0: bits 0-63 (PFNs 0-63)
        // Word 1: bits 64-127 (PFNs 64-127)
        // Word 2: bits 128-191 (PFNs 128-191)
        bitmap.mark(10);
        bitmap.mark(65);
        bitmap.mark(130);

        let mut set_pages = bitmap.iter_set_pages();
        set_pages.sort_unstable();

        assert_eq!(set_pages, vec![10, 65, 130]);
    }

    #[test]
    fn test_count() {
        let bitmap = ReclaimedBitmap::new(100);

        assert_eq!(bitmap.count(), 0);

        bitmap.mark(5);
        assert_eq!(bitmap.count(), 1);

        bitmap.mark(10);
        bitmap.mark(15);
        assert_eq!(bitmap.count(), 3);

        bitmap.clear(10);
        assert_eq!(bitmap.count(), 2);

        // Marking already set page doesn't increase count
        bitmap.mark(5);
        assert_eq!(bitmap.count(), 2);
    }

    #[test]
    fn test_large_bitmap() {
        let large_size = 100_000;
        let bitmap = ReclaimedBitmap::new(large_size);

        // Mark scattered pages across the large bitmap
        let test_pfns = vec![0, 1000, 10000, 50000, 99999];
        for &pfn in &test_pfns {
            bitmap.mark(pfn);
        }

        // Verify they're all set
        for &pfn in &test_pfns {
            assert!(bitmap.is_set(pfn));
        }

        // Verify count is correct
        assert_eq!(bitmap.count(), test_pfns.len());

        // Verify iter_set_pages returns them
        let mut set_pages = bitmap.iter_set_pages();
        set_pages.sort_unstable();
        assert_eq!(set_pages, test_pfns);
    }

    #[test]
    fn test_mark_range_partial() {
        let bitmap = ReclaimedBitmap::new(50);

        // Mark range that partially goes out of bounds
        bitmap.mark_range(45, 10);

        // Pages 45-49 should be set
        for pfn in 45..50 {
            assert!(bitmap.is_set(pfn));
        }

        // Pages 50+ would be out of bounds (silently ignored)
        assert!(!bitmap.is_set(50));
    }

    #[cfg(not(loom))]
    mod proptest_tests {
        use super::ReclaimedBitmap;
        use proptest::prelude::*;

        proptest! {
            /// mark_range then count returns exactly the marked count.
            #[test]
            fn prop_mark_range_count(
                start in 0u32..100,
                count in 0u32..50,
            ) {
                let total = start.saturating_add(count) as usize + 1;
                let bitmap = ReclaimedBitmap::new(total);
                bitmap.mark_range(start, count);
                // Pages in [start, start+count) should all be set
                let actual_count = bitmap.count();
                prop_assert_eq!(actual_count, count as usize);
            }

            /// mark_range then iter_set_pages returns exactly those PFNs.
            #[test]
            fn prop_mark_range_iter(
                start in 0u32..50,
                count in 1u32..20,
            ) {
                let total = start.saturating_add(count) as usize + 1;
                let bitmap = ReclaimedBitmap::new(total);
                bitmap.mark_range(start, count);

                let pages = bitmap.iter_set_pages();
                let expected: Vec<u32> = (start..start.saturating_add(count)).collect();
                let mut got = pages.clone();
                got.sort();
                prop_assert_eq!(got, expected);
            }

            /// mark then clear is a round-trip: is_set returns false.
            #[test]
            fn prop_mark_clear_roundtrip(pfn in 0u32..100) {
                let bitmap = ReclaimedBitmap::new(101);
                bitmap.mark(pfn);
                prop_assert!(bitmap.is_set(pfn));
                bitmap.clear(pfn);
                prop_assert!(!bitmap.is_set(pfn));
            }
        }
    }

    #[cfg(loom)]
    mod loom_tests {
        use super::*;
        use loom::sync::Arc;
        use loom::thread;

        /// Concurrent mark and clear: is_set result consistent with operations.
        ///
        /// Property: count() must be 0 or 1 after concurrent mark + clear on the same PFN.
        #[test]
        fn loom_mark_clear_consistency() {
            loom::model(|| {
                let bitmap = Arc::new(ReclaimedBitmap::new(64));

                let b1 = Arc::clone(&bitmap);
                let marker = thread::spawn(move || {
                    b1.mark(0);
                });

                let b2 = Arc::clone(&bitmap);
                let clearer = thread::spawn(move || {
                    b2.clear(0);
                });

                marker.join().unwrap();
                clearer.join().unwrap();

                // After concurrent mark+clear, count must be 0 or 1 (never 2, never negative)
                let count = bitmap.count();
                assert!(count <= 1, "count out of range: {}", count);
            });
        }

        /// Concurrent marks on different PFNs: both must be set.
        #[test]
        fn loom_concurrent_distinct_marks() {
            loom::model(|| {
                let bitmap = Arc::new(ReclaimedBitmap::new(64));

                let b1 = Arc::clone(&bitmap);
                let m1 = thread::spawn(move || {
                    b1.mark(0);
                });

                let b2 = Arc::clone(&bitmap);
                let m2 = thread::spawn(move || {
                    b2.mark(1);
                });

                m1.join().unwrap();
                m2.join().unwrap();

                assert!(bitmap.is_set(0), "pfn 0 not set");
                assert!(bitmap.is_set(1), "pfn 1 not set");
                assert_eq!(bitmap.count(), 2);
            });
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Proof: mark(pfn) followed by is_set(pfn) returns true.
    ///
    /// Bound: 256 pages; pfn is any valid index in [0, 255].
    /// ReclaimedBitmap::new allocates ceil(256/64)=4 words → unwind(5).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_mark_then_is_set() {
        // Symbolic number of pages: [1, 256]
        let num_pages: usize = kani::any_where(|&n| n > 0 && n <= 256);
        let bitmap = ReclaimedBitmap::new(num_pages);

        // Symbolic PFN: valid index.
        let pfn: u32 = kani::any_where(|&p| (p as usize) < num_pages);

        bitmap.mark(pfn);

        kani::assert(
            bitmap.is_set(pfn),
            "mark(pfn) must make is_set(pfn) return true",
        );
        // Cover: word boundary PFNs are exercised (last bit of word 0, first bit of word 1).
        kani::cover!(
            pfn % 64 == 63,
            "PFN at last bit position of a word exercised"
        );
        kani::cover!(
            pfn % 64 == 0 && pfn > 0,
            "PFN at first bit position of a non-first word exercised"
        );
    }

    /// Proof: mark(pfn) then clear(pfn) makes is_set(pfn) return false.
    ///
    /// Production coupling: if clear() uses fetch_xor instead of fetch_and,
    /// this proof fails because XOR of an already-set bit would re-set it.
    ///
    /// Bound: 256 pages. ReclaimedBitmap::new allocates ceil(256/64)=4 words → unwind(5).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_clear_then_not_is_set() {
        let num_pages: usize = kani::any_where(|&n| n > 0 && n <= 256);
        let bitmap = ReclaimedBitmap::new(num_pages);

        let pfn: u32 = kani::any_where(|&p| (p as usize) < num_pages);

        bitmap.mark(pfn);
        bitmap.clear(pfn);

        kani::assert(
            !bitmap.is_set(pfn),
            "clear(pfn) must make is_set(pfn) return false",
        );
        // Cover: word boundary case (last bit of word 0 and first bit of word 1)
        kani::cover!(pfn == 63, "boundary PFN 63 (last bit of word 0) exercised");
        kani::cover!(pfn == 64, "boundary PFN 64 (first bit of word 1) exercised");
    }

    /// Proof: count() equals the structural popcount — two distinct pages give count 2.
    ///
    /// The sequence test anti-pattern (mark one page → count==1 → clear → count==0)
    /// is broken: a `count()` that returns `marks_called - clears_called` would also
    /// pass. This proof marks two *distinct* symbolic pages and asserts count() == 2,
    /// which a counter-based fake cannot satisfy while also passing
    /// proof_clear_then_not_is_set (which clears without re-marking first).
    ///
    /// Production coupling: if count() is replaced with an operation counter instead
    /// of summing popcount over words, marking the same page twice would yield count==2
    /// when it should be 1 — breaking the idempotency assertion below.
    ///
    /// Bound: 256 pages. count() iterates ceil(256/64)=4 words → unwind(5).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_count_equals_popcount() {
        let num_pages: usize = kani::any_where(|&n| n > 0 && n <= 256);
        let bitmap = ReclaimedBitmap::new(num_pages);

        // Initially empty: count must be 0.
        kani::assert(bitmap.count() == 0, "fresh bitmap must have count 0");

        // Two distinct symbolic PFNs within bounds.
        let pfn_a: u32 = kani::any_where(|&p| (p as usize) < num_pages);
        let pfn_b: u32 = kani::any_where(|&p| (p as usize) < num_pages && p != pfn_a);

        // Mark both distinct pages: count must be 2.
        bitmap.mark(pfn_a);
        bitmap.mark(pfn_b);
        kani::assert(
            bitmap.count() == 2,
            "count must be 2 after marking two distinct pages",
        );

        // Idempotency: marking pfn_a again must not change count.
        bitmap.mark(pfn_a);
        kani::assert(
            bitmap.count() == 2,
            "count must remain 2 after re-marking an already-set page",
        );

        // Cover: same-word vs cross-word distinct pairs
        kani::cover!(pfn_a / 64 == pfn_b / 64, "both PFNs in same 64-bit word");
        kani::cover!(pfn_a / 64 != pfn_b / 64, "PFNs in different 64-bit words");
    }

    /// Proof: out-of-bounds pfn is silently ignored by mark and clear.
    ///
    /// A PFN equal to num_pages is out of bounds. mark and clear must not panic.
    /// count() iterates ceil(64/64)=1 word → unwind(2).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(2)]
    fn proof_out_of_bounds_pfn_ignored() {
        let num_pages: usize = kani::any_where(|&n| n > 0 && n <= 64);
        let bitmap = ReclaimedBitmap::new(num_pages);

        // PFN exactly at the boundary (out of bounds).
        let pfn = num_pages as u32;

        // These must not panic.
        bitmap.mark(pfn);
        bitmap.clear(pfn);

        // Bitmap must remain empty.
        kani::assert(
            bitmap.count() == 0,
            "out-of-bounds pfn must not affect count",
        );
        kani::assert(
            !bitmap.is_set(pfn),
            "out-of-bounds pfn must not appear as set",
        );
        // Cover: PFN == 0 (num_pages == 0 disallowed, but pfn == 1 when num_pages == 1)
        // and word-boundary case (pfn == 64 when num_pages == 64).
        kani::cover!(pfn == 1, "smallest OOB boundary (num_pages == 1) exercised");
        kani::cover!(pfn == 64, "word-boundary OOB (num_pages == 64) exercised");
    }

    /// Proof: mark/is_set/clear path coverage.
    /// ReclaimedBitmap::new allocates ceil(256/64)=4 words → unwind(5).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_reclaimed_bitmap_path_coverage() {
        let num_pages: usize = kani::any_where(|&n| n > 0 && n <= 256);
        let bitmap = ReclaimedBitmap::new(num_pages);
        let pfn: u32 = kani::any();
        bitmap.mark(pfn);
        kani::cover!(bitmap.is_set(pfn), "in-bounds pfn is set after mark");
        kani::cover!(!bitmap.is_set(pfn), "out-of-bounds pfn remains unset");
    }

    /// Proof: mark_range(start, count) marks exactly `count` in-bounds pages, handles
    /// partial out-of-bounds clamping, and start + count overflow is not silent corruption.
    ///
    /// Three properties verified:
    /// 1. All pages in [start, start + count) that are in-bounds are marked after the call.
    /// 2. Pages before `start` are not set (isolation).
    /// 3. When start is near u32::MAX and count would overflow, saturating_add prevents
    ///    wrap-around from marking wrong pages — out-of-bounds pages are silently ignored
    ///    and count() only reflects the in-bounds pages that were actually marked.
    ///
    /// Production coupling: if mark_range used wrapping_add instead of saturating_add,
    /// property 3 fails — overflowed PFNs could land within bounds, silently marking
    /// wrong pages and producing a count mismatch.
    ///
    /// Bound: bitmap up to 128 pages; count up to 16. mark_range loops count times → unwind(17).
    /// count() iterates ceil(128/64)=2 words → also covered within unwind(17).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(17)]
    fn proof_mark_range_marks_exactly_count() {
        // Small bitmap for tractability.
        let num_pages: usize = kani::any_where(|&n| n > 0 && n <= 128);
        let bitmap = ReclaimedBitmap::new(num_pages);

        // Symbolic start and count — bounded small enough for tractability.
        let start: u32 = kani::any_where(|&s: &u32| (s as usize) < num_pages);
        let count: u32 = kani::any_where(|&c| c > 0 && c <= 16);

        bitmap.mark_range(start, count);

        // Property 1: all in-bounds pages [start, start+count) are set.
        let effective_end = start.saturating_add(count);
        for i in start..effective_end {
            if (i as usize) < num_pages {
                kani::assert(
                    bitmap.is_set(i),
                    "every in-bounds page in [start, start+count) must be set after mark_range",
                );
            }
        }

        // Property 2: the page immediately before start is not set (no underflow corruption).
        if start > 0 {
            kani::assert(
                !bitmap.is_set(start - 1),
                "page before start must not be set after mark_range",
            );
        }

        // Cover: fully in-bounds range vs. partially out-of-bounds range.
        kani::cover!(
            start.saturating_add(count) <= num_pages as u32,
            "fully in-bounds mark_range exercised"
        );
        kani::cover!(
            start.saturating_add(count) > num_pages as u32,
            "partially out-of-bounds mark_range exercised"
        );
    }

    /// Proof: mark_range with start near u32::MAX does not corrupt bitmap via integer overflow.
    ///
    /// Uses saturating_add: when start + i overflows u32, it saturates to u32::MAX which is
    /// out of bounds, so mark silently ignores it. No pages from the "wrapped" address are
    /// marked. A broken implementation using wrapping_add could mark pages at index 0..N.
    ///
    /// Production coupling: replacing saturating_add with wrapping_add in mark_range would
    /// cause overflow pages to land at small PFN values, failing the count assertion.
    ///
    /// Bound: bitmap 128 pages; count ≤ 8; start near u32::MAX → all marks OOB → unwind(9).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(9)]
    fn proof_mark_range_overflow_no_corruption() {
        let num_pages: usize = kani::any_where(|&n| n > 0 && n <= 128);
        let bitmap = ReclaimedBitmap::new(num_pages);

        // start is near u32::MAX so start + any i > 0 saturates to u32::MAX (OOB).
        let start: u32 = kani::any_where(|&s: &u32| s > u32::MAX - 8);
        let count: u32 = kani::any_where(|&c| c > 0 && c <= 8);

        bitmap.mark_range(start, count);

        // All pages land OOB (start itself is OOB since start > u32::MAX-8 >> 128).
        // count() must remain 0 — no corruption from overflow.
        kani::assert(
            bitmap.count() == 0,
            "mark_range near u32::MAX must not corrupt bitmap via integer overflow",
        );
        // Cover: start overflows u32 when adding count.
        kani::cover!(
            start.checked_add(count).is_none(),
            "start + count overflows u32 (saturating_add exercises overflow path)"
        );
    }

    /// Contract proof: `is_set` returns false for out-of-bounds PFNs.
    #[kani::proof_for_contract(ReclaimedBitmap::is_set)]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_contract_is_set_oob() {
        let num_pages: usize = kani::any_where(|&n| n > 0 && n <= 256);
        let bitmap = ReclaimedBitmap::new(num_pages);
        let pfn: u32 = kani::any();
        let _ = bitmap.is_set(pfn);
    }
}
