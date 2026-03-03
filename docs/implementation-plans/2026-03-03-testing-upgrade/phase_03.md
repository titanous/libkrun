# Phase 3: Miri + proptest + Loom

## Overview

**Goal:** Apply pure-logic testing tools to the modules extracted in Phase 2 and to existing pure files.

**Design reference:** `docs/design-plans/2026-03-03-testing-upgrade.md` — `<!-- START_PHASE_3 -->`

**Acceptance criteria addressed:**
- testing-upgrade.AC2.1: `just miri` runs and passes on all pure-logic modules
- testing-upgrade.AC2.3: `just loom` runs exhaustive concurrency tests on `DirtyBitmap`, `ReclaimedBitmap`, `PageTracker`
- testing-upgrade.AC2.5: `just proptest` runs property tests for bitmap invariants, GDT, address translation, snapshot round-trips, Builder validation

**Done when:** `just miri` passes, `just proptest` passes, `just loom` passes.

**Dependencies:** Phase 2 complete (modules extracted; loom shims in dirty_bitmap.rs, reclaimed_bitmap.rs, page_tracker.rs; request.rs created).

---

## Investigation Findings

### Miri-safe (no file I/O, no mmap):

| File | Line count | Existing tests | Notes |
|------|-----------|----------------|-------|
| `src/arch/src/x86_64/gdt.rs` | 116 | `test_field_parse` | All pure bitfield ops |
| `src/vmm/src/dirty_bitmap.rs` | 231 | 7 tests (lines 114–229) | All pure AtomicU64 |
| `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` | 308 | 11 tests (lines 119–306) | All pure AtomicU64 |
| `src/vmm/src/snapshot.rs` | 910 | 8 Miri-safe, 10 file-I/O | Mark I/O tests `#[cfg_attr(miri, ignore)]` |
| `src/vmm/src/uffd/page_tracker.rs` | — | (new from Phase 2) | Pure atomics + address math |
| `src/devices/src/virtio/block/request.rs` | — | (new from Phase 2) | Pure C-layout structs |

### Snapshot types (for proptest):
- `SnapshotHeader` fields: `magic: u32, version: u32, vcpu_count: u32, ram_regions: Vec<(u64, u64)>, nested_enabled: bool`
- `VmSnapshot` fields: `header: SnapshotHeader, vcpu_states: Vec<Vec<u8>>, device_states: Vec<(String, Vec<u8>)>, gic_state: Option<Vec<u8>>, vm_state: Option<Vec<u8>>, excluded_pages: Vec<u64>`
- `IncrementalSnapshot` fields: `header, vcpu_states, device_states, dirty_pages: Vec<DirtyPage>, gic_state, vm_state, reclaimed_pages: Vec<u64>`
- `DirtyPage` fields: `guest_addr: u64, data: Vec<u8>`
- All types derive `Serialize`, `Deserialize`, `Clone`, `Debug` behind `feature = "snapshot"`

### Builder validation (pure — no KVM open):
- `Builder::new()` — constructs state, no KVM
- `Builder::vm_config(0, 256)` → `Err(StartError::ZeroVcpus)` — pure validation
- `Builder::add_virtiofs_vhost_user(too_long_tag, ...)` → `Err(StartError::TagTooLong(n))` — pure validation
- Both gated on relevant feature flags (`vhost-user` for TagTooLong)

### Loom patterns:
- `DirtyBitmap::drain_dirty_pages` uses `Ordering::AcqRel` in the underlying `reset()` (swap), all other ops use Relaxed — the AcqRel/Relaxed boundary is the primary race condition of interest
- `ReclaimedBitmap`: all ops Relaxed — tests concurrent mark + clear + count consistency
- `PageTracker::mark_loaded` uses `fetch_or` + counter increment — the gap between the two ops is the deduplication race

---

## Task 1: Add `proptest` dev-dependency to crates

### Step 1.1 — `src/vmm/Cargo.toml`

```toml
[dev-dependencies]
# existing:
devices = { path = "../devices", features = ["test_utils"] }
# add:
proptest = "1.4"
proptest-derive = "0.4"
```

### Step 1.2 — `src/devices/Cargo.toml`

Add to `[dev-dependencies]` (create section if absent):
```toml
[dev-dependencies]
proptest = "1.4"
proptest-derive = "0.4"
```

