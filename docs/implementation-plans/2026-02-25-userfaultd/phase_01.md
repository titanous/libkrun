# Userfaultd Implementation Plan — Phase 1: SnapshotStore Trait + FsSnapshotStore Write Path

**Goal:** Define the `SnapshotStore` and `SnapshotStoreFactory` traits and implement the filesystem write path, wiring existing snapshot methods to delegate through.

**Architecture:** New `snapshot_store.rs` module defines async traits following the `AsyncBlockBackendFactory`/`AsyncBlockBackend` pattern. `FsSnapshotStore` wraps current `snapshot.rs` write functions. Existing `VmHandle::snapshot(path)` becomes a thin wrapper.

**Tech Stack:** Rust, futures (BoxFuture/BoxStream), bincode, tokio (for factory async)

**Scope:** 6 phases from original design (phase 1 of 6)

**Codebase verified:** 2026-02-25

---

## Acceptance Criteria Coverage

This phase implements and tests:

### userfaultd.AC1: SnapshotStore trait
- **userfaultd.AC1.1 Success:** `SnapshotStore` trait has six methods: `read_vmstate`, `read_page`, `preload`, `write_vmstate`, `write_pages`, `close`
- **userfaultd.AC1.2 Success:** All methods return `BoxFuture<'_, io::Result<T>>` (or `BoxStream` for `preload`), consistent with existing `AsyncBlockBackend` pattern
- **userfaultd.AC1.3 Success:** Trait is `Send + Sync + 'static` and object-safe (`Box<dyn SnapshotStore>` compiles)
- **userfaultd.AC1.4 Success:** `SnapshotStoreFactory` trait has one method (`create`), returns `BoxFuture<'static, io::Result<Box<dyn SnapshotStore>>>`, is `Send + 'static`
- **userfaultd.AC1.5 Success:** No snapshot IDs or lineage concepts in libkrun — caller manages identity externally

### userfaultd.AC2: FsSnapshotStore (write path only)
- **userfaultd.AC2.1 Success:** `FsSnapshotStore` implements `SnapshotStore` for filesystem-backed snapshots
- **userfaultd.AC2.2 Success:** Write path produces files compatible with current snapshot format (`vmstate` + `memory` files)

### userfaultd.AC4: Backward compatibility (write path only)
- **userfaultd.AC4.2 Success:** `VmHandle::snapshot(path)` and `incremental_snapshot(path)` still work, delegate internally
- **userfaultd.AC4.4 Success:** No changes to snapshot file format

---

## Reference Files

The executor should read these files for context on testing patterns and crate conventions:

- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/CLAUDE.md` — project-level conventions and test commands
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/vmm/CLAUDE.md` — VMM crate contracts and invariants
- `/home/titanous/vm-platform/libkrun/.worktrees/userfaultd/src/libkrun/CLAUDE.md` — libkrun API contracts

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->

<!-- START_TASK_1 -->
### Task 1: Define SnapshotStore and SnapshotStoreFactory traits

**Verifies:** userfaultd.AC1.1, userfaultd.AC1.2, userfaultd.AC1.3, userfaultd.AC1.4, userfaultd.AC1.5

**Files:**
- Create: `src/vmm/src/snapshot_store.rs`
- Modify: `src/vmm/src/lib.rs` (add `pub mod snapshot_store` declaration)
- Modify: `src/vmm/Cargo.toml` (add `futures` dependency gated on `snapshot` feature)

**Implementation:**

Create `src/vmm/src/snapshot_store.rs` with the trait definitions. Follow the project's existing pattern of defining `BoxFuture`/`SendBoxFuture` type aliases locally (as done in `src/devices/src/virtio/block/mod.rs:208-211`). The traits must be object-safe and use `BoxFuture` (not native async fn) for `Send` enforcement, consistent with `AsyncBlockBackend`.

