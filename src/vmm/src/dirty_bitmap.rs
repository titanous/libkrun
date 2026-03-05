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

#[cfg(kani)]
mod verification {
    use super::*;

    /// Proof: mark_dirty never panics for any address with bitmap up to 256 pages.
    ///
    /// Kani exhaustively explores all combinations of (guest_addr, size) where
    /// size is at most 256 pages worth of bytes. For every combination, mark_dirty
    /// must complete without panic or out-of-bounds array access.
    ///
    /// Bound: 256 pages * PAGE_SIZE (16384) = 4,194,304 bytes maximum bitmap size.
    /// DirtyBitmap::new creates Vec of ceil(num_pages/64) AtomicU64 words.
    /// With num_pages <= 256: up to 4 words → Vec init loop needs unwind(5).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_mark_dirty_no_panic() {
        // Symbolic address: any possible u64 value.
        let guest_addr: u64 = kani::any();

        // Symbolic size: constrain to keep verification tractable using any_where.
        // DirtyBitmap::new uses PAGE_SIZE = 16384 internally.
        // 256 pages = 4,194,304 bytes.
        let num_pages: u64 = kani::any_where(|&n| n > 0 && n <= 256);
        let size = num_pages * 16384; // PAGE_SIZE = 16384

        // Base address: use 0 for simplicity; the address arithmetic is relative.
        let bitmap = DirtyBitmap::new(0, size);