### Step 1.3 — `src/libkrun/Cargo.toml`

Add to `[dev-dependencies]` (create section if absent):
```toml
[dev-dependencies]
proptest = "1.4"
```

---

## Task 2: Mark snapshot.rs I/O tests as Miri-incompatible

**Purpose:** `cargo miri test -p vmm` currently fails on tests that call `std::fs` functions. Mark them so the suite passes cleanly.

In `src/vmm/src/snapshot.rs`, add `#[cfg_attr(miri, ignore)]` to each test that creates temp files:

```rust
// Lines to update — add attribute above each #[test]:

#[cfg_attr(miri, ignore)]  // uses tempfile / std::fs
#[test]
fn test_memory_file_size_mismatch() { ... }  // line ~520

#[cfg_attr(miri, ignore)]
#[test]
fn test_truncated_vmstate_file() { ... }  // line ~552

#[cfg_attr(miri, ignore)]
#[test]
fn test_vmstate_roundtrip() { ... }  // line ~595

#[cfg_attr(miri, ignore)]
#[test]
fn test_incremental_snapshot_roundtrip() { ... }  // line ~647

#[cfg_attr(miri, ignore)]
#[test]
fn test_load_vmstate_exceeds_size_limit() { ... }  // line ~722

#[cfg_attr(miri, ignore)]
#[test]
fn test_load_incremental_snapshot_exceeds_size_limit() { ... }  // line ~754

#[cfg_attr(miri, ignore)]
#[test]
fn test_incremental_snapshot_with_reclaimed_pages() { ... }  // line ~786

#[cfg_attr(miri, ignore)]
#[test]
fn test_apply_reclaimed_pages_zero_fills() { ... }  // line ~842

#[cfg_attr(miri, ignore)]
#[test]
fn test_apply_reclaimed_pages_empty_list() { ... }  // line ~889
```

### Verify

```bash
cargo +nightly miri test -p vmm --features snapshot -- snapshot
```

All snapshot tests should pass (Miri-safe ones run, I/O ones are skipped).

---

## Task 3: proptest for `DirtyBitmap`

Add a new `proptest_tests` module to `src/vmm/src/dirty_bitmap.rs` **inside** the existing `#[cfg(test)]` block:

```rust
// In dirty_bitmap.rs, inside the #[cfg(test)] mod tests { ... } block, add:

#[cfg(not(loom))]  // proptest is not compatible with loom's AtomicU64
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
```

---

## Task 4: proptest for `ReclaimedBitmap`

Add to `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` **inside** the `#[cfg(test)]` block:

```rust
#[cfg(not(loom))]
mod proptest_tests {
    use super::*;
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
```

---

## Task 5: proptest for `gdt.rs`

Add to `src/arch/src/x86_64/gdt.rs` inside the `#[cfg(test)]` block:

```rust
#[cfg(not(loom))]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// get_base(gdt_entry(flags, base, limit)) == base for all valid inputs.
        ///
        /// GDT base is a 32-bit field embedded across bytes 2, 4, 5 of the 8-byte GDT entry.
        /// base is valid for values 0..=0xFF_FFFF (24-bit embedded portion).
        /// Full 32-bit base is encoded: bits 31-24 in byte 7, bits 23-16 in byte 4, bits 15-0 in bytes 2-3.
        #[test]
        fn prop_gdt_base_roundtrip(
            flags in 0u16..0xFFFF,
            base in 0u32..=u32::MAX,
            limit in 0u32..=0xFFFFF,  // 20-bit limit field
        ) {
            let entry = gdt_entry(flags, base, limit);
            let recovered_base = get_base(entry);
            prop_assert_eq!(recovered_base, base as u64);
        }

        /// kvm_segment_from_gdt preserves base.
        #[test]
        fn prop_kvm_segment_base_preserved(
            flags in 0u16..0xFFFF,
            base in 0u32..=u32::MAX,
            limit in 0u32..=0xFFFFF,
            table_index in 0u8..8,
        ) {
            let entry = gdt_entry(flags, base, limit);
            let seg = kvm_segment_from_gdt(entry, table_index);
            prop_assert_eq!(seg.base, base as u64);
        }
    }
}
```

---

## Task 6: proptest for snapshot round-trips

Add a new proptest module to `src/vmm/src/snapshot.rs` inside the `#[cfg(test)]` block (requires `feature = "snapshot"`):

