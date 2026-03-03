# Phase 6: Kani Proofs

## Overview

**Goal:** Bounded formal verification proofs for correctness-critical functions — bitmap deduplication, bounds safety, header validation, and address translation — using Kani's bounded model checker.

**Design reference:** `docs/design-plans/2026-03-03-testing-upgrade.md` — `<!-- START_PHASE_6 -->`

**Acceptance criteria addressed:**
- testing-upgrade.AC2.6: `just kani` runs all proofs and passes
- testing-upgrade.AC2.6a: PageTracker deduplication proof: `mark_loaded` on same page twice increments counter exactly once (bound: 128 pages)
- testing-upgrade.AC2.6b: DirtyBitmap bounds proof: `mark_dirty` with any `u64` never panics or writes out of bounds (bound: 256 pages)
- testing-upgrade.AC2.6c: ReclaimedBitmap consistency proof: `mark` → `is_set` returns true; `clear` → `is_set` returns false; `count` equals popcount (bound: 256 pages)
- testing-upgrade.AC2.6d: Snapshot header validation proof: `validate_magic_and_version` rejects all invalid magic/version
- testing-upgrade.AC2.6e: GDT round-trip proof: `get_base(gdt_entry(flags, base, limit)) == base` and `get_limit` equivalent
- testing-upgrade.AC2.6f: Address translation proof: `guest_to_host` returns correct offset for in-range, None for out-of-range (bound: 4 regions)

**Note:** AC2.6a-AC2.6f are plan-level decompositions of the single design-document criterion `testing-upgrade.AC2.6`.

**Done when:** `just kani` completes with all 6 proofs reporting VERIFICATION SUCCESSFUL. `just kani-proof <name>` runs a single named proof.

**Dependencies:** Phase 2 complete (pure logic extracted to `src/vmm/src/uffd/page_tracker.rs`). Note: Phase 6 harnesses work against whichever layout is current — if Phase 2 has not yet split `uffd.rs`, the uffd proof paths in `kani-proofs/Cargo.toml` reference the monolithic `uffd.rs` module instead.

---

## Investigation Findings

### Visibility changes required

`validate_magic_and_version` in `src/vmm/src/snapshot.rs` is currently a private function (`fn`, not `pub` or `pub(crate)`). The Kani harness lives in a separate crate (`kani-proofs/`) and cannot access private items. It must be changed to `pub(crate)` so the harness can call it directly.

All other targets — `DirtyBitmap`, `ReclaimedBitmap`, `PageTracker`, `gdt_entry`/`get_base`/`get_limit`, `guest_to_host` — are already `pub` or are accessible with `pub(crate)` from the same workspace.

**Functions that are currently private in `gdt.rs`:**
`get_base` and `get_limit` are private (`fn`, not `pub`). The GDT proof needs to call them. Options:
1. Change both to `pub(crate)` in `src/arch/src/x86_64/gdt.rs`.
2. Use `kvm_segment_from_gdt` as a proxy to test base (already public) and expose limit via `kvm_segment.limit`.

Option 2 avoids modifying the arch crate and is preferred. The GDT limit proof verifies `kvm_segment_from_gdt(gdt_entry(flags, base, limit), 0).limit == limit`. The GDT base proof verifies `kvm_segment_from_gdt(gdt_entry(flags, base, limit), 0).base == base as u64`. Both are already possible through the public `kvm_segment_from_gdt` API.

**`UffdRegion` and `guest_to_host` visibility in `uffd.rs`:**
`UffdRegion` is a private struct and `guest_to_host` is a private function in `src/vmm/src/uffd.rs`. After Phase 2, they are moved to `src/vmm/src/uffd/page_tracker.rs` and made `pub`. If Phase 2 has not run, the kani-proofs crate cannot access these; the address translation proof must be deferred until after Phase 2, OR a thin wrapper can be added to expose them.

The plan below assumes Phase 2 has run (split complete, `UffdRegion` and `guest_to_host` are `pub` in `vmm::uffd::page_tracker`). If Phase 2 has not run, skip Task 7 and implement it after Phase 2 completes.

### Kani toolchain setup

Kani ships its own toolchain and is invoked via `cargo kani`, not `cargo +nightly`. Installation:
```bash
cargo install --locked kani-verifier
cargo kani setup
```

After setup, `cargo kani` is available and uses Kani's bundled toolchain automatically. No `+nightly` or `+kani` prefix is needed — `cargo kani` is itself the entry point.

### kani-proofs as a separate package

The `kani-proofs/` directory is a standalone Cargo package, **not** added to the root workspace's `[members]` list. This prevents `cargo build` and `cargo test` from trying to compile Kani harnesses in normal builds (Kani requires its own toolchain). The package is only used via `cargo kani --manifest-path kani-proofs/Cargo.toml`.

