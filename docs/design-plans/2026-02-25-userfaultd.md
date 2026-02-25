# Userfaultd Design

## Summary

Introduces a `SnapshotStore` trait that abstracts snapshot storage from filesystem paths, enabling arbitrary backends (CAS, S3, WAL). Adds a Linux-only in-process userfaultfd handler that demand-pages guest memory during cold restore — pages are loaded on fault or via a backend-controlled preload stream. A built-in `FsSnapshotStore` preserves backward compatibility. The existing path-based API becomes a thin wrapper around the new trait-based API.

## Definition of Done

Design and implement three interconnected components that enable demand-paged snapshot restore from arbitrary storage backends:

1. **Snapshot storage trait** — abstracts both read (restore) and write (create) paths, replacing the current `&Path`-based snapshot API. Callers implement this trait for their storage backend (filesystem, S3, CAS, WAL, etc.). A built-in filesystem implementation preserves backward compatibility.

2. **Page map / index** — a mechanism for efficient random-access page lookups across base + incremental snapshots, enabling range requests or equivalent. Allows resolving "which source has the latest version of page X" across a base snapshot and N incremental overlays. Owned by the backend — each `SnapshotStore` implementation manages its own index.

3. **Userfaultd** — an in-process, Linux-only demand-paging fault handler that uses the storage trait to page in guest memory on-demand during cold restore, with a factory+backend trait pattern (matching existing `AsyncBlockBackendFactory`/`AsyncNetBackendFactory`). Uses parallel fault resolution via tokio tasks and a concurrent preload stream.

**Success criteria:**
- Existing filesystem-based snapshot create/restore works through the new storage trait (no regression)
- Callers can implement the storage trait for S3/CAS/WAL backends
- Cold restore (`restore_and_run`) can demand-page memory via userfaultfd instead of loading it all upfront
- Incremental snapshots are resolvable through the backend's page map for demand-paging (latest version of each page is served)
- vmstate + device state are still loaded eagerly (only the memory blob is demand-paged)
- Integration tests verify actual demand-paging (faults fire, not just eager preload)

**Out of scope:**
- macOS / HVF support (Linux/KVM only for UFFD; storage trait is cross-platform)
- Out-of-process UFFD handler (Firecracker-style fd passing)
- Concrete S3/CAS/WAL implementations (caller provides behind trait)
- Hot restore demand-paging (only cold restore path initially)

## Acceptance Criteria

### AC1: SnapshotStore trait

- **AC1.1** `SnapshotStore` trait has six methods: `read_vmstate`, `read_page`, `preload`, `write_vmstate`, `write_pages`, `close`
- **AC1.2** All methods return `BoxFuture<'_, io::Result<T>>` (or `BoxStream` for `preload`), consistent with existing `AsyncBlockBackend` pattern
- **AC1.3** Trait is `Send + Sync + 'static` and object-safe (`Box<dyn SnapshotStore>` compiles)
- **AC1.4** `SnapshotStoreFactory` trait has one method (`create`), returns `BoxFuture<'static, io::Result<Box<dyn SnapshotStore>>>`, is `Send + 'static`
- **AC1.5** No snapshot IDs or lineage concepts in libkrun — caller manages identity externally

### AC2: FsSnapshotStore

- **AC2.1** `FsSnapshotStore` implements `SnapshotStore` for filesystem-backed snapshots
- **AC2.2** Write path produces files compatible with current snapshot format (`vmstate` + `memory` files)
- **AC2.3** Read path supports base + N incremental overlays; `read_page` resolves latest version
- **AC2.4** `preload` yields memory in sequential chunks (4MB default), enabling efficient UFFDIO_COPY
- **AC2.5** Existing `restore_and_run(path, incrementals)` delegates to `restore_and_run_with_store` using `FsSnapshotStoreFactory` — zero behavior change

### AC3: UFFD handler

