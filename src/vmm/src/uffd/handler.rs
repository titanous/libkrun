// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! UffdHandler: registers guest memory with userfaultfd and resolves page faults.

use super::page_tracker::{
    guest_addr_to_page_index, guest_to_host, host_to_guest, is_eexist, LoadSource, PageTracker,
    PageTrackerStats, UffdRegion,
};
use crate::snapshot_store::{system_page_size, SnapshotStore};
use crate::vm_exit::SharedVmExit;
use futures::StreamExt;
use std::sync::Arc;
use std::thread;
use tokio::sync::oneshot;
use userfaultfd::Uffd;

/// Handler for UFFD-driven demand paging.
///
/// This struct encapsulates the userfaultfd lifecycle:
/// - Creates the UFFD fd
/// - Registers guest memory regions
/// - Runs on a dedicated thread with its own tokio runtime
/// - Waits for main thread ready signal via oneshot channel
pub struct UffdHandler {
    /// Shared UFFD file descriptor
    uffd: Arc<Uffd>,
    /// Snapshot store for reading pages
    store: Arc<dyn SnapshotStore>,
    /// Shared VM exit state
    vm_exit: SharedVmExit,
    /// Memory region mappings for guest_addr <-> host_addr translation
    regions: Vec<UffdRegion>,
    /// Receiver to signal that main thread is ready for faults
    ready_rx: Option<oneshot::Receiver<()>>,
    /// Page tracker for monitoring restore progress (preload vs fault)
    tracker: Arc<PageTracker>,
}

/// Preload task that consumes the store's preload stream and copies pages via UFFD.
///
/// This task runs concurrently with the fault loop. It yields control to the fault handler
/// if a race occurs (EEXIST), and stops gracefully on stream errors (non-fatal preload).
async fn preload_task(
    store: Arc<dyn SnapshotStore>,
    uffd: Arc<Uffd>,
    regions: Vec<UffdRegion>,
    tracker: Arc<PageTracker>,
) {
    let region_params: Vec<(u64, u64)> = regions.iter().map(|r| (r.guest_addr, r.size)).collect();

    let mut stream = store.preload(region_params);

    while let Some(result) = stream.next().await {
        match result {
            Ok((guest_addr, data)) => {
                // Translate guest address to host address for UFFD copy
                let host_addr = match guest_to_host(&regions, guest_addr) {
                    Some(addr) => addr,
                    None => {
                        log::warn!(
                            "preload chunk at 0x{guest_addr:x} not in registered regions, skipping"
                        );
                        continue;
                    }
                };

                let result = unsafe {
                    uffd.copy(
                        data.as_ptr() as *const _,
                        host_addr as *mut _,
                        data.len(), // multi-page len (e.g., 4MB for FsSnapshotStore)
                        true,       // wake
                    )
                };

                match result {
                    Ok(_) => {
                        // Successfully copied chunk. Mark all pages in the chunk as loaded via preload.
                        // Chunk is typically multi-page (e.g., 4MB chunks from FsSnapshotStore).
                        let chunk_pages = data.len().div_ceil(system_page_size() as usize);
                        if let Some(start_page_index) =
                            guest_addr_to_page_index(&regions, guest_addr)
                        {
                            for i in 0..chunk_pages {
                                tracker.mark_loaded(start_page_index + i, LoadSource::Preload);
                            }
                        }
                    }
                    Err(e) if is_eexist(&e) => {
                        // Race with fault handler — a page in this chunk was
                        // already mapped. The kernel processes pages sequentially
                        // within the UFFDIO_COPY range: pages before the existing
                        // one WERE successfully copied; pages at and after the
                        // existing one were NOT copied. The fault handler will
                        // serve any missed pages on demand, so this is safe to
                        // ignore and continue with the next preload chunk.
                        //
                        // Stats will undercount preloaded pages on EEXIST.
                        // This is acceptable since PageTracker is monitoring-only.
                    }
                    Err(e) => {
                        // Non-fatal preload error — log and stop preloading.
                        // Remaining pages will be demand-paged via fault handler.
                        log::warn!(
                            "preload uffd.copy failed at 0x{guest_addr:x}: {e:?}, stopping preload"
                        );
                        break;
                    }
                }
            }
            Err(e) => {
                // Stream error — non-fatal. Stop preloading.
                log::warn!("preload stream error: {e}, stopping preload");
                break;
            }
        }
    }
    log::debug!("preload task finished");
}