```rust
#[cfg(all(not(loom), feature = "snapshot"))]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_snapshot_header() -> impl Strategy<Value = SnapshotHeader> {
        (
            any::<u32>(),       // magic (arbitrary for round-trip)
            any::<u32>(),       // version (arbitrary for round-trip)
            1u32..=16u32,       // vcpu_count (at least 1)
            prop::collection::vec(
                (any::<u64>(), 1u64..=(1u64 << 30)),  // (addr, size) pairs
                0..4
            ),
            any::<bool>(),      // nested_enabled
        ).prop_map(|(magic, version, vcpu_count, ram_regions, nested_enabled)| {
            SnapshotHeader {
                magic,
                version,
                vcpu_count,
                ram_regions,
                nested_enabled,
            }
        })
    }

    fn arb_dirty_page() -> impl Strategy<Value = DirtyPage> {
        (any::<u64>(), prop::collection::vec(any::<u8>(), 0..64))
            .prop_map(|(guest_addr, data)| DirtyPage { guest_addr, data })
    }

    fn arb_vm_snapshot() -> impl Strategy<Value = VmSnapshot> {
        (
            arb_snapshot_header(),
            prop::collection::vec(prop::collection::vec(any::<u8>(), 0..128), 0..4),
            prop::collection::vec(
                ("[a-z]{1,8}".prop_map(|s: String| s), prop::collection::vec(any::<u8>(), 0..64))
                    .prop_map(|(id, state)| (id, state)),
                0..4
            ),
            proptest::option::of(prop::collection::vec(any::<u8>(), 0..32)),
            proptest::option::of(prop::collection::vec(any::<u8>(), 0..32)),
            prop::collection::vec(any::<u64>(), 0..8),
        ).prop_map(|(header, vcpu_states, device_states, gic_state, vm_state, excluded_pages)| {
            VmSnapshot {
                header,
                vcpu_states,
                device_states,
                gic_state,
                vm_state,
                excluded_pages,
            }
        })
    }

    proptest! {
        /// VmSnapshot serializes and deserializes with identity (bincode round-trip).
        #[test]
        fn prop_vm_snapshot_bincode_roundtrip(snapshot in arb_vm_snapshot()) {
            let serialized = bincode::serialize(&snapshot)
                .expect("serialization failed");
            let deserialized: VmSnapshot = bincode::deserialize(&serialized)
                .expect("deserialization failed");

            prop_assert_eq!(snapshot.header.vcpu_count, deserialized.header.vcpu_count);
            prop_assert_eq!(snapshot.header.ram_regions, deserialized.header.ram_regions);
            prop_assert_eq!(snapshot.vcpu_states, deserialized.vcpu_states);
            prop_assert_eq!(snapshot.excluded_pages, deserialized.excluded_pages);
        }

        /// SnapshotHeader round-trip preserves all fields.
        #[test]
        fn prop_snapshot_header_roundtrip(header in arb_snapshot_header()) {
            let serialized = bincode::serialize(&header).expect("serialize");
            let recovered: SnapshotHeader = bincode::deserialize(&serialized).expect("deserialize");
            prop_assert_eq!(header.magic, recovered.magic);
            prop_assert_eq!(header.version, recovered.version);
            prop_assert_eq!(header.vcpu_count, recovered.vcpu_count);
            prop_assert_eq!(header.ram_regions, recovered.ram_regions);
            prop_assert_eq!(header.nested_enabled, recovered.nested_enabled);
        }

        /// validate_magic_and_version: wrong magic always fails.
        #[test]
        fn prop_invalid_magic_always_fails(
            version in any::<u32>(),
            vcpu_count in 1u32..16,
        ) {
            let header = SnapshotHeader {
                magic: SNAPSHOT_MAGIC + 1,  // wrong magic
                version,
                vcpu_count,
                ram_regions: vec![],
                nested_enabled: false,
            };
            let result = validate_magic_and_version(&header);
            prop_assert!(result.is_err());
        }
    }
}
```

**Note:** Verify `SNAPSHOT_MAGIC` constant name and `validate_magic_and_version` function signature in snapshot.rs before implementing. The codebase investigation confirmed these exist at lines 102 and 143.

---

## Task 7: proptest for address translation in `page_tracker.rs`