- **AC3.1** `UffdHandler` creates UFFD fd, registers guest memory regions, runs on dedicated thread with tokio runtime
- **AC3.2** Fault loop uses `AsyncFd<Uffd>` (non-blocking) and `tokio::spawn` per fault for parallel resolution
- **AC3.3** Each fault task calls `store.read_page(guest_addr)` then `uffd.copy()`; EEXIST return is silently ignored
- **AC3.4** Preload task runs concurrently: consumes `store.preload()` stream, UFFDIO_COPY per chunk, EEXIST ignored
- **AC3.5** UFFDIO_COPY uses multi-page `len` for preload chunks (not per-page calls)
- **AC3.6** Fatal `read_page` error signals VMM stop via `VmExit::Error`; preload stream errors are non-fatal (logged, preload stops, faults handle remaining pages)

### AC4: Backward compatibility

- **AC4.1** `Context::restore_and_run(path, incrementals)` still works, delegates internally
- **AC4.2** `VmHandle::snapshot(path)` and `incremental_snapshot(path)` still work, delegate internally
- **AC4.3** Existing snapshot integration tests pass without modification
- **AC4.4** No changes to snapshot file format

### AC5: Integration tests with demand-paging

- **AC5.1** Test with empty-preload store: all pages loaded via UFFD faults, guest runs correctly
- **AC5.2** Test with FsSnapshotStore: preload loads everything, near-zero faults
- **AC5.3** Test with partial-preload store: both preload and fault paths exercise
- **AC5.4** Test incremental chain: base + 2 incrementals via demand-paging, latest dirty pages win
- **AC5.5** Test error handling: store fails `read_page` for specific address, VM gets clean `VmExit::Error`
- **AC5.6** Parallel fault test: store with artificial 50ms delay per `read_page`, multiple vCPUs. Verify total restore time is significantly less than sequential (num_faults * 50ms), confirming faults are resolved concurrently

## Architecture

### Component Overview

```
+--------------------------------------------------+
|  libkrun Rust API                                |
|  Context::restore_and_run_with_store(factory)     |
|  VmHandle::snapshot_to_store(store)               |
+----------+----------------------+-----------------+
           | eager                | on fault
           v                     v
+------------------+  +----------------------------+
|  vmstate restore |  |  UffdHandler<S>            |
|  (vCPU, devices) |  |  - tokio runtime           |
|  runs on main    |  |  - AsyncFd<Uffd>           |
|  thread          |  |  - spawns task per fault   |
+------------------+  |  - Arc<dyn SnapshotStore>  |
                      |  - PageTracker bitmap      |
                      +------------+---------------+
                                   | read_page / preload
                      +------------v---------------+
                      |  SnapshotStore trait        |
                      |  (caller implements)        |
                      +----------------------------+
                      |  FsSnapshotStore            |
                      |  (libkrun provides)         |
                      +----------------------------+
```

### SnapshotStore Trait

```rust
pub trait SnapshotStore: Send + Sync + 'static {
    // Read
    fn read_vmstate(&self) -> BoxFuture<'_, io::Result<Vec<u8>>>;
    fn read_page(&self, guest_addr: u64) -> BoxFuture<'_, io::Result<Vec<u8>>>;
    fn preload(&self, regions: Vec<(u64, u64)>) -> BoxStream<'_, io::Result<(u64, Vec<u8>)>>;

    // Write
    fn write_vmstate(&self, data: Vec<u8>) -> BoxFuture<'_, io::Result<()>>;
    fn write_pages(&self, pages: Vec<(u64, Vec<u8>)>) -> BoxFuture<'_, io::Result<()>>;
    fn close(&self) -> BoxFuture<'_, io::Result<()>>;
}

pub trait SnapshotStoreFactory: Send + 'static {
    fn create(self: Box<Self>) -> BoxFuture<'static, io::Result<Box<dyn SnapshotStore>>>;
}
```

**Design decisions:**