impl UffdHandler {
    /// Create a new UFFD handler.
    ///
    /// # Arguments
    /// * `store` - Snapshot store for reading pages
    /// * `vm_exit` - Shared VM exit state for signaling errors
    /// * `regions` - Memory regions to register (guest_addr, host_addr, size)
    /// * `ready_rx` - Receiver to signal handler when main thread is ready
    ///
    /// # Returns
    /// `Ok(handler)` if UFFD creation and registration succeeds.
    /// Returns error if UFFD creation fails or registration fails.
    pub fn new(
        store: Arc<dyn SnapshotStore>,
        vm_exit: SharedVmExit,
        regions: Vec<(u64, u64, u64)>,
        ready_rx: oneshot::Receiver<()>,
    ) -> std::io::Result<Self> {
        // Create UFFD fd with non-blocking mode for AsyncFd integration
        let uffd = userfaultfd::UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(true)
            .user_mode_only(true)
            .create()
            .map_err(|e| std::io::Error::other(format!("Failed to create UFFD: {e}")))?;

        let uffd = Arc::new(uffd);

        // Register all memory regions and compute total pages
        let mut uffd_regions = Vec::new();
        let mut total_pages: usize = 0;
        for (guest_addr, host_addr, size) in regions {
            uffd.register(host_addr as *mut _, size as usize)
                .map_err(|e| {
                    std::io::Error::other(format!(
                        "Failed to register UFFD region at 0x{guest_addr:x}: {e}"
                    ))
                })?;

            let num_pages = size.div_ceil(system_page_size());
            let page_offset = total_pages;
            total_pages = total_pages
                .checked_add(num_pages as usize)
                .ok_or_else(|| std::io::Error::other("total_pages overflow"))?;

            uffd_regions.push(UffdRegion {
                guest_addr,
                host_addr,
                size,
                page_offset,
            });
        }

        // Create page tracker for monitoring restore progress
        let tracker = Arc::new(PageTracker::new(total_pages));

        Ok(UffdHandler {
            uffd,
            store,
            vm_exit,
            regions: uffd_regions,
            ready_rx: Some(ready_rx),
            tracker,
        })
    }

    /// Get current restore progress statistics.
    ///
    /// Returns a snapshot of pages loaded, fault counts, and progress percentage.
    pub fn tracker_stats(&self) -> PageTrackerStats {
        self.tracker.stats()
    }