The `SnapshotStore` trait has six methods split into read (3) and write (3) groups:
- Read: `read_vmstate` → `BoxFuture<'_, io::Result<Vec<u8>>>`, `read_page(guest_addr: u64)` → `BoxFuture<'_, io::Result<Vec<u8>>>`, `preload(regions: Vec<(u64, u64)>)` → `BoxStream<'_, io::Result<(u64, Vec<u8>)>>`
- Write: `write_vmstate(data: Vec<u8>)` → `BoxFuture<'_, io::Result<()>>`, `write_pages(pages: Vec<(u64, Vec<u8>)>)` → `BoxFuture<'_, io::Result<()>>`, `close()` → `BoxFuture<'_, io::Result<()>>`

The `SnapshotStoreFactory` trait has one method: `create(self: Box<Self>)` → `BoxFuture<'static, io::Result<Box<dyn SnapshotStore>>>`. Factory is `Send + 'static` (consumed by worker thread, same as `AsyncBlockBackendFactory`).

No snapshot IDs, lineage, or identity concepts — caller manages these externally.

Add `futures` as an optional dependency in `src/vmm/Cargo.toml`, gated on the `snapshot` feature (needed for `futures::stream::BoxStream`).

Add `pub mod snapshot_store;` to `src/vmm/src/lib.rs`, gated with `#[cfg(feature = "snapshot")]`.

**Testing:**

Tests must verify each AC listed above:
- userfaultd.AC1.1: Compile-time verification that trait has all six methods (test creates a mock struct implementing SnapshotStore)
- userfaultd.AC1.2: Compile-time verification that return types are correct BoxFuture/BoxStream
- userfaultd.AC1.3: Write a test that creates a `Box<dyn SnapshotStore>` to prove object safety; the mock struct must be `Send + Sync + 'static`
- userfaultd.AC1.4: Write a test that boxes a factory and calls `create` to verify the signature compiles
- userfaultd.AC1.5: Inspection — trait has no ID parameters (compile-time check via the mock)

Tests go in a `#[cfg(test)] mod tests` block inside `snapshot_store.rs`. The mock `SnapshotStore` implementation can return dummy data or errors — the point is compile-time verification of trait signatures and object safety.

**Verification:**
Run: `cargo test -p vmm --features snapshot`
Expected: All tests pass, including new trait object-safety tests

**Commit:** `feat(vmm): define SnapshotStore and SnapshotStoreFactory traits`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Implement FsSnapshotStore write path

**Verifies:** userfaultd.AC2.1, userfaultd.AC2.2

**Files:**
- Modify: `src/vmm/src/snapshot_store.rs` (add FsSnapshotStore struct and write-side trait impl)

**Implementation:**

Add `FsSnapshotStore` struct and `FsSnapshotStoreFactory` struct to `snapshot_store.rs`.

`FsSnapshotStoreFactory` holds the base path and incremental paths (for restore — but for write, it only needs the output directory path). The factory `create` method constructs the `FsSnapshotStore`.

`FsSnapshotStore` write path delegates to existing snapshot.rs functions and produces format-compatible files:

- `write_vmstate(data)`: Creates the snapshot directory if needed, writes `data` (pre-serialized bincode bytes) to `{path}/vmstate`, calls `sync_all()`. For full snapshots, `data` is a serialized `VmSnapshot`. For incremental snapshots, `data` is a serialized `IncrementalSnapshot` (which already embeds dirty page data).
- `write_pages(pages)`: Writes raw contiguous memory to `{path}/memory`. The `pages` parameter is a `Vec<(u64, Vec<u8>)>` of `(guest_addr, page_data)` pairs, ordered sequentially. For a full snapshot, this is all guest memory (matching existing `dump_memory` format). For incremental snapshots, callers pass an empty vec (dirty pages are already embedded in the `IncrementalSnapshot` vmstate blob).
- `close()`: Calls `sync_all()` on the snapshot directory to ensure durability. Returns `Ok(())`.

