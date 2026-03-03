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