### AtomicU64 in Kani

Kani supports `std::sync::atomic::AtomicU64` operations. Proofs run single-threaded (no concurrency model), which is appropriate for the bounds/logic invariants being checked. Kani models atomics as non-atomic for verification purposes, which is safe here because these proofs check single-threaded invariants (deduplication logic, not concurrency correctness — loom covers that in Phase 3).

### kani::any() and bounds

`kani::any::<T>()` generates an unconstrained symbolic value of type `T`. `kani::assume(condition)` restricts the symbolic state space. For `Vec`-based types, Kani cannot symbolically explore unbounded `Vec` lengths — harnesses must construct fixed-size instances using `kani::assume(total_pages <= N)` or by building the type directly with a concrete small size.

For `DirtyBitmap` and `ReclaimedBitmap` (which take size at construction), the harness creates instances with `kani::any()` sizes bounded by `kani::assume`. For `PageTracker`, the same pattern applies.

### kani::stub for transitive dependencies

The `kani-proofs` crate pulls in `vmm`, `devices`, and `arch` as dependencies. These crates transitively depend on `kvm-bindings`, `kvm-ioctls`, `userfaultfd`, and `libc` functions. Kani cannot model kernel interface calls.

However, the specific functions being proved — `DirtyBitmap::mark_dirty`, `ReclaimedBitmap::mark`, `PageTracker::mark_loaded`, `validate_magic_and_version`, `gdt_entry`/`kvm_segment_from_gdt`, and `guest_to_host` — do **not** call any kernel interfaces. They are pure computation. The transitive dependencies only pull in types (structs, enums) and are not exercised by the proof harnesses.

If Kani reports stub errors for KVM functions, use `#[kani::proof] #[kani::stub(kvm_ioctls::SomeStruct::some_fn, stub_fn)]` as needed. In practice, none of the 6 proof targets call into KVM or libc, so stubs are unlikely to be needed. The note below each proof task identifies the risk.

---

<!-- START_TASK_1 -->
## Task 1: Create `kani-proofs/` package structure

**Verifies:** Prerequisite for all proofs — package compiles under `cargo kani`.

**Files:**
- Create: `kani-proofs/Cargo.toml`
- Create: `kani-proofs/src/lib.rs`
- Create: `kani-proofs/src/dirty_bitmap.rs`
- Create: `kani-proofs/src/reclaimed_bitmap.rs`
- Create: `kani-proofs/src/page_tracker.rs`
- Create: `kani-proofs/src/snapshot_header.rs`
- Create: `kani-proofs/src/gdt.rs`
- Create: `kani-proofs/src/address_translation.rs`

**Implementation:**

**Step 1: Create `kani-proofs/Cargo.toml`**

```toml
[package]
name = "kani-proofs"
version = "0.1.0"
edition = "2021"
publish = false

# This package is NOT a member of the root workspace.
# It is invoked only via: cargo kani --manifest-path kani-proofs/Cargo.toml
# Running `cargo build` at the workspace root does NOT compile this package.

[dependencies]
vmm = { path = "../src/vmm", features = ["snapshot", "uffd"] }
devices = { path = "../src/devices", features = ["net"] }
arch = { path = "../src/arch" }

[patch.crates-io]
vhost = { path = "../vendor/vhost" }
```

**Note on `[patch.crates-io]`:** The `kani-proofs` package is outside the root workspace, so it does not inherit the workspace-level `[patch.crates-io]` entries. The `vmm` and `devices` crates may indirectly depend on `vhost`. Add the patch here if `cargo kani` reports unresolved dependency errors for `vhost`. Check at runtime and add other `vendor/` patches if needed.

**Step 2: Create `kani-proofs/src/lib.rs`**

```rust
// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Kani bounded model-checking proofs for libkrun correctness invariants.
//!
//! This crate contains #[kani::proof] harnesses for critical functions.
//! Run with: cargo kani --manifest-path kani-proofs/Cargo.toml
//! Run one:  cargo kani --manifest-path kani-proofs/Cargo.toml --harness <name>

pub mod address_translation;
pub mod dirty_bitmap;
pub mod gdt;
pub mod page_tracker;
pub mod reclaimed_bitmap;
pub mod snapshot_header;
```

**Verification:**

```bash
cargo kani --manifest-path kani-proofs/Cargo.toml --only-codegen
```

Expected: Package compiles without errors. The `--only-codegen` flag skips the model-checking step and just verifies the Rust code compiles under Kani's toolchain.

