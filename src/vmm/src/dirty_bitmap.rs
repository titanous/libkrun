// Copyright 2024 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Lock-free dirty page bitmap for tracking guest memory writes.
//!
//! Uses atomic operations so vCPU fault handlers can mark pages dirty
//! without acquiring a lock.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};

/// Page size on Apple Silicon (16KB).
pub const PAGE_SIZE: u64 = 16384;

/// A bitmap tracking dirty (written-to) guest memory pages.
///
/// Thread-safe: `mark_dirty` uses atomic OR operations and can be called
/// from vCPU data abort handlers without locking.
pub struct DirtyBitmap {
    /// Guest physical address of the start of the tracked region.
    base_addr: u64,
    /// Number of pages tracked.
    num_pages: usize,
    /// Bitmap stored as AtomicU64 words (each covers 64 pages).
    bitmap: Vec<AtomicU64>,
}

impl DirtyBitmap {
    /// Create a new dirty bitmap covering the given address range.
    pub fn new(base_addr: u64, size: u64) -> Self {
        let num_pages = size.div_ceil(PAGE_SIZE) as usize;
        let num_words = num_pages.div_ceil(64);
        let bitmap: Vec<AtomicU64> = (0..num_words).map(|_| AtomicU64::new(0)).collect();

        DirtyBitmap {
            base_addr,
            num_pages,
            bitmap,
        }
    }

    /// Check whether a guest address falls within this bitmap's range.
    pub fn contains(&self, guest_addr: u64) -> bool {
        guest_addr >= self.base_addr
            && guest_addr < self.base_addr + (self.num_pages as u64 * PAGE_SIZE)
    }

    /// Mark the page containing `guest_addr` as dirty.
    /// This is lock-free and safe to call from vCPU fault handlers.
    pub fn mark_dirty(&self, guest_addr: u64) {
        if !self.contains(guest_addr) {
            return;
        }
        let page_idx = ((guest_addr - self.base_addr) / PAGE_SIZE) as usize;
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        self.bitmap[word_idx].fetch_or(1u64 << bit_idx, Ordering::Relaxed);
    }

    /// Check whether the page at the given index is dirty.
    pub fn is_dirty(&self, page_idx: usize) -> bool {
        if page_idx >= self.num_pages {
            return false;
        }
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        (self.bitmap[word_idx].load(Ordering::Relaxed) >> bit_idx) & 1 != 0
    }

    /// Reset the bitmap, returning the old state as a Vec of u64 words.
    /// Each bit that was set represents a dirty page.
    pub fn reset(&self) -> Vec<u64> {
        self.bitmap
            .iter()
            .map(|word| word.swap(0, Ordering::AcqRel))
            .collect()
    }

    /// Get the base address of this bitmap's tracked region.
    pub fn base_addr(&self) -> u64 {
        self.base_addr
    }

    /// Get the number of pages tracked.
    pub fn num_pages(&self) -> usize {
        self.num_pages
    }

