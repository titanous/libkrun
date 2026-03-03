// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

mod handler;
mod page_tracker;

// Re-export the same public surface that uffd.rs exported:
pub use handler::UffdHandler;
pub use page_tracker::{
    guest_addr_to_page_index, guest_to_host, host_to_guest, LoadSource, PageTracker,
    PageTrackerStats, UffdRegion,
};