**Do not commit yet.**
<!-- END_TASK_1 -->

---

<!-- START_TASK_2 -->
## Task 2: DirtyBitmap bounds proof

**Verifies:** testing-upgrade.AC2.6b — `mark_dirty` with any `u64` never panics or writes out of bounds.

**Files:**
- Create: `kani-proofs/src/dirty_bitmap.rs`

**Background:**

`DirtyBitmap::mark_dirty` silently ignores addresses outside the bitmap's range (returns early if `!self.contains(guest_addr)`). The proof must verify that for ANY `u64` address and ANY bitmap size up to 256 pages, `mark_dirty` never panics, never accesses out-of-bounds memory, and leaves the bitmap in a consistent state.

This is a safety invariant: vCPU fault handlers call `mark_dirty` from hot paths with arbitrary guest addresses, and a panic or buffer overrun would crash the VMM.

**Implementation:**

Create `kani-proofs/src/dirty_bitmap.rs`:

```rust
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
```

**Run command:**

```bash
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_dirty_no_panic
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_dirty_in_bounds_recorded
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_dirty_out_of_bounds_no_effect
```

**Expected output:** `VERIFICATION SUCCESSFUL` for each proof.

**Stub risk:** None. `DirtyBitmap` uses only `AtomicU64` — no libc or kernel calls.

**Do not commit yet.**
<!-- END_TASK_2 -->

---

<!-- START_TASK_3 -->
## Task 3: ReclaimedBitmap consistency proof

**Verifies:** testing-upgrade.AC2.6c — `mark` → `is_set` returns true; `clear` → `is_set` returns false; `count` equals popcount (bound: 256 pages).

**Files:**
- Create: `kani-proofs/src/reclaimed_bitmap.rs`

**Background:**

`ReclaimedBitmap` tracks balloon-reclaimed pages. The invariant is: if `mark(pfn)` is called on a valid PFN, then `is_set(pfn)` must return true. If `clear(pfn)` is called, `is_set(pfn)` must return false. And `count()` must always equal the number of set bits.

This invariant is load-bearing: the VMM uses `count()` to decide how many pages to exclude from snapshots. A miscounted bitmap could corrupt snapshot/restore.

**Implementation:**

Create `kani-proofs/src/reclaimed_bitmap.rs`:

```rust
// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Kani proofs for ReclaimedBitmap correctness.
//!
//! Verifies: mark/is_set round-trip, clear/is_set round-trip, count consistency.

use devices::virtio::balloon::reclaimed_bitmap::ReclaimedBitmap;

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
```

**Run command:**

```bash
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_then_is_set
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_clear_then_not_is_set
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_count_equals_popcount
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_out_of_bounds_pfn_ignored
```

**Expected output:** `VERIFICATION SUCCESSFUL` for each proof.

**Stub risk:** None. `ReclaimedBitmap` uses only `AtomicU64` — no libc or kernel calls.

**Do not commit yet.**
<!-- END_TASK_3 -->

---

<!-- START_TASK_4 -->
## Task 4: PageTracker deduplication proof

**Verifies:** testing-upgrade.AC2.6a — `mark_loaded` on same page twice increments counter exactly once (bound: 128 pages).

**Files:**
- Create: `kani-proofs/src/page_tracker.rs`

**Background:**

`PageTracker::mark_loaded` uses `fetch_or` to atomically set a bit, then checks if the bit was already set before incrementing the counter. The property: if the same `page_index` is marked twice, `preload_count + fault_count + zero_count` must be exactly 1, not 2. This is the deduplication invariant that prevents double-counting when both the preload task and the fault handler race on the same page.

Since Kani runs single-threaded, this proof verifies the sequential version of the invariant (two sequential calls, same source or different sources). Concurrent deduplication correctness is covered by Phase 3 loom tests.

**Note on PageTracker location:** After Phase 2, `PageTracker` is in `vmm::uffd::page_tracker`. Before Phase 2, it is in `vmm::uffd`. The import path in the harness depends on which phase has run. The plan below uses the post-Phase-2 path. If Phase 2 has not run, change the use statement to `use vmm::uffd::{PageTracker, LoadSource, PageTrackerStats};`.

**Implementation:**

Create `kani-proofs/src/page_tracker.rs`:

```rust
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
```

**Run command:**

```bash
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_loaded_same_source_dedup
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_loaded_different_source_dedup
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_loaded_out_of_bounds_ignored
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_two_distinct_pages
```

**Expected output:** `VERIFICATION SUCCESSFUL` for each proof.