- **`BoxFuture` not native async fn**: Required for object safety (`Box<dyn SnapshotStore>`), `Send` enforcement on returned futures, and consistency with existing `AsyncBlockBackend` pattern. Cost is one heap allocation per call, dwarfed by I/O latency.
- **`read_page` returns `Vec<u8>` not `&mut [u8]`**: Avoids lifetime issues with `tokio::spawn` (spawned futures must be `'static`). Natural for network backends where data arrives as owned `Bytes`.
- **`io::Result` error type**: Consistent with existing async trait pattern. CAS/S3 backends wrap errors via `io::Error::new(ErrorKind::Other, e)`.
- **`write_pages` is unified** (no separate `write_dirty_pages`): Full snapshot writes all pages, incremental writes dirty pages. The store doesn't need to distinguish — it just stores what it receives. Each `write_vmstate` + `write_pages` + `close` cycle is one layer.
- **No snapshot IDs in libkrun**: Caller manages identity and lineage externally. The factory/store is opaque to libkrun — configured by the caller with whatever metadata it needs (IDs, parent refs, trace context).
- **No `prefetch` method**: Removed as redundant. Backend-side prefetch is more natural — CAS backends cache adjacent pages when fetching a chunk, S3 backends can issue wider range requests. The preload stream covers bulk pre-fetching.

### Page Map / Index

Owned entirely by each `SnapshotStore` implementation. libkrun does not maintain a page map.

**FsSnapshotStore page map:** Computed from `ram_regions` in the snapshot header (for file offset calculation) plus dirty page lists from incrementals (for overlay resolution). `read_page` checks incrementals newest-first, falls back to base memory file. O(1) lookup via `HashMap<u64, (usize, usize)>` mapping guest_addr to (incremental_index, dirty_page_index).

**CAS backend page map (caller-provided):** The backend maintains its own manifest mapping page ranges to content hashes across layers. Resolution is the backend's concern.

**PageTracker (UFFD handler):** Atomic bitmap tracking which pages have been UFFDIO_COPY'd. One bit per page, packed into `AtomicU64` words. Used for stats and monitoring (pages loaded via preload vs fault, total faults, restore progress). Not required for correctness — the kernel prevents re-faulting once a page is mapped.

### UFFD Handler

**Thread lifecycle:**

1. Main thread creates guest memory (anonymous mmap), creates `Uffd`, registers memory regions
2. Spawns UFFD handler thread with factory
3. Handler thread creates tokio runtime, calls `factory.create()` to get store
4. Handler calls `store.read_vmstate()`, sends bytes to main thread via oneshot channel
5. Main thread deserializes vmstate, validates header, restores vCPU + device states
6. Main thread signals "ready" to handler thread
7. Handler spawns two concurrent tasks:
   - **Preload task**: consumes `store.preload(regions)` stream, UFFDIO_COPY per chunk
   - **Fault loop**: `async_read_event()` → `tokio::spawn` per fault → `store.read_page()` → `uffd.copy()`
8. Main thread resumes vCPUs

**Parallel fault resolution:**

```rust
// Fault loop (simplified)
let store = Arc::new(store);
let uffd = Arc::new(uffd);
loop {
    let event = read_event_async(&async_uffd).await?;
    let store = store.clone();
    let uffd = uffd.clone();
    tokio::spawn(async move {
        let data = store.read_page(guest_addr).await?;
        match uffd.copy(host_addr, &data, data.len(), true) {
            Ok(_) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
            Err(e) => Err(e),
        }
    });
}
```

The kernel serializes UFFD events per-page but different pages can fault concurrently. `tokio::spawn` overlaps network fetches for multiple pages. Single-threaded handlers (Firecracker, QEMU pattern) work for local files; parallel resolution matters for network-backed stores.

**UFFDIO_COPY multi-page support:** The kernel accepts `len` spanning multiple contiguous pages. The preload task uses this — FsSnapshotStore yields 4MB chunks, resulting in ~256 UFFDIO_COPY calls for 1GB RAM instead of 262K.

**Race safety:** Verified against Linux kernel source (`mm/userfaultfd.c`). UFFDIO_COPY holds the Page Table Lock (PTL), checks if PTE is empty, returns EEXIST if page already present. No corruption, no partial pages. The kernel docs state: "UFFDIO_COPY is atomic — nothing can see a half-populated page."

**Shutdown:** Main thread drops/closes the Uffd fd. `async_read_event()` returns error. Fault loop exits, runtime drop cancels outstanding tasks. Store is dropped (backend cleans up connections). Thread joins.

**Error handling:** `read_page` failure sends error via `watch` channel. UFFD thread drops Uffd fd, unblocking all pending faults. VMM stores `VmExit::Error`. Preload stream errors are non-fatal — logged, preload stops, remaining pages are demand-paged.

