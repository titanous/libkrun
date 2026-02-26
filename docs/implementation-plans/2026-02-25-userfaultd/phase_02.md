# Userfaultd Implementation Plan — Phase 2: FsSnapshotStore Read Path (Eager, No UFFD)

**Goal:** Implement the read side of `FsSnapshotStore` and wire `Context::restore_and_run_with_store` as an eager restore (drains preload before resuming vCPUs).

**Architecture:** `FsSnapshotStore` read methods parse snapshot files into page-level access. `read_vmstate` returns the raw vmstate bytes. `read_page` resolves the latest version of a page across base + incremental layers. `preload` yields sequential 4MB chunks. The new `restore_and_run_with_store` on `Context` creates a store from the factory, loads vmstate eagerly, drains the preload stream to populate memory, then resumes vCPUs.

**Tech Stack:** Rust, futures (BoxFuture/BoxStream/StreamExt), bincode, vm-memory

**Scope:** 6 phases from original design (phase 2 of 6)

**Codebase verified:** 2026-02-25

---

## Acceptance Criteria Coverage

This phase implements and tests:

### userfaultd.AC2: FsSnapshotStore (read path)
- **userfaultd.AC2.3 Success:** Read path supports base + N incremental overlays; `read_page` resolves latest version
- **userfaultd.AC2.4 Success:** `preload` yields memory in sequential chunks (4MB default), enabling efficient UFFDIO_COPY
- **userfaultd.AC2.5 Success:** Existing `restore_and_run(path, incrementals)` delegates to `restore_and_run_with_store` using `FsSnapshotStoreFactory` — zero behavior change

### userfaultd.AC4: Backward compatibility (restore path)
- **userfaultd.AC4.1 Success:** `Context::restore_and_run(path, incrementals)` still works, delegates internally
- **userfaultd.AC4.3 Success:** Existing snapshot integration tests pass without modification

---

## Reference Files

- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/CLAUDE.md` — VMM crate contracts
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/src/snapshot.rs` — Snapshot format, `SnapshotHeader`, `VmSnapshot`, `IncrementalSnapshot`, `DirtyPage`, `load_vmstate`, `load_memory`, `apply_dirty_pages`
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/src/builder.rs:669-719` — Current `restore_from_snapshot` cold restore flow
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/src/lib.rs:466-534` — Current `Vmm::restore_snapshot` (loads vmstate, validates header, loads memory, restores devices/vCPUs)
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/libkrun/src/lib.rs:3147-3165` — Current `Context::restore_and_run`

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Implement FsSnapshotStore read methods

**Verifies:** userfaultd.AC2.3, userfaultd.AC2.4

**Files:**
- Modify: `src/vmm/src/snapshot_store.rs` (implement read_vmstate, read_page, preload on FsSnapshotStore)

**Implementation:**

`FsSnapshotStore` needs internal state for the read path:
- `base_path: PathBuf` — path to the base snapshot directory (contains `vmstate` and `memory` files)
- `incremental_paths: Vec<PathBuf>` — ordered list of incremental snapshot file paths
- `header: Option<SnapshotHeader>` — cached header from vmstate (populated lazily or during factory create)
- `incremental_snapshots: Vec<IncrementalSnapshot>` — loaded incrementals (for page resolution)
- `dirty_page_index: HashMap<u64, (usize, usize)>` — maps `guest_addr` → `(incremental_index, dirty_page_index)` for O(1) lookup, newest-first resolution

The `FsSnapshotStoreFactory::create()` method should:
1. Load and deserialize the base vmstate (`load_vmstate`)
2. Load all incrementals (`load_incremental_snapshot` for each)
3. Build the `dirty_page_index` by iterating incrementals newest-first: for each dirty page, insert `(guest_addr → (inc_idx, page_idx))` only if not already present (newest wins)
4. Construct `FsSnapshotStore` with all loaded data

**`read_vmstate(&self)`**: Return the **merged** vmstate — a single `VmSnapshot` serialized as bytes that represents the final state across base + all incrementals:
- If no incrementals: return base `VmSnapshot` serialized bytes (read from `base_path/vmstate`)
- If incrementals exist: construct a `VmSnapshot` using the base header (for `ram_regions` layout) but with vCPU states, device states, gic_state, and vm_state from the **last** incremental (since each incremental contains complete state that overwrites the previous). Serialize this merged `VmSnapshot` via bincode and return the bytes.

The VMM layer receives one vmstate blob, deserializes it, validates the header, and restores device/vCPU state from it. Memory loading is handled separately via `read_page` / `preload`.

**Page size handling:** The existing `PAGE_SIZE = 16384` constant in `snapshot.rs` is for Apple Silicon incremental snapshots only. For the `SnapshotStore` trait, `read_page` returns one system page. Add a new function `fn system_page_size() -> u64` in `snapshot_store.rs` that returns `libc::sysconf(libc::_SC_PAGESIZE) as u64` on Linux (4096 for x86_64, 65536 for aarch64). This is used by both `FsSnapshotStore::read_page` and the UFFD handler. The existing `PAGE_SIZE` constant remains unchanged for macOS incremental snapshot compatibility.

**`read_page(&self, guest_addr: u64)`**: Look up `guest_addr` in `dirty_page_index`:
- If found: return the dirty page data from the indicated incremental. Each `DirtyPage` entry uses the snapshot's page granularity (4KB on Linux KVM, 16KB on macOS HVF). Since UFFD is Linux-only, entries will be at 4KB granularity matching `system_page_size()`.
- If not found: compute file offset from `header.ram_regions` (sum sizes of regions before the one containing `guest_addr`, add offset within region), read `system_page_size()` bytes from the base `memory` file at that offset.

**`preload(&self, regions: Vec<(u64, u64)>)`**: Return a `BoxStream` that yields `(guest_addr, data)` chunks. For FsSnapshotStore, iterate through each `(base_addr, size)` region, read the base memory file in 4MB sequential chunks, then overlay dirty pages from incrementals. Each yielded item is `(chunk_start_addr, chunk_data)` where `chunk_data` is up to 4MB. Apply dirty pages inline: for each 4MB chunk, check if any dirty pages fall within the range and overwrite them in the chunk buffer before yielding.

**Testing:**

Tests must verify:
- userfaultd.AC2.3: Create temp directory with base snapshot + 2 incrementals. Verify `read_page` returns data from the latest incremental for pages dirty in multiple incrementals. Verify `read_page` returns base data for clean pages.
- userfaultd.AC2.4: Call `preload` and collect all chunks. Verify they cover the full memory range in sequential order. Verify chunk sizes are 4MB (or smaller for the last chunk).

**Verification:**
Run: `cargo test -p vmm --features snapshot`
Expected: All tests pass

**Commit:** `feat(vmm): implement FsSnapshotStore read path with incremental overlay resolution`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Wire Context::restore_and_run_with_store and delegate existing method

**Verifies:** userfaultd.AC2.5, userfaultd.AC4.1, userfaultd.AC4.3

**Files:**
- Modify: `src/vmm/src/builder.rs` (add `restore_from_store` method on `BuiltVm`)
- Modify: `src/vmm/src/lib.rs` (add `restore_from_store` method on `Vmm` that takes vmstate bytes + uses store for memory)
- Modify: `src/libkrun/src/lib.rs` (add `restore_and_run_with_store` on `Context`, make `restore_and_run` delegate)

**Implementation:**

The new restore flow using stores works as follows:

1. `Context::restore_and_run_with_store(self, factory: Box<dyn SnapshotStoreFactory>)`:
   - Creates a tokio runtime (single-threaded) for async store operations
   - Calls `factory.create()` to get the store
   - Calls `store.read_vmstate()` to get vmstate bytes
   - Deserializes vmstate, validates header
   - Calls `self.built_vm.restore_from_store(vmstate, store)` which:
     - Starts vCPU threads paused (same as current `restore_from_snapshot`)
     - Unblocks secondary vCPUs (macOS, same as current)
     - Quiesces device workers
     - Drains `store.preload(regions)` stream to populate guest memory (for eager restore, this loads all memory)
     - Restores device states, interrupt controller, VM state, vCPU states from the vmstate
     - Completes device restores, resumes device workers
     - Resumes vCPUs
   - Runs event loop (same as current `restore_and_run`)

2. `Context::restore_and_run(self, base_path, incremental_paths)` becomes:
   ```rust
   pub fn restore_and_run(self, base_path: &Path, incremental_paths: &[&Path])
       -> Result<VmExit, StartError>
   {
       let factory = FsSnapshotStoreFactory::new(base_path, incremental_paths);
       self.restore_and_run_with_store(Box::new(factory))
   }
   ```

**Key detail for eager preload:** The preload stream from `FsSnapshotStore` yields `(guest_addr, data)` chunks. For eager restore, the builder drains the entire stream and writes each chunk to guest memory using `guest_memory.write_slice(&data, GuestAddress(guest_addr))`. This replaces the current `load_memory()` call. After draining, all memory is populated and vCPUs can resume.

**Memory population approach:** The preload chunks already have dirty pages overlaid (from Task 1), so a single pass through the preload stream populates the final memory state. No separate incremental apply step needed.

**Testing:**

Tests must verify:
- userfaultd.AC2.5: Write a unit test that creates an `FsSnapshotStoreFactory`, verifies it produces a store, and the store's `preload` yields the expected data. (Integration test coverage via existing tests.)
- userfaultd.AC4.1: Existing `Context::restore_and_run(path, incrementals)` still works — verified by running existing integration tests.
- userfaultd.AC4.3: Run existing snapshot integration tests (`snapshot-restore-full`, `snapshot-restore-incremental`) — they must pass without modification.

**Verification:**
Run: `cargo test -p vmm --features snapshot`
Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-restore-full`
Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-restore-incremental`
Expected: All tests pass

**Commit:** `feat(vmm): wire eager restore through SnapshotStore, delegate restore_and_run`
<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->