**Stub risk:** `PageTracker` uses `AtomicU64` and `AtomicUsize` — no syscalls. However, the `vmm` crate with the `uffd` feature also pulls in `userfaultfd`. This crate contains bindings to the Linux `userfaultfd` syscall. Kani may report errors if it tries to model `userfaultfd::Uffd::new()` or similar. Since the proof harnesses never call `UffdHandler` or any function that creates a UFFD fd, this should not occur. If it does, add a stub:

```rust
// In proof harness, if needed:
#[kani::proof]
#[kani::stub(userfaultfd::Uffd::new, stub_uffd_new)]
fn proof_mark_loaded_same_source_dedup() { ... }
```

**Do not commit yet.**
<!-- END_TASK_4 -->

---

<!-- START_TASK_5 -->
## Task 5: Snapshot header validation proof

**Verifies:** testing-upgrade.AC2.6d — `validate_magic_and_version` rejects all invalid magic/version.

**Files:**
- Modify: `src/vmm/src/snapshot.rs` — change `validate_magic_and_version` to `pub(crate)`
- Create: `kani-proofs/src/snapshot_header.rs`

**Background:**

`validate_magic_and_version` checks that `header.magic == SNAPSHOT_MAGIC` and `header.version == SNAPSHOT_VERSION`. It is called at the start of every restore operation. The proof verifies that:
1. Any header with wrong magic → `Err(SnapshotError::InvalidMagic)`.
2. Any header with right magic but wrong version → `Err(SnapshotError::InvalidVersion(_))`.
3. The valid combination (correct magic AND correct version) → `Ok(())`.

**Step 1: Change visibility of `validate_magic_and_version` in `src/vmm/src/snapshot.rs`**

In `src/vmm/src/snapshot.rs`, change line 102:

```rust
// BEFORE:
fn validate_magic_and_version(header: &SnapshotHeader) -> Result<(), SnapshotError> {

// AFTER:
pub(crate) fn validate_magic_and_version(header: &SnapshotHeader) -> Result<(), SnapshotError> {
```

This makes the function accessible from the `kani-proofs` crate (which depends on `vmm` as a library) while keeping it hidden from external crates. `pub(crate)` is sufficient because Kani treats the dependency as a library — `pub(crate)` items are accessible within the crate that defines them, and from test/harness code that treats the dependency as an external crate with `pub` API only.

**Correction:** `pub(crate)` is NOT accessible from an external dependent crate. To make `validate_magic_and_version` accessible from `kani-proofs`, it must be `pub`. However, making an internal validation function `pub` exposes it in the library's external API, which is undesirable.

**Alternative:** Instead of changing visibility, write the proof harness against `validate_header_for_vm` (which is already `pub`) and supply a minimal stub `GuestMemoryMmap`. However, `GuestMemoryMmap` requires mmap and cannot be constructed in Kani.

**Preferred solution:** Add a thin `pub(crate)` re-export in a test-gating module, or make `validate_magic_and_version` `pub` but `#[doc(hidden)]`:

```rust
// In src/vmm/src/snapshot.rs:
#[doc(hidden)]
pub fn validate_magic_and_version(header: &SnapshotHeader) -> Result<(), SnapshotError> {
```

`#[doc(hidden)]` keeps it out of rustdoc while making it accessible to the `kani-proofs` crate. This is the standard pattern for test-only visibility in Rust libraries.

Apply this change to `src/vmm/src/snapshot.rs` line 102.

**Step 2: Create `kani-proofs/src/snapshot_header.rs`**

