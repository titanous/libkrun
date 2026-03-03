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