Add to `src/vmm/src/uffd/page_tracker.rs` inside the `#[cfg(test)]` block:

```rust
#[cfg(all(test, not(loom)))]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_region() -> impl Strategy<Value = UffdRegion> {
        (
            0u64..0x8000_0000,      // guest_addr (up to 2GB)
            1u64..0x1000_0000,      // size (up to 256MB, must be > 0)
            0u64..0x8000_0000,      // host_addr
        ).prop_map(|(guest_addr, size, host_addr)| UffdRegion {
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
```

---

## Task 8: proptest for Builder validation in `libkrun`

Add a test module to `src/libkrun/src/lib.rs` inside the `#[cfg(test)]` block (or add a new `#[cfg(test)]` block at the end of the file if none exists):

```rust
#[cfg(test)]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// vm_config with 0 vCPUs always returns ZeroVcpus error.
        #[test]
        fn prop_zero_vcpus_always_fails(ram_mib in 1u32..65536) {
            let mut builder = Builder::new();
            let result = builder.vm_config(0, ram_mib);
            prop_assert!(matches!(result, Err(StartError::ZeroVcpus)));
        }

        /// vm_config with non-zero vCPUs does not return ZeroVcpus.
        #[test]
        fn prop_nonzero_vcpus_succeeds_validation(
            num_vcpus in 1u8..=16,
            ram_mib in 128u32..65536,
        ) {
            let mut builder = Builder::new();
            let result = builder.vm_config(num_vcpus, ram_mib);
            prop_assert!(!matches!(result, Err(StartError::ZeroVcpus)));
        }
    }

    /// add_virtiofs_vhost_user with tag > 36 bytes returns TagTooLong.
    /// Not proptest (exact boundary test), but added here for completeness.
    #[test]
    #[cfg(all(feature = "vhost-user", not(feature = "tee")))]
    fn tag_too_long_boundary() {
        let mut builder = Builder::new();
        let tag_36 = "a".repeat(36);
        let tag_37 = "a".repeat(37);
        assert!(builder.add_virtiofs_vhost_user(&tag_36, "/tmp/sock", None).is_ok());
        let mut builder2 = Builder::new();
        let result = builder2.add_virtiofs_vhost_user(&tag_37, "/tmp/sock", None);
        assert!(matches!(result, Err(StartError::TagTooLong(37))));
    }
}
```

---

## Task 9: loom tests for `DirtyBitmap`

Add to `src/vmm/src/dirty_bitmap.rs` inside `#[cfg(test)]`:

```rust
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
                b1.mark_dirty(0x0);  // page 0
            });

            let b2 = Arc::clone(&bitmap);
            let drainer = thread::spawn(move || {
                b2.drain_dirty_pages()
            });

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
            let m1 = thread::spawn(move || { b1.mark_dirty(0x0); });

            let b2 = Arc::clone(&bitmap);
            let m2 = thread::spawn(move || { b2.mark_dirty(PAGE_SIZE); });

            m1.join().unwrap();
            m2.join().unwrap();

            let drained = bitmap.drain_dirty_pages();
            assert_eq!(drained.len(), 2, "expected 2 dirty pages, got {:?}", drained);
        });
    }
}
```

---

## Task 10: loom tests for `ReclaimedBitmap`

Add to `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` inside `#[cfg(test)]`:

```rust
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
            let m1 = thread::spawn(move || { b1.mark(0); });

            let b2 = Arc::clone(&bitmap);
            let m2 = thread::spawn(move || { b2.mark(1); });

            m1.join().unwrap();
            m2.join().unwrap();

            assert!(bitmap.is_set(0), "pfn 0 not set");
            assert!(bitmap.is_set(1), "pfn 1 not set");
            assert_eq!(bitmap.count(), 2);
        });
    }
}
```

---

## Task 11: loom tests for `PageTracker`

Add to `src/vmm/src/uffd/page_tracker.rs` inside `#[cfg(test)]`:

```rust
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
                stats.preload_pages + stats.fault_pages, 1,
                "preload={} fault={} — should sum to 1",
                stats.preload_pages, stats.fault_pages
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
```

---

## Task 12: Update `just miri`, `just loom`, `just proptest` justfile targets

Replace the stub targets from Phase 2 with full implementations:

```just
# Full feature set used by all targets
features := "embedded_init,snapshot,uffd,blk,vhost-user"

# Miri: run pure-logic unit tests under Miri (requires nightly toolchain)
# Tests marked #[cfg_attr(miri, ignore)] are skipped automatically.
miri:
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p arch -- gdt
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p vmm --features snapshot -- dirty_bitmap snapshot::tests::test_header
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p vmm --features uffd,snapshot -- uffd::page_tracker
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p devices --features net -- balloon::reclaimed_bitmap
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p devices --features blk -- virtio::block::request

# proptest: property-based tests for bitmap invariants, GDT, address translation, round-trips
proptest:
    cargo test -p vmm --features snapshot -- proptest_tests
    cargo test -p vmm --features uffd,snapshot -- uffd::page_tracker::proptest_tests
    cargo test -p devices --features net -- balloon::reclaimed_bitmap::proptest_tests
    cargo test -p arch -- gdt::proptest_tests

# proptest-long: extended runs (10x cases)
proptest-long:
    PROPTEST_CASES=10000 cargo test -p vmm --features snapshot -- proptest_tests
    PROPTEST_CASES=10000 cargo test -p vmm --features uffd,snapshot -- uffd::page_tracker::proptest_tests
    PROPTEST_CASES=10000 cargo test -p devices --features net -- balloon::reclaimed_bitmap::proptest_tests

# Loom: exhaustive concurrency testing on all bitmap/tracker types
# Requires --release for performance (loom is computationally intensive).
loom:
    RUSTFLAGS="--cfg loom" cargo test --release -p vmm -- dirty_bitmap::loom_tests
    RUSTFLAGS="--cfg loom" cargo test --release -p vmm --features uffd -- uffd::page_tracker::loom_tests
    RUSTFLAGS="--cfg loom" cargo test --release -p devices --features net -- balloon::reclaimed_bitmap::loom_tests

# all: run all quality checks
all: build test miri loom proptest
```

---

## Verification

After completing all tasks:

```bash
# 1. proptest runs
cargo test -p vmm --features snapshot -- proptest_tests
cargo test -p devices --features net -- balloon::reclaimed_bitmap::proptest_tests
cargo test -p arch -- gdt::proptest_tests

# 2. Miri runs (requires nightly)
MIRIFLAGS="-Zmiri-backtrace=full" cargo +nightly miri test -p vmm --features snapshot -- dirty_bitmap
MIRIFLAGS="-Zmiri-backtrace=full" cargo +nightly miri test -p arch -- gdt

# 3. Loom runs (requires --cfg loom + --release)
RUSTFLAGS="--cfg loom" cargo test --release -p vmm -- dirty_bitmap::loom_tests
RUSTFLAGS="--cfg loom" cargo test --release -p devices --features net -- balloon::reclaimed_bitmap::loom_tests

# 4. justfile targets
just miri
just proptest
just loom

# 5. Full test suite still passes (no regressions)
just test
```

---

## Design Discrepancy Notes

- **`validate_magic_and_version` visibility:** Confirmed as `fn` (private) at line 102 of snapshot.rs per the codebase investigation. The proptest in Task 6 needs to either make it `pub(crate)` or duplicate the validation logic. Preferred: make it `pub(crate)` to enable testing.

- **`SNAPSHOT_MAGIC` constant:** Verify the exact constant name in snapshot.rs before using it in proptest_tests. The investigation references it but doesn't confirm the variable name.

- **`DirtyBitmap::PAGE_SIZE` in loom tests:** The loom tests use `PAGE_SIZE` constant from dirty_bitmap.rs (currently `16384` for Apple Silicon). This is fine for x86_64 tests too since the tests just need a positive size.

- **`arch` crate name in justfile:** Verify the Cargo package name for `src/arch/` via `src/arch/Cargo.toml` — use the exact `[package] name` value in the `-p` flag.

- **loom with `just loom`:** The loom targets run with `--release` per best practice (loom is computationally intensive). Standard CI runs can use shorter exploration depth via `LOOM_MAX_PREEMPTIONS=1 RUSTFLAGS="--cfg loom" cargo test --release`.

- **proptest for libkrun Builder:** The `tag_too_long_boundary` test in Task 8 is feature-gated (`vhost-user`). The justfile proptest target should include it:
  ```just
  cargo test -p libkrun --features {{features}} -- proptest_tests
  ```
  Add this line to the `proptest:` target.