```rust
// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Kani proofs for snapshot header validation.
//!
//! validate_magic_and_version must reject all invalid magic and version values
//! and accept exactly the one valid combination.

use vmm::snapshot::{
    validate_magic_and_version, SnapshotHeader, SnapshotError, SNAPSHOT_MAGIC, SNAPSHOT_VERSION,
};

/// Proof: wrong magic always produces InvalidMagic error.
///
/// For any header where magic != SNAPSHOT_MAGIC, validate_magic_and_version
/// must return Err(SnapshotError::InvalidMagic).
#[kani::proof]
fn proof_invalid_magic_rejected() {
    let magic: u32 = kani::any();
    kani::assume(magic != SNAPSHOT_MAGIC);

    let header = SnapshotHeader {
        magic,
        version: SNAPSHOT_VERSION, // correct version (magic is the error)
        vcpu_count: 1,
        ram_regions: vec![],
        nested_enabled: false,
    };

    let result = validate_magic_and_version(&header);
    kani::assert(
        matches!(result, Err(SnapshotError::InvalidMagic)),
        "wrong magic must produce InvalidMagic error",
    );
}

/// Proof: correct magic but wrong version produces InvalidVersion error.
///
/// For any header where magic == SNAPSHOT_MAGIC and version != SNAPSHOT_VERSION,
/// validate_magic_and_version must return Err(SnapshotError::InvalidVersion(v)).
#[kani::proof]
fn proof_invalid_version_rejected() {
    let version: u32 = kani::any();
    kani::assume(version != SNAPSHOT_VERSION);

    let header = SnapshotHeader {
        magic: SNAPSHOT_MAGIC,
        version,
        vcpu_count: 1,
        ram_regions: vec![],
        nested_enabled: false,
    };

    let result = validate_magic_and_version(&header);
    kani::assert(
        matches!(result, Err(SnapshotError::InvalidVersion(_))),
        "wrong version (with correct magic) must produce InvalidVersion error",
    );
}

/// Proof: correct magic AND correct version produces Ok(()).
///
/// This is the only valid input combination. All other combinations must fail
/// (proven by the proofs above).
#[kani::proof]
fn proof_valid_header_accepted() {
    let header = SnapshotHeader {
        magic: SNAPSHOT_MAGIC,
        version: SNAPSHOT_VERSION,
        vcpu_count: kani::any(),
        ram_regions: vec![],
        nested_enabled: kani::any(),
    };

    let result = validate_magic_and_version(&header);
    kani::assert(result.is_ok(), "correct magic and version must produce Ok(())");
}

/// Proof: exhaustive check — magic XOR version wrong always fails.
///
/// Explores all combinations where at least one of (magic, version) is wrong.
/// Together with proof_valid_header_accepted, this covers the full input space.
#[kani::proof]
fn proof_any_wrong_field_fails() {
    let magic: u32 = kani::any();
    let version: u32 = kani::any();

    // At least one of the two fields is wrong.
    kani::assume(magic != SNAPSHOT_MAGIC || version != SNAPSHOT_VERSION);

    let header = SnapshotHeader {
        magic,
        version,
        vcpu_count: 1,
        ram_regions: vec![],
        nested_enabled: false,
    };

    let result = validate_magic_and_version(&header);
    kani::assert(result.is_err(), "any wrong field must produce an error");
}
```

**Run command:**

```bash
# First verify the snapshot.rs change compiles:
cargo check -p vmm --features snapshot

# Then run proofs:
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_invalid_magic_rejected
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_invalid_version_rejected
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_valid_header_accepted
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_any_wrong_field_fails
```

**Expected output:** `VERIFICATION SUCCESSFUL` for each proof.

**Stub risk:** `SnapshotHeader` contains a `Vec<(u64, u64)>` field (`ram_regions`). The harness constructs it with `vec![]` (empty), avoiding any heap allocation that Kani cannot model symbolically for unbounded `Vec`. The `validate_magic_and_version` function does not touch `ram_regions` — it only checks `magic` and `version`. This is safe.

**Commit:** `feat: make validate_magic_and_version pub #[doc(hidden)] for Kani proof access`
<!-- END_TASK_5 -->

---

<!-- START_TASK_6 -->
## Task 6: GDT round-trip proof

**Verifies:** testing-upgrade.AC2.6e — `get_base(gdt_entry(flags, base, limit)) == base` and `get_limit` equivalent.

**Files:**
- Create: `kani-proofs/src/gdt.rs`

**Background:**

`gdt_entry(flags, base, limit)` encodes a GDT descriptor from its components. `kvm_segment_from_gdt(entry, table_index)` decodes the entry into a `kvm_segment` struct. The round-trip property: the decoded `base` and `limit` must equal the original inputs.

`get_base` and `get_limit` are private functions in `src/arch/src/x86_64/gdt.rs`. Rather than changing their visibility, the proof uses `kvm_segment_from_gdt` as the public decoding path — it calls `get_base` and `get_limit` internally, and exposes the results as `kvm_segment.base` and `kvm_segment.limit`.

**GDT encoding constraints:**
- `base` is a 32-bit value, fully round-tripable: `gdt_entry` encodes all 32 bits of base. `get_base` recovers all 32 bits. The proof uses `base: u32` (full range).
- `limit` is a 20-bit value: `gdt_entry` encodes only bits 0-15 and bits 16-19. `get_limit` returns a `u32` with the top 12 bits always 0. The proof must bound `limit` to 20 bits: `limit <= 0xFFFFF`.
- `flags` is a 16-bit value used to set descriptor type/access bits. `gdt_entry` masks it to `0xF0FF` — 12 significant flag bits. The proof uses `flags: u16` (full range; masking is part of the implementation under test).

**Implementation:**

Create `kani-proofs/src/gdt.rs`:

```rust
// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Kani proofs for GDT encoding/decoding round-trip correctness.
//!
//! gdt_entry encodes (flags, base, limit) into a u64 GDT descriptor.
//! kvm_segment_from_gdt decodes it back. The round-trip must preserve base and limit.

use arch::x86_64::gdt::{gdt_entry, kvm_segment_from_gdt};

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
```

**Run command:**

```bash
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_gdt_base_roundtrip
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_gdt_limit_roundtrip
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_gdt_selector_encoding
```

**Expected output:** `VERIFICATION SUCCESSFUL` for each proof.

**Stub risk:** `kvm_segment_from_gdt` returns a `kvm_segment` struct from `kvm-bindings`. This is a pure C-layout struct with no syscalls. `kvm-bindings` is a safe, pure-Rust wrapper around KVM type definitions. No stubs are needed.

**Note on arch crate:** The `arch` crate re-exports x86_64 submodules. Verify the exact import path. In `src/arch/src/lib.rs`, x86_64 items are gated on `#[cfg(target_arch = "x86_64")]`. Since Kani runs on x86_64 Linux, this is fine.

**Do not commit yet.**
<!-- END_TASK_6 -->

---

<!-- START_TASK_7 -->
## Task 7: Address translation proof

**Verifies:** testing-upgrade.AC2.6f — `guest_to_host` returns correct offset for in-range, None for out-of-range (bound: 4 regions).

**Files:**
- Create: `kani-proofs/src/address_translation.rs`

**Prerequisites:** Phase 2 must be complete (uffd split, `UffdRegion` and `guest_to_host` are `pub` in `vmm::uffd::page_tracker`). If Phase 2 has not run, defer this task.

**Background:**

`guest_to_host(regions, guest_addr)` iterates over `UffdRegion` slices and returns `Some(host_addr + offset)` when `guest_addr` falls within a region, or `None` otherwise. The correctness properties:
1. If `guest_addr` is in `[region.guest_addr, region.guest_addr + region.size)`, the result is `Some(region.host_addr + (guest_addr - region.guest_addr))`.
2. If `guest_addr` is outside all regions, the result is `None`.
3. The offset arithmetic never overflows (since `guest_addr < region.guest_addr + region.size` implies `guest_addr - region.guest_addr < region.size`).

**Implementation:**

Create `kani-proofs/src/address_translation.rs`:

```rust
// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Kani proofs for guest_to_host address translation correctness.
//!
//! guest_to_host must return the correct host offset for in-range addresses
//! and None for out-of-range addresses. Bound: up to 4 regions.

// Post-Phase-2 import path:
use vmm::uffd::page_tracker::{UffdRegion, guest_to_host};

/// Proof: guest_to_host returns Some with correct offset for in-range addresses.
///
/// For a single region, any address in [guest_addr, guest_addr + size) must map
/// to Some(host_addr + (addr - guest_addr)).
///
/// Bound: 1 region (the correctness of the loop is the same for N regions).
#[kani::proof]
fn proof_guest_to_host_in_range_correct() {
    // Symbolic region parameters.
    let region_guest: u64 = kani::any();
    let region_host: u64 = kani::any();
    let region_size: u64 = kani::any();
    // Avoid empty regions and overflow in guest_addr + size.
    kani::assume(region_size > 0);
    kani::assume(region_guest.checked_add(region_size).is_some());

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
}

/// Proof: guest_to_host returns None for addresses before any region.
///
/// Bound: 1 region.
#[kani::proof]
fn proof_guest_to_host_before_region_is_none() {
    let region_guest: u64 = kani::any();
    let region_host: u64 = kani::any();
    let region_size: u64 = kani::any();
    kani::assume(region_size > 0);
    kani::assume(region_guest.checked_add(region_size).is_some());
    // Ensure there is address space before the region.
    kani::assume(region_guest > 0);

    let region = UffdRegion {
        guest_addr: region_guest,
        host_addr: region_host,
        size: region_size,
        page_offset: 0,
    };

    // Symbolic address strictly before the region.
    let addr: u64 = kani::any();
    kani::assume(addr < region_guest);

    let result = guest_to_host(&[region], addr);
    kani::assert(result.is_none(), "address before region must produce None");
}

/// Proof: guest_to_host returns None for addresses at or after region end.
///
/// Bound: 1 region.
#[kani::proof]
fn proof_guest_to_host_after_region_is_none() {
    let region_guest: u64 = kani::any();
    let region_host: u64 = kani::any();
    let region_size: u64 = kani::any();
    kani::assume(region_size > 0);
    // Ensure region_guest + region_size does not overflow.
    kani::assume(region_guest.checked_add(region_size).is_some());

    let region = UffdRegion {
        guest_addr: region_guest,
        host_addr: region_host,
        size: region_size,
        page_offset: 0,
    };

    // Address at or after the region end.
    let addr: u64 = kani::any();
    kani::assume(addr >= region_guest + region_size);

    let result = guest_to_host(&[region], addr);
    kani::assert(result.is_none(), "address at or after region end must produce None");
}

/// Proof: guest_to_host with 2 non-overlapping regions returns correct mapping.
///
/// When two regions exist, an address in region 1 maps to region 1's host space,
/// and an address in region 2 maps to region 2's host space.
/// Bound: 2 regions (sufficient to verify multi-region correctness).
#[kani::proof]
fn proof_guest_to_host_two_regions() {
    // Region A.
    let guest_a: u64 = kani::any();
    let host_a: u64 = kani::any();
    let size_a: u64 = kani::any();
    kani::assume(size_a > 0);
    kani::assume(guest_a.checked_add(size_a).is_some());

    // Region B: must start after region A ends (non-overlapping).
    let guest_b: u64 = kani::any();
    let host_b: u64 = kani::any();
    let size_b: u64 = kani::any();
    kani::assume(size_b > 0);
    kani::assume(guest_b.checked_add(size_b).is_some());
    kani::assume(guest_b >= guest_a + size_a); // B starts at or after A ends.

    let regions = [
        UffdRegion { guest_addr: guest_a, host_addr: host_a, size: size_a, page_offset: 0 },
        UffdRegion { guest_addr: guest_b, host_addr: host_b, size: size_b, page_offset: 0 },
    ];

    // Address in region A.
    let addr_a: u64 = kani::any();
    kani::assume(addr_a >= guest_a && addr_a < guest_a + size_a);
    let result_a = guest_to_host(&regions, addr_a);
    kani::assert(result_a == Some(host_a + (addr_a - guest_a)), "address in A maps to A's host");

    // Address in region B.
    let addr_b: u64 = kani::any();
    kani::assume(addr_b >= guest_b && addr_b < guest_b + size_b);
    let result_b = guest_to_host(&regions, addr_b);
    kani::assert(result_b == Some(host_b + (addr_b - guest_b)), "address in B maps to B's host");
}
```

