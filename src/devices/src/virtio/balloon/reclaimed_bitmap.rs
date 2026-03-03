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
}