    /// Spawn the handler on a dedicated thread using the provided tokio runtime.
    ///
    /// The runtime is moved to the handler thread. The caller must ensure
    /// `enable_all()` was used when building the runtime (UFFD handler needs
    /// the I/O driver for `AsyncFd` and the timer driver).
    ///
    /// Returns a `JoinHandle` to await the handler's completion.
    pub fn run(self, rt: tokio::runtime::Runtime) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("uffd-handler".into())
            .spawn(move || {
                rt.block_on(self.run_handler());
            })
            .expect("failed to spawn uffd handler thread")
    }

    /// Main async handler entry point that coordinates preload and fault loop.
    ///
    /// Runs preload and fault loop concurrently. When fault loop exits (Uffd fd closed),
    /// both tasks stop.
    async fn run_handler(self) {
        // Clone once for preload task before we consume self in fault_loop
        let store_for_preload = self.store.clone();
        let uffd_for_preload = self.uffd.clone();
        let regions_for_preload = self.regions.clone();
        let tracker_for_preload = self.tracker.clone();

        // Create preload and fault loop futures
        let preload_future = preload_task(
            store_for_preload,
            uffd_for_preload,
            regions_for_preload,
            tracker_for_preload,
        );
        let fault_future = self.fault_loop();

        // Run both concurrently until the fault loop exits
        // Since the preload stream isn't Send, we can't spawn separate tasks
        // Instead, we use join! to run them concurrently in this task
        futures::future::join(preload_future, fault_future).await;
    }

    /// Main async fault loop.
    ///
    /// Waits for main thread ready signal, then handles UFFD page faults.
    async fn fault_loop(mut self) {
        // Wait for main thread to signal ready (device/vCPU states restored)
        if let Some(ready_rx) = self.ready_rx.take() {
            if ready_rx.await.is_err() {
                // Main thread dropped the channel (error occurred)
                return;
            }
        }

        // Start the fault loop
        let uffd_fd = UffdFd(self.uffd.clone());
        let async_uffd = match tokio::io::unix::AsyncFd::new(uffd_fd) {
            Ok(fd) => fd,
            Err(_) => return, // Failed to create AsyncFd
        };

        loop {
            // Wait for UFFD to be readable
            let mut guard = match async_uffd.readable().await {
                Ok(guard) => guard,
                Err(_) => break, // UFFD fd closed (shutdown)
            };

            // Read event (non-blocking)
            match async_uffd.get_ref().0.read_event() {
                Ok(Some(userfaultfd::Event::Pagefault { addr, .. })) => {
                    // Record that a fault event was received
                    self.tracker.record_fault();

                    let guest_addr = host_to_guest(&self.regions, addr as u64);
                    let store_clone = self.store.clone();
                    let uffd_clone = self.uffd.clone();
                    let host_addr = addr as u64;
                    let vm_exit = self.vm_exit.clone();
                    let tracker_clone = self.tracker.clone();
                    let regions_clone = self.regions.clone();

                    tokio::spawn(async move {
                        match store_clone.read_page(guest_addr).await {
                            Ok(Some(data)) => {
                                let result = unsafe {
                                    uffd_clone.copy(
                                        data.as_ptr() as *const _,
                                        host_addr as *mut _,
                                        data.len(),
                                        true, // wake
                                    )
                                };
                                match result {
                                    Ok(_) => {
                                        // Successfully copied page data. Mark page as loaded via fault.
                                        if let Some(page_index) =
                                            guest_addr_to_page_index(&regions_clone, guest_addr)
                                        {
                                            tracker_clone
                                                .mark_loaded(page_index, LoadSource::Fault);
                                        }
                                    }
                                    Err(e) => {
                                        // Check for EEXIST (page already mapped, race condition)
                                        if !is_eexist(&e) {
                                            signal_error(
                                                &vm_exit,
                                                format!("uffd copy failed: {e:?}"),
                                            );
                                        }
                                        // Silently ignore EEXIST — race with preload or another fault
                                    }
                                }
                            }
                            Ok(None) => {
                                // Reclaimed page — resolve via zeropage ioctl.
                                // Maps the kernel shared zero page — no data copy, no physical allocation.
                                let result =
                                    unsafe { uffd_clone.zeropage(host_addr as *mut _, 4096, true) };
                                match result {
                                    Ok(_) => {
                                        if let Some(page_index) =
                                            guest_addr_to_page_index(&regions_clone, guest_addr)
                                        {
                                            tracker_clone.mark_loaded(page_index, LoadSource::Zero);
                                        }
                                    }
                                    Err(e) => {
                                        if !is_eexist(&e) {
                                            signal_error(
                                                &vm_exit,
                                                format!("uffd zeropage failed: {e:?}"),
                                            );
                                        }
                                        // Silently ignore EEXIST — race with preload or another fault (AC3.4)
                                    }
                                }
                            }
                            Err(e) => {
                                // Fatal: read_page failed, signal VmExit::Error
                                signal_error(
                                    &vm_exit,
                                    format!("demand page read failed at 0x{guest_addr:x}: {e}"),
                                );
                            }
                        }
                    });
                }
                Ok(Some(_)) => {
                    // Other events (Fork, Remap, Remove, Unmap) — ignore
                }
                Ok(None) => {
                    // No event ready (WouldBlock) — clear readiness and retry
                    guard.clear_ready();
                }
                Err(_) => {
                    // UFFD fd error (likely closed) — exit loop
                    break;
                }
            }
        }
    }
}

/// Wrapper to make `Arc<Uffd>` implement `AsRawFd` for use with `tokio::io::unix::AsyncFd`.
struct UffdFd(Arc<Uffd>);

impl std::os::unix::io::AsRawFd for UffdFd {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.0.as_raw_fd()
    }
}

/// Signal a fatal error to the VM exit state.
fn signal_error(vm_exit: &SharedVmExit, message: String) {
    if let Ok(mut exit) = vm_exit.lock() {
        if exit.is_none() {
            *exit = Some(crate::vm_exit::VmExit::Error { message });
        }
    }
}