### Unified Restore Flow

Always register UFFD. Preload and faults run concurrently. Backend controls the balance:

| Backend | preload yields | faults expected |
|---------|---------------|-----------------|
| Filesystem | All memory in 4MB sequential chunks | ~zero (preload wins) |
| CAS (warm cache) | Cached chunks immediately | Uncached pages only |
| CAS (cold) | Empty stream | Every page |
| CAS (hybrid) | Local tier fast, then S3 streaming | Few, during fetch gaps |

This eliminates separate eager vs demand-paged code paths. Filesystem converges to the same performance as current eager restore (sequential read + ~256 UFFDIO_COPY calls for 1GB, negligible overhead).

### Backward Compatibility

Existing path-based API becomes thin wrappers:

```rust
impl Context {
    pub fn restore_and_run(&self, base: &Path, incrementals: &[&Path])
        -> Result<VmExit, StartError>
    {
        let factory = FsSnapshotStoreFactory::new(base, incrementals);
        self.restore_and_run_with_store(Box::new(factory))
    }
}

impl VmHandle {
    pub fn snapshot_to_store(&self, store: Box<dyn SnapshotStore>) -> Result<(), SnapshotError>;
    pub fn incremental_snapshot_to_store(&self, store: Box<dyn SnapshotStore>) -> Result<(), SnapshotError>;
}
```

No changes to snapshot file format. No changes to existing C API behavior.

## Existing Patterns Followed

- **Factory + backend trait**: Same pattern as `AsyncBlockBackendFactory`/`AsyncBlockBackend` and `AsyncNetBackendFactory`/`AsyncNetBackend`. Factory is `Send + 'static`, consumed on worker thread, creates backend inside tokio runtime.
- **`BoxFuture` for async trait methods**: Same as `AsyncBlockBackend::read_vectored_at()` returning `BoxFuture<'_, io::Result<usize>>`.
- **`io::Result` error type**: Same as all existing async backend traits.
- **Feature gating**: New `uffd` feature follows the same pattern as `net`, `blk`, `snapshot` features.
- **Dedicated worker thread with tokio runtime**: Same as `async_worker.rs` for block and net devices.

## Implementation Phases

### Phase 1: SnapshotStore trait + FsSnapshotStore write path

Define `SnapshotStore` and `SnapshotStoreFactory` traits in `src/vmm/src/snapshot_store.rs`. Implement `FsSnapshotStore` write side: `write_vmstate`, `write_pages`, `close`. Wire `VmHandle::snapshot_to_store` and `incremental_snapshot_to_store`. Make existing path-based methods delegate through.

**Files:** `src/vmm/src/snapshot_store.rs` (new), `src/vmm/src/lib.rs`, `src/libkrun/src/lib.rs`
**Tests:** Existing snapshot tests pass. New unit tests for FsSnapshotStore write roundtrip.

### Phase 2: FsSnapshotStore read path (eager, no UFFD)

Implement `read_vmstate`, `read_page`, `preload` on `FsSnapshotStore`. Wire `Context::restore_and_run_with_store` as eager restore — drains preload stream before resuming vCPUs (no UFFD yet). Make `restore_and_run` delegate through.

**Files:** `src/vmm/src/snapshot_store.rs`, `src/vmm/src/builder.rs`, `src/libkrun/src/lib.rs`
**Tests:** Existing restore tests pass through new code path.

### Phase 3: UFFD handler (demand-paging)

Add `uffd` feature flag and `userfaultfd` dependency. Implement `UffdHandler` in `src/vmm/src/uffd.rs`: UFFD creation, memory registration, `AsyncFd` wrapping, fault loop with `tokio::spawn` per fault, EEXIST handling, error signaling. Wire into `restore_and_run_with_store` when `uffd` feature enabled.

**Files:** `src/vmm/src/uffd.rs` (new), `src/vmm/src/builder.rs`, `src/vmm/Cargo.toml`
**Tests:** Unit test with mock `SnapshotStore` (empty preload, tracks `read_page` calls).

### Phase 4: Preload integration

