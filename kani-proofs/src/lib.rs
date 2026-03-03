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