        // This must not panic regardless of guest_addr value.
        bitmap.mark_dirty(guest_addr);
        kani::cover!(true, "mark_dirty no-panic path reachable");
    }

    /// Proof: mark_dirty on an in-bounds address is reflected by drain_dirty_pages.
    ///
    /// If guest_addr is within [base_addr, base_addr + size), then after mark_dirty,
    /// drain_dirty_pages must return a non-empty vec containing the marked page.
    ///
    /// Bound: 4 pages (keeps state space small; the bitmap logic is identical for N pages).
    /// drain_dirty_pages outer loop: 1 word (ceil(4/64)=1) → unwind(2).
    /// Inner bit-scan loop: 64 iterations → unwind(65).
    /// Use max: unwind(65) covers both.
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(66)]
    fn proof_mark_dirty_in_bounds_recorded() {
        // Symbolic base_addr: page-aligned and no overflow with 4-page region.
        let num_pages: u64 = 4;
        let base_addr: u64 = kani::any_where(|&b: &u64| {
            b % 16384 == 0 && b.checked_add(num_pages * 16384).is_some()
        });
        let bitmap = DirtyBitmap::new(base_addr, num_pages * 16384);

        // Symbolic in-bounds address: within [base_addr, base_addr + 4 * PAGE_SIZE).
        let page_idx: u64 = kani::any_where(|&i: &u64| i < num_pages);
        let guest_addr = base_addr + page_idx * 16384;

        bitmap.mark_dirty(guest_addr);

        let dirty = bitmap.drain_dirty_pages();
        kani::assert(!dirty.is_empty(), "in-bounds mark_dirty must be recorded");
        // Check the marked page address is in the drained set.
        // Avoid Vec::contains which creates an unbounded slice iteration loop;
        // instead check the specific page index directly (4 pages → at most 4 entries).
        let page_addr = base_addr + (page_idx * 16384);
        let mut found = false;
        if dirty.len() > 0 && dirty[0] == page_addr {
            found = true;
        }
        if dirty.len() > 1 && dirty[1] == page_addr {
            found = true;
        }
        if dirty.len() > 2 && dirty[2] == page_addr {
            found = true;
        }
        if dirty.len() > 3 && dirty[3] == page_addr {
            found = true;
        }
        kani::assert(found, "drained pages must contain marked address");
        kani::cover!(true, "in-bounds mark_dirty recorded path reachable");
    }

    /// Proof: mark_dirty on an out-of-bounds address leaves the bitmap empty.
    ///
    /// If guest_addr is outside [base_addr, base_addr + size), then drain_dirty_pages
    /// must return empty.
    ///
    /// Bound: 4 pages. drain_dirty_pages outer loop: 1 word → unwind(2).
    /// Inner bit-scan: 64 iterations → unwind(65).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(65)]
    fn proof_mark_dirty_out_of_bounds_no_effect() {
        // Symbolic base_addr: page-aligned and no overflow with 4-page region.
        let num_pages: u64 = 4;
        let base_addr: u64 = kani::any_where(|&b: &u64| {
            b % 16384 == 0 && b.checked_add(num_pages * 16384).is_some()
        });
        let bitmap = DirtyBitmap::new(base_addr, num_pages * 16384);

        // Symbolic out-of-bounds address: before base_addr or at/beyond region end.
        let region_end = base_addr + num_pages * 16384;
        let guest_addr: u64 = kani::any_where(|&a| a < base_addr || a >= region_end);

        bitmap.mark_dirty(guest_addr);

        let dirty = bitmap.drain_dirty_pages();
        kani::assert(
            dirty.is_empty(),
            "out-of-bounds mark_dirty must not record anything",
        );
        kani::cover!(true, "out-of-bounds silent path reachable");
    }

    /// Proof: both in-bounds and out-of-bounds paths are reachable for mark_dirty.
    /// drain_dirty_pages: 1 word outer loop → unwind(2); 64-bit inner loop → unwind(65).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(65)]
    fn proof_mark_dirty_both_paths_reachable() {
        let bitmap = DirtyBitmap::new(0, 4 * 16384);
        let guest_addr: u64 = kani::any();
        bitmap.mark_dirty(guest_addr);
        let dirty = bitmap.drain_dirty_pages();
        kani::cover!(!dirty.is_empty(), "in-bounds mark is recorded");
        kani::cover!(dirty.is_empty(), "out-of-bounds mark is silent");
    }

    // ── is_dirty / lifecycle / word-boundary proofs ───────────────────────────

    /// Proof: is_dirty returns false for any out-of-bounds page index.
    ///
    /// A concrete 4-page bitmap (1 word) is used to keep verification tractable.
    /// Page indices 4..u64::MAX must all return false.
    /// outer loop: 1 word → unwind(2)
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(2)]
    fn proof_is_dirty_out_of_bounds_returns_false() {
        // Concrete 4-page bitmap to avoid symbolic Vec construction overhead.
        let bitmap = DirtyBitmap::new(0, 4 * PAGE_SIZE);

        // Symbolic out-of-bounds page index.
        let page_idx: usize = kani::any_where(|&i| i >= 4);

        kani::assert(
            !bitmap.is_dirty(page_idx),
            "is_dirty must return false for out-of-bounds page index",
        );
        kani::cover!(true, "out-of-bounds is_dirty false path covered");
    }

    /// Proof: is_dirty reflects mark_dirty for in-bounds pages.
    ///
    /// After mark_dirty on page P, is_dirty(P) must return true.
    /// Uses a concrete 4-page bitmap; page_idx is symbolic in [0, 3].
    /// No drain needed — is_dirty reads the live bitmap directly.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_is_dirty_reflects_mark_dirty() {
        let bitmap = DirtyBitmap::new(0, 4 * PAGE_SIZE);
        let page_idx: usize = kani::any_where(|&i| i < 4);
        let guest_addr = page_idx as u64 * PAGE_SIZE;

        bitmap.mark_dirty(guest_addr);

        kani::assert(
            bitmap.is_dirty(page_idx),
            "is_dirty must return true after mark_dirty on the same page",
        );
        kani::cover!(true, "is_dirty reflects mark_dirty path covered");
    }

    /// Proof: mark_dirty at page A does not affect is_dirty for a distinct page B.
    ///
    /// Isolation property: writes to one page must not corrupt adjacent pages.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_mark_dirty_only_affects_target_page() {
        let bitmap = DirtyBitmap::new(0, 4 * PAGE_SIZE);
        let page_a: usize = kani::any_where(|&i| i < 4);
        let page_b: usize = kani::any_where(|&i| i < 4);
        kani::assume(page_a != page_b);

        let addr_a = page_a as u64 * PAGE_SIZE;
        bitmap.mark_dirty(addr_a);

        // Page B must still be clean.
        kani::assert(
            !bitmap.is_dirty(page_b),
            "marking page A dirty must not affect page B",
        );
        kani::cover!(true, "isolation proof path covered");
    }

    /// Proof: full lifecycle — mark, drain, all pages become clean.
    ///
    /// After drain_dirty_pages, the bitmap is fully reset: is_dirty returns false
    /// for every page index.
    /// drain_dirty_pages outer loop: 1 word → unwind(2); inner bit-scan → unwind(65).
    /// Combined: unwind(66).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(66)]
    fn proof_lifecycle_mark_drain_clean() {
        let bitmap = DirtyBitmap::new(0, 4 * PAGE_SIZE);

        // Mark all 4 pages dirty.
        for i in 0..4usize {
            bitmap.mark_dirty(i as u64 * PAGE_SIZE);
        }

        // Drain clears the bitmap.
        let _ = bitmap.drain_dirty_pages();

        // All pages must now be clean.
        kani::assert(!bitmap.is_dirty(0), "page 0 must be clean after drain");
        kani::assert(!bitmap.is_dirty(1), "page 1 must be clean after drain");
        kani::assert(!bitmap.is_dirty(2), "page 2 must be clean after drain");
        kani::assert(!bitmap.is_dirty(3), "page 3 must be clean after drain");
        kani::cover!(true, "lifecycle mark-drain-clean path covered");
    }

    /// Proof: page 63 (last bit of word 0) is tracked correctly.
    ///
    /// Word boundary: page 63 is the highest bit of the first AtomicU64 word.
    /// A bitmap with 128 pages covers 2 words; page 63 is the MSB of word 0.
    /// unwind(66): drain inner bit-scan loop (64 iterations) + 1.
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(66)]
    fn proof_word_boundary_last_bit_of_word0() {
        // 128 pages = 2 words; pages 0-63 in word 0, pages 64-127 in word 1.
        let bitmap = DirtyBitmap::new(0, 128 * PAGE_SIZE);
        let page_63_addr = 63u64 * PAGE_SIZE;

        bitmap.mark_dirty(page_63_addr);

        kani::assert(
            bitmap.is_dirty(63),
            "page 63 (word 0 MSB) must be dirty after mark",
        );

        let dirty = bitmap.drain_dirty_pages();
        let mut found = false;
        for i in 0..dirty.len() {
            if dirty[i] == page_63_addr {
                found = true;
            }
        }
        kani::assert(
            found,
            "page 63 address must appear in drain_dirty_pages result",
        );
        kani::cover!(true, "word-boundary page-63 proof covered");
    }

    /// Proof: page 64 (first bit of word 1) is tracked correctly.
    ///
    /// Word boundary: page 64 is the LSB of the second AtomicU64 word.
    /// unwind(66): drain inner bit-scan loop (64 iterations) + 1.
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(66)]
    fn proof_word_boundary_first_bit_of_word1() {
        // 128 pages = 2 words; page 64 is the LSB of word 1.
        let bitmap = DirtyBitmap::new(0, 128 * PAGE_SIZE);
        let page_64_addr = 64u64 * PAGE_SIZE;

        bitmap.mark_dirty(page_64_addr);

        kani::assert(
            bitmap.is_dirty(64),
            "page 64 (word 1 LSB) must be dirty after mark",
        );

        let dirty = bitmap.drain_dirty_pages();
        let mut found = false;
        for i in 0..dirty.len() {
            if dirty[i] == page_64_addr {
                found = true;
            }
        }
        kani::assert(
            found,
            "page 64 address must appear in drain_dirty_pages result",
        );
        kani::cover!(true, "word-boundary page-64 proof covered");
    }

    /// Proof: reset() returns the previous state and leaves bitmap clean.
    ///
    /// After marking page 0 dirty and calling reset(), the returned words
    /// must have bit 0 set (page 0 was dirty) and is_dirty(0) must then
    /// return false (bitmap cleared by swap-with-zero).
    /// 4-page bitmap: 1 word → unwind(2).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(2)]
    fn proof_reset_clears_and_returns_old_state() {
        let bitmap = DirtyBitmap::new(0, 4 * PAGE_SIZE);
        bitmap.mark_dirty(0); // mark page 0 (addr 0)

        let old = bitmap.reset();

        // The returned word must have bit 0 set (page 0 was dirty).
        kani::assert(!old.is_empty(), "reset must return at least one word");
        kani::assert(
            old[0] & 1 != 0,
            "bit 0 of old word must be set (page 0 was dirty)",
        );

        // After reset, page 0 must be clean.
        kani::assert(!bitmap.is_dirty(0), "page 0 must be clean after reset");
        kani::cover!(true, "reset clears and returns old state path covered");
    }
}