    /// Iterate over dirty page indices and return their guest addresses.
    /// Resets the bitmap in the process.
    pub fn drain_dirty_pages(&self) -> Vec<u64> {
        let old_bitmap = self.reset();
        let mut dirty = Vec::new();
        for (word_idx, &word) in old_bitmap.iter().enumerate() {
            if word == 0 {
                continue;
            }
            for bit_idx in 0..64 {
                if (word >> bit_idx) & 1 != 0 {
                    let page_idx = word_idx * 64 + bit_idx;
                    if page_idx < self.num_pages {
                        dirty.push(self.base_addr + (page_idx as u64 * PAGE_SIZE));
                    }
                }
            }
        }
        dirty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_dirty_tracking() {
        let bitmap = DirtyBitmap::new(0x8000_0000, 4 * PAGE_SIZE);
        assert_eq!(bitmap.num_pages(), 4);

        // Initially nothing is dirty
        assert!(!bitmap.is_dirty(0));
        assert!(!bitmap.is_dirty(1));

        // Mark page 1 dirty
        bitmap.mark_dirty(0x8000_0000 + PAGE_SIZE);
        assert!(!bitmap.is_dirty(0));
        assert!(bitmap.is_dirty(1));

        // Contains check
        assert!(bitmap.contains(0x8000_0000));
        assert!(bitmap.contains(0x8000_0000 + 3 * PAGE_SIZE));
        assert!(!bitmap.contains(0x8000_0000 + 4 * PAGE_SIZE));
        assert!(!bitmap.contains(0x7FFF_FFFF));
    }

    #[test]
    fn test_drain_dirty_pages() {
        let bitmap = DirtyBitmap::new(0x8000_0000, 4 * PAGE_SIZE);

        bitmap.mark_dirty(0x8000_0000);
        bitmap.mark_dirty(0x8000_0000 + 2 * PAGE_SIZE);

        let dirty = bitmap.drain_dirty_pages();
        assert_eq!(dirty.len(), 2);
        assert_eq!(dirty[0], 0x8000_0000);
        assert_eq!(dirty[1], 0x8000_0000 + 2 * PAGE_SIZE);

        // After drain, bitmap should be clear
        assert!(!bitmap.is_dirty(0));
        assert!(!bitmap.is_dirty(2));
    }

    #[test]
    fn test_last_valid_page() {
        // AC2.1: Page at exactly num_pages - 1 (last valid index) is tracked
        let base = 0x8000_0000;
        let bitmap = DirtyBitmap::new(base, 4 * PAGE_SIZE);

        // Mark the last valid page (page index 3, address base + 3*PAGE_SIZE)
        let last_page_addr = base + 3 * PAGE_SIZE;
        bitmap.mark_dirty(last_page_addr);

        // Drain and verify the address is present
        let dirty = bitmap.drain_dirty_pages();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0], last_page_addr);
    }

    #[test]
    fn test_out_of_bounds_ignored() {
        // AC2.2: Page at num_pages (out-of-bounds) is silently ignored; no panic
        let base = 0x8000_0000;
        let bitmap = DirtyBitmap::new(base, 4 * PAGE_SIZE);

        // Try to mark a page beyond the end (should be silently ignored)
        let out_of_bounds_addr = base + 4 * PAGE_SIZE;
        bitmap.mark_dirty(out_of_bounds_addr);

        // Drain and verify nothing is marked dirty
        let dirty = bitmap.drain_dirty_pages();
        assert_eq!(dirty.len(), 0);
    }

    #[test]
    fn test_all_pages_dirty() {
        // AC2.3: All pages marked dirty → drain_dirty_pages returns the full set
        let base = 0x8000_0000;
        let bitmap = DirtyBitmap::new(base, 4 * PAGE_SIZE);

        // Mark all 4 pages dirty
        for i in 0..4 {
            bitmap.mark_dirty(base + i * PAGE_SIZE);
        }

        // Drain and verify we get all 4 pages
        let dirty = bitmap.drain_dirty_pages();
        assert_eq!(dirty.len(), 4);
        assert_eq!(dirty[0], base);
        assert_eq!(dirty[1], base + PAGE_SIZE);
        assert_eq!(dirty[2], base + 2 * PAGE_SIZE);
        assert_eq!(dirty[3], base + 3 * PAGE_SIZE);
    }

    #[test]
    fn test_no_pages_dirty() {
        // AC2.4: No pages marked → drain_dirty_pages returns empty vec
        let base = 0x8000_0000;
        let bitmap = DirtyBitmap::new(base, 4 * PAGE_SIZE);

        // Don't mark any pages dirty, just drain
        let dirty = bitmap.drain_dirty_pages();
        assert_eq!(dirty.len(), 0);
    }

    #[test]
    fn test_duplicate_mark() {
        // AC2.5: Same page marked dirty twice → appears exactly once in drain output
        let base = 0x8000_0000;
        let bitmap = DirtyBitmap::new(base, 4 * PAGE_SIZE);

        // Mark the same page twice
        let page_addr = base + PAGE_SIZE;
        bitmap.mark_dirty(page_addr);
        bitmap.mark_dirty(page_addr);

        // Drain and verify the page appears exactly once (atomics naturally deduplicate)
        let dirty = bitmap.drain_dirty_pages();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0], page_addr);
    }

    #[test]
    fn miri_spike_pure_atomics() {
        // Miri spike: verify DirtyBitmap pure atomic ops are Miri-compatible.
        let bitmap = DirtyBitmap::new(0x0, 4);
        bitmap.mark_dirty(0x0);
        bitmap.mark_dirty(0x1000);
        let pages = bitmap.drain_dirty_pages();
        assert!(!pages.is_empty());
    }

    #[cfg(not(loom))]
    mod proptest_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Mark N distinct pages, drain_dirty_pages returns exactly those N addresses.
            #[test]
            fn prop_mark_then_drain_returns_all_pages(
                // Generate up to 32 distinct page indices in range [0, 63]
                page_indices in prop::collection::hash_set(0usize..64, 0..32)
            ) {
                let bitmap = DirtyBitmap::new(0x0, 64 * PAGE_SIZE);
                for &idx in &page_indices {
                    let addr = idx as u64 * PAGE_SIZE;
                    bitmap.mark_dirty(addr);
                }
                let drained = bitmap.drain_dirty_pages();
                prop_assert_eq!(drained.len(), page_indices.len());

                let drained_indices: std::collections::HashSet<usize> = drained
                    .iter()
                    .map(|&addr| (addr / PAGE_SIZE) as usize)
                    .collect();
                prop_assert_eq!(drained_indices, page_indices);
            }

            /// After drain, bitmap is empty.
            #[test]
            fn prop_drain_empties_bitmap(
                page_indices in prop::collection::hash_set(0usize..64, 1..32)
            ) {
                let bitmap = DirtyBitmap::new(0x0, 64 * PAGE_SIZE);
                for &idx in &page_indices {
                    bitmap.mark_dirty(idx as u64 * PAGE_SIZE);
                }
                let _ = bitmap.drain_dirty_pages();
                // Second drain should return empty
                let second_drain = bitmap.drain_dirty_pages();
                prop_assert!(second_drain.is_empty());
            }

            /// mark_dirty is idempotent: marking same page twice yields count of 1.
            #[test]
            fn prop_mark_idempotent(page_idx in 0usize..64) {
                let bitmap = DirtyBitmap::new(0x0, 64 * PAGE_SIZE);
                let addr = page_idx as u64 * PAGE_SIZE;
                bitmap.mark_dirty(addr);
                bitmap.mark_dirty(addr);
                let drained = bitmap.drain_dirty_pages();
                prop_assert_eq!(drained.len(), 1);
            }
        }
    }

    #[cfg(loom)]
    mod loom_tests {
        use super::*;
        use loom::sync::Arc;
        use loom::thread;

        /// Concurrent mark_dirty and drain_dirty_pages: no page lost.
        ///
        /// One thread marks a page dirty (Relaxed fetch_or).
        /// Another thread drains all dirty pages (AcqRel swap).
        /// After both complete, the page must appear in exactly one place.
        #[test]
        fn loom_mark_and_drain_no_page_lost() {
            loom::model(|| {
                // Use a small bitmap to keep loom's state space manageable.
                let bitmap = Arc::new(DirtyBitmap::new(0x0, 2 * PAGE_SIZE));

                let b1 = Arc::clone(&bitmap);
                let marker = thread::spawn(move || {
                    b1.mark_dirty(0x0); // page 0
                });

                let b2 = Arc::clone(&bitmap);
                let drainer = thread::spawn(move || b2.drain_dirty_pages());

                marker.join().unwrap();
                let drained = drainer.join().unwrap();

                // After both threads complete, collect remaining.
                // The page must be in drained OR in a subsequent drain (never lost).
                let remaining = bitmap.drain_dirty_pages();
                let page_found = drained.contains(&0x0) || remaining.contains(&0x0);
                assert!(
                    page_found,
                    "page 0x0 was lost: drained={:?}, remaining={:?}",
                    drained, remaining
                );
            });
        }

        /// Two concurrent marker threads: both pages must be present after draining.
        #[test]
        fn loom_two_markers_both_present() {
            loom::model(|| {
                let bitmap = Arc::new(DirtyBitmap::new(0x0, 2 * PAGE_SIZE));

                let b1 = Arc::clone(&bitmap);
                let m1 = thread::spawn(move || {
                    b1.mark_dirty(0x0);
                });

                let b2 = Arc::clone(&bitmap);
                let m2 = thread::spawn(move || {
                    b2.mark_dirty(PAGE_SIZE);
                });

                m1.join().unwrap();
                m2.join().unwrap();

                let drained = bitmap.drain_dirty_pages();
                assert_eq!(
                    drained.len(),
                    2,
                    "expected 2 dirty pages, got {:?}",
                    drained
                );
            });
        }
    }
}