Add preload task alongside fault loop. Both run concurrently on UFFD thread's runtime. EEXIST races handled. `FsSnapshotStore::preload` yields 4MB sequential chunks with multi-page UFFDIO_COPY.

**Files:** `src/vmm/src/uffd.rs`
**Tests:** FsSnapshotStore restore — verify preload loads everything, near-zero faults.

### Phase 5: PageTracker + stats

Add atomic bitmap for loaded-page tracking. Expose stats: pages loaded via preload vs fault, total fault count, restore progress percentage.

**Files:** `src/vmm/src/uffd.rs`
**Tests:** Unit tests for PageTracker bitmap operations.

### Phase 6: Integration tests

- Demand-paging test: empty-preload store, all pages via faults, guest runs correctly
- Preload test: FsSnapshotStore, preload loads everything
- Mixed test: partial-preload store, both paths exercise
- Incremental test: base + 2 incrementals via demand-paging, latest dirty pages win
- Error test: store fails `read_page`, VM gets clean `VmExit::Error`
- Parallel test: slow store (50ms/page), multiple vCPUs, verify wall-clock time confirms concurrent resolution

**Files:** `tests/` workspace

## Additional Considerations

### Performance

- **BoxFuture allocation per `read_page`**: ~50ns per fault. Dwarfed by network latency (1-100ms for S3/CAS) and acceptable even for filesystem (~1us pread).
- **UFFD registration overhead**: Kernel maintains per-page tracking structures. ~500KB for 1GB RAM. Acceptable for unified flow.
- **Preload UFFDIO_COPY**: Multi-page `len` keeps syscall count low. ~256 calls for 1GB/4MB chunks.
- **No `&mut [u8]` buffer reuse**: Each spawned fault task allocates a `Vec<u8>` for the page. Pool optimization possible later if profiling shows need.

### Feature Flags

| Feature | Gates |
|---------|-------|
| `snapshot` | `SnapshotStore`, `SnapshotStoreFactory`, `FsSnapshotStore`, write path, serialization |
| `uffd` | `UffdHandler`, `PageTracker`, demand-paging restore path, `userfaultfd` crate dep |

`uffd` implies `snapshot`. Linux-only (gated with `#[cfg(target_os = "linux")]`).

### Dependencies

| Crate | Feature | Purpose |
|-------|---------|---------|
| `userfaultfd` 0.9 | `uffd` | UFFD creation and ioctls |
| `futures` | `snapshot` | `BoxFuture`, `BoxStream` (already in tree) |
| `tokio` | `uffd` | Runtime on UFFD thread (already in tree) |

### Security

- UFFD is registered with `user_mode_only(true)` — cannot handle kernel-mode faults
- `close_on_exec` set on UFFD fd
- Guest memory regions are the only registered ranges
- No fd passing to external processes (in-process only)

## Glossary

- **UFFD / userfaultfd**: Linux kernel mechanism for userspace page fault handling. Allows a handler thread to intercept and resolve page faults by providing page contents via `UFFDIO_COPY`.
- **UFFDIO_COPY**: Kernel ioctl that atomically copies data into a faulting page and wakes blocked threads. Supports multi-page `len`. Returns `EEXIST` if page already mapped.
- **Demand paging**: Loading memory pages on-fault rather than eagerly at restore time. Reduces cold restore latency by deferring I/O until pages are actually accessed.
- **Preload stream**: A `BoxStream` of `(guest_addr, data)` chunks that the backend yields for proactive page loading. Runs concurrently with the fault handler. Filesystem backends yield all memory; remote backends yield cached data.
- **PageTracker**: Atomic bitmap tracking which guest pages have been loaded (via preload or fault). Used for stats and monitoring, not correctness.
- **vmstate**: Bincode-serialized snapshot metadata (header + vCPU states + device states). Small (~KB), always loaded eagerly.
- **CAS**: Content-Addressable Storage. Objects addressed by hash of content. Used as a storage backend behind the `SnapshotStore` trait.
- **Page map / index**: Backend-internal data structure mapping guest addresses to storage locations across base + incremental snapshot layers. Each `SnapshotStore` implementation manages its own.
- **PTL**: Page Table Lock. Kernel lock that serializes `UFFDIO_COPY` operations on the same page, preventing races.
