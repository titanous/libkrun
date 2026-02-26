# Userfaultd Implementation Plan — Phase 4: Preload Integration

**Goal:** Add a preload task that runs concurrently with the fault loop, using multi-page UFFDIO_COPY for efficient bulk memory loading.

**Architecture:** The UFFD handler thread spawns two concurrent tasks: the fault loop (from Phase 3) and a preload task. The preload task consumes the `store.preload()` stream, calling `uffd.copy()` with multi-page `len` for each chunk. EEXIST races between preload and fault tasks are handled gracefully. Preload errors are non-fatal — logging + stop preload, remaining pages served by faults.

**Tech Stack:** Rust, userfaultfd, tokio (spawn, select!), futures (StreamExt)

**Scope:** 6 phases from original design (phase 4 of 6)

**Codebase verified:** 2026-02-25

---

## Acceptance Criteria Coverage

This phase implements and tests:

### userfaultd.AC3: UFFD handler (preload integration)
- **userfaultd.AC3.4 Success:** Preload task runs concurrently: consumes `store.preload()` stream, UFFDIO_COPY per chunk, EEXIST ignored
- **userfaultd.AC3.5 Success:** UFFDIO_COPY uses multi-page `len` for preload chunks (not per-page calls)
- **userfaultd.AC3.6 Success:** Fatal `read_page` error signals VMM stop via `VmExit::Error`; preload stream errors are non-fatal (logged, preload stops, faults handle remaining pages)

---

## Reference Files

- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/src/uffd.rs` — UFFD handler from Phase 3 (fault loop, UffdHandler struct)
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/src/snapshot_store.rs` — SnapshotStore trait (`preload` returns `BoxStream<'_, io::Result<(u64, Vec<u8>)>>`)

---

<!-- START_TASK_1 -->
### Task 1: Add preload task alongside fault loop

**Verifies:** userfaultd.AC3.4, userfaultd.AC3.5, userfaultd.AC3.6

**Files:**
- Modify: `src/vmm/src/uffd.rs` (add preload task, modify run to spawn both concurrently)

**Implementation:**

Modify the `UffdHandler::fault_loop` method (or rename to `run_async`) to spawn both the fault loop and preload task concurrently using `tokio::select!` or by spawning both as separate tasks.

The preload task:
```rust
async fn preload_task(
    store: Arc<dyn SnapshotStore>,
    uffd: Arc<Uffd>,
    regions: Vec<UffdRegion>,
) {
    use futures::StreamExt;

    let region_params: Vec<(u64, u64)> = regions.iter()
        .map(|r| (r.guest_addr, r.size))
        .collect();

    let mut stream = store.preload(region_params);

    while let Some(result) = stream.next().await {
        match result {
            Ok((guest_addr, data)) => {
                let host_addr = guest_to_host(&regions, guest_addr);
                let result = unsafe {
                    uffd.copy(
                        data.as_ptr() as *const _,
                        host_addr as *mut _,
                        data.len(),  // multi-page len (e.g., 4MB for FsSnapshotStore)
                        true, // wake
                    )
                };
                match result {
                    Ok(_) => {},
                    Err(e) if is_eexist(&e) => {
                        // Race with fault handler — a page in this chunk was
                        // already mapped. The kernel processes pages sequentially
                        // within the UFFDIO_COPY range: pages before the existing
                        // one WERE successfully copied; pages at and after the
                        // existing one were NOT copied. The fault handler will
                        // serve any missed pages on demand, so this is safe to
                        // ignore and continue with the next preload chunk.
                    }
                    Err(e) => {
                        // Non-fatal preload error — log and stop preloading.
                        // Remaining pages will be demand-paged via fault handler.
                        log::warn!("preload uffd.copy failed at 0x{guest_addr:x}: {e:?}, stopping preload");
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
```

**Multi-page UFFDIO_COPY (AC3.5):** The kernel accepts `len` spanning multiple contiguous pages in a single `UFFDIO_COPY` call. FsSnapshotStore's `preload` yields 4MB chunks (1024 x 4KB pages), so each `uffd.copy()` call covers 4MB, reducing syscall count from ~262K to ~256 for 1GB RAM.

**Concurrent execution:** The handler thread's async entry point spawns both tasks:
```rust
async fn run_handler(self) {
    let store = Arc::new(self.store);
    let uffd = Arc::new(self.uffd);

    // Spawn preload as a background task
    let preload_handle = tokio::spawn(preload_task(
        store.clone(),
        uffd.clone(),
        self.regions.clone(),
    ));

    // Run fault loop in the foreground
    self.fault_loop(store, uffd).await;

    // Fault loop exited (Uffd fd closed) — cancel preload
    preload_handle.abort();
}
```

The fault loop runs until the Uffd fd is closed (shutdown). The preload task runs concurrently and may finish before or after the fault loop. On shutdown, aborting the preload handle cancels any in-progress stream consumption.

**Non-fatal preload errors (AC3.6):** Preload stream errors are logged and cause preload to stop. The fault handler continues serving pages on demand. This is the correct behavior: if the backend's preload stream fails (e.g., network error during bulk transfer), individual `read_page` calls may still succeed (e.g., from cache). Fatal `read_page` errors in the fault handler still signal `VmExit::Error`.

**EEXIST race between preload and faults:** Both preload and fault handlers may try to copy the same page:
- vCPU faults on page X → fault handler calls `read_page(X)` → starts copying
- Preload stream yields chunk containing page X → preload calls `uffd.copy` with multi-page len
- Whichever arrives first succeeds. The second gets EEXIST, which is silently handled.
- No data corruption: both operations copy the same page data (from the same store).

**Testing:**

Tests must verify:
- userfaultd.AC3.4: Create a mock store with a preload stream that yields known data. Verify pages are populated via preload (not fault handler). Check that preload stream is consumed.
- userfaultd.AC3.5: Verify that the `uffd.copy` call uses the full chunk length (not per-page calls). This can be verified by having the mock store yield a multi-page chunk and checking that only one `uffd.copy` call is made per chunk (or by checking `data.len()` in the copy call).
- userfaultd.AC3.6: Test with a mock store whose preload stream returns an error after a few chunks. Verify preload stops but fault handling continues for remaining pages.

**Verification:**
Run: `cargo test -p vmm --features uffd`
Expected: Tests pass on Linux

**Commit:** `feat(vmm): add concurrent preload task with multi-page UFFDIO_COPY`
<!-- END_TASK_1 -->