Read-side methods (`read_vmstate`, `read_page`, `preload`) return `unimplemented!()` for now — they'll be implemented in Phase 2.

**Testing:**

Tests must verify:
- userfaultd.AC2.1: `FsSnapshotStore` implements `SnapshotStore` (compile-time, via using it as `Box<dyn SnapshotStore>`)
- userfaultd.AC2.2: Write a test that calls `write_vmstate` + `write_pages` + `close`, then verifies the output files match the current snapshot format. Use a temp directory. Write known vmstate bytes and memory pages, then read them back with existing `load_vmstate` and `load_memory` functions to verify compatibility.

**Verification:**
Run: `cargo test -p vmm --features snapshot`
Expected: All tests pass

**Commit:** `feat(vmm): implement FsSnapshotStore write path`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Wire VmHandle snapshot methods to delegate through SnapshotStore

**Verifies:** userfaultd.AC4.2, userfaultd.AC4.4

**Files:**
- Modify: `src/vmm/src/lib.rs` (add `snapshot_to_store` / `incremental_snapshot_to_store` on `Vmm`)
- Modify: `src/libkrun/src/lib.rs` (add `snapshot_to_store` / `incremental_snapshot_to_store` on `VmHandle`, wire existing path-based methods to delegate)

**Implementation:**

Add two new methods to `Vmm` in `src/vmm/src/lib.rs`:
- `pub fn snapshot_to_store(&mut self, store: &dyn SnapshotStore) -> Result<(), SnapshotError>` — collects device/vCPU states (same as existing `create_snapshot`), serializes to VmSnapshot, calls `store.write_vmstate()`, dumps memory via `store.write_pages()`, calls `store.close()`. Since the store methods are async but Vmm methods are sync, use `tokio::runtime::Handle::current().block_on()` or create a small runtime.
- `pub fn incremental_snapshot_to_store(&mut self, store: &dyn SnapshotStore) -> Result<(), SnapshotError>` — same pattern for incremental snapshots.

**Important:** The Vmm snapshot methods currently run synchronously. The store trait is async. For the write path, since filesystem writes are fast and the VmHandle methods pause vCPUs first, blocking on the async store is acceptable. Use `futures::executor::block_on()` (simpler than creating a tokio runtime for sync wrappers).

Add two new methods to `VmHandle` in `src/libkrun/src/lib.rs`:
- `pub fn snapshot_to_store(&self, store: Box<dyn SnapshotStore>) -> Result<(), StartError>` — pauses vCPUs, calls `vmm.snapshot_to_store(&*store)`, resumes vCPUs.
- `pub fn incremental_snapshot_to_store(&self, store: Box<dyn SnapshotStore>) -> Result<(), StartError>` — same pattern.

Make existing path-based methods delegate:
- `VmHandle::snapshot(path)` creates an `FsSnapshotStore` (or `FsSnapshotStoreFactory`) and delegates to `snapshot_to_store`.
- `VmHandle::incremental_snapshot(path)` similarly delegates.

**Delegation approach:** Make the existing `create_snapshot` internally construct an `FsSnapshotStore` and delegate to `snapshot_to_store`. This way there's one code path for both the path-based and store-based APIs.

**Testing:**

Tests must verify:
- userfaultd.AC4.2: Existing `VmHandle::snapshot(path)` and `incremental_snapshot(path)` still work — verified by existing integration tests passing without modification.
- userfaultd.AC4.4: No changes to snapshot file format — verified by the AC2.2 test (write + read-back roundtrip).

No new unit tests needed here beyond running existing tests to confirm no regression. The wiring is verified by the existing snapshot integration tests.

**Verification:**
Run: `cargo test -p vmm --features snapshot`
Run: `make test FEATURE_FLAGS="--features embedded_init" TEST=snapshot-restore-full`
Expected: All tests pass (unit and integration)

**Commit:** `feat(vmm): wire VmHandle snapshot methods through SnapshotStore`
<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->