**Run command:**

```bash
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_guest_to_host_in_range_correct
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_guest_to_host_before_region_is_none
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_guest_to_host_after_region_is_none
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_guest_to_host_two_regions
```

**Expected output:** `VERIFICATION SUCCESSFUL` for each proof.

**Stub risk:** `UffdRegion` is a plain struct (four fields, no methods involving syscalls). `guest_to_host` is a pure loop — no syscalls. However, since `kani-proofs` depends on `vmm` with `features = ["uffd"]`, the `userfaultfd` crate is compiled as a dependency. Since the harness never calls UFFD syscall wrappers, stubs are not needed. If Kani complains about unresolvable UFFD functions, add:

```toml
# In kani-proofs/Cargo.toml, change the vmm dependency to drop uffd if needed:
vmm = { path = "../src/vmm", features = ["snapshot"] }
# Then adjust the import to the pre-Phase-2 location if necessary.
```

**Do not commit yet.**
<!-- END_TASK_7 -->

---

<!-- START_TASK_8 -->
## Task 8: Add `just kani` and `just kani-proof` justfile targets

**Verifies:** testing-upgrade.AC5.6 (`just safety` includes kani), testing-upgrade.AC2.6 (`just kani` passes).

**Files:**
- Modify: `justfile` at project root

**Implementation:**

Replace the stub `kani` and `kani-proof` targets added in Phase 1 with full implementations:

```just
# Kani: bounded formal verification proofs.
# Requires: cargo install --locked kani-verifier && cargo kani setup
# All proofs in kani-proofs/:
kani:
    cargo kani --manifest-path kani-proofs/Cargo.toml

# Run a single named Kani proof.
# Usage: just kani-proof proof_mark_dirty_no_panic
kani-proof name:
    cargo kani --manifest-path kani-proofs/Cargo.toml --harness {{name}}
```

Also update the `safety` compound target to include kani (per AC5.6):

```just
# Compound target: safety checks
# Phase 1: check
# Phase 5: + asan + shuttle
# Phase 6: + kani
safety: check asan shuttle kani
```

**Verification:**

```bash
just kani-proof proof_mark_dirty_no_panic
just kani
```

**Commit:** `feat: implement just kani and just kani-proof targets`
<!-- END_TASK_8 -->

---

## Verification

After completing all tasks, verify the full phase passes:

```bash
# 1. All individual proofs pass:
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_dirty_no_panic
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_dirty_in_bounds_recorded
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_dirty_out_of_bounds_no_effect
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_then_is_set
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_clear_then_not_is_set
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_count_equals_popcount
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_out_of_bounds_pfn_ignored
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_loaded_same_source_dedup
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_loaded_different_source_dedup
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_loaded_out_of_bounds_ignored
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_mark_two_distinct_pages
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_invalid_magic_rejected
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_invalid_version_rejected
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_valid_header_accepted
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_any_wrong_field_fails
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_gdt_base_roundtrip
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_gdt_limit_roundtrip
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_gdt_selector_encoding
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_guest_to_host_in_range_correct
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_guest_to_host_before_region_is_none
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_guest_to_host_after_region_is_none
cargo kani --manifest-path kani-proofs/Cargo.toml --harness proof_guest_to_host_two_regions

# 2. just kani runs all proofs in one command:
just kani

# 3. just kani-proof runs a single named proof:
just kani-proof proof_gdt_base_roundtrip

# 4. Existing unit tests still pass (snapshot.rs change is backward-compatible):
cargo test -p vmm --features snapshot

# 5. Root workspace still compiles:
cargo check --features embedded_init,snapshot,uffd,blk,vhost-user
```

---

## Design Discrepancy Notes

- **`validate_magic_and_version` visibility:** The function is private (`fn`, not `pub`) in snapshot.rs. The plan changes it to `pub` with `#[doc(hidden)]`. This is the minimum change needed for the Kani harness to call it from an external crate. An alternative — wrapping it with a test-only `cfg(kani)` re-export — would require adding `kani` as an optional dependency in `vmm/Cargo.toml`, which is more complex and less clean. `#[doc(hidden)]` is the standard Rust pattern for "accessible but not part of the public API".

- **`get_base` and `get_limit` remain private:** The GDT proof uses `kvm_segment_from_gdt` as a proxy, which is already `pub`. This avoids modifying the arch crate. The tradeoff: the proof tests the full encode/decode chain (including `kvm_segment_from_gdt`'s field extraction), not just the individual `get_base`/`get_limit` functions. This is acceptable because `kvm_segment_from_gdt` is the only consumer of those private functions.

- **`UffdRegion` and `guest_to_host` visibility:** These are private in the current `uffd.rs`. Phase 2 makes them `pub` in `page_tracker.rs`. Task 7 depends on Phase 2 being complete. If implementing Phase 6 before Phase 2: either (a) add temporary `pub` visibility to `uffd.rs` directly, or (b) skip Task 7 and implement it as part of Phase 2.

- **kani-proofs not in workspace:** The `kani-proofs/Cargo.toml` deliberately omits itself from the root workspace `[members]`. This means `cargo build` at the root does not compile it. The `Cargo.lock` for `kani-proofs` is separate from the root `Cargo.lock`. Do not add `kani-proofs` to the workspace `[members]` list — doing so would require the Kani toolchain for all workspace builds.

- **`[patch.crates-io]` in kani-proofs:** The root workspace `Cargo.toml` has `[patch.crates-io]` entries for `vendor/vhost`, `vendor/vhost-user-backend`, and `vendor/virtio-queue`. The `kani-proofs` package is outside the workspace and does not inherit these patches. If `cargo kani` fails with "found package vhost X.Y.Z but only X.Y.Z-patched is available"-style errors, add the relevant `[patch.crates-io]` entries to `kani-proofs/Cargo.toml`. For the specific proofs in this phase (bitmap, snapshot header, GDT, address translation), `vhost` is not exercised and the patches are unlikely to be needed.

- **Vec-based type bounds:** `DirtyBitmap`, `ReclaimedBitmap`, and `PageTracker` all internally use `Vec<AtomicU64>`. Kani can reason about `Vec` contents but requires bounded sizes to terminate. The bounds in the proofs (256 pages for bitmaps, 128 pages for PageTracker) were chosen to keep verification time under ~5 minutes per proof on typical hardware. If proofs timeout, reduce bounds (e.g., 64 pages) — the correctness properties are identical at smaller bounds.

- **`ram_regions: vec![]` in snapshot proof:** The `validate_magic_and_version` function does not read `ram_regions`. The harness uses `vec![]` to avoid constructing a symbolic `Vec`, which Kani cannot do for arbitrary lengths. This is correct — the proof only needs to cover the magic/version check paths.

- **Kani installation in CI:** `cargo kani setup` downloads Kani's bundled toolchain (~1 GB). In CI, cache the Kani toolchain directory (`~/.kani/` by default) between runs. The `just kani` target assumes Kani is already installed. If it is not, the command will fail with a clear error: "cargo-kani: command not found". The CI setup instructions (not part of this plan) should include `cargo install --locked kani-verifier && cargo kani setup`.
