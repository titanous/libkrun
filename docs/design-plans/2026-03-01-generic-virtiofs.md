# Generic Virtiofs Design

## Summary

libkrun's in-process virtiofs device is currently hardcoded to a single filesystem backend, `PassthroughFs`, which exposes a host directory into a guest VM. This design refactors the virtio-fs device stack so the `FileSystem` trait becomes a stable, publicly exported interface that any backend can implement — not just `PassthroughFs`. The concrete change is replacing all static generic parameters (`Server<F: FileSystem>`, `Fs` holding a `PassthroughFs` config) with dynamic dispatch (`Box<dyn FileSystem>`) throughout the device, worker, and builder layers, following the pattern already used for `VirtioDevice` throughout the rest of the codebase.

A secondary goal is cleaning up the two main object-safety barriers that currently prevent `dyn FileSystem` from compiling: associated types (`Inode`, `Handle`) and generic method parameters on `read`, `write`, and `readdir`. Alongside this, a new `DaxMapper` trait abstracts platform-specific DAX window operations so that `FileSystem` implementations never call `mmap` directly and macOS-specific parameters are removed from the trait interface. The result is that external callers — for example, a holodeck chunk-store backend — can provide a `Box<dyn FileSystem>` to `Builder::add_virtiofs()` and have it mounted inside a VM without any changes to the device or VMM layers.

## Definition of Done

1. `DaxMapper` trait abstracts platform-specific DAX window operations (`map_file`, `map_data`, `unmap`) — `FileSystem` impls never call `libc::mmap` directly for DAX mappings
2. `Fs` virtio device accepts any `FileSystem` trait implementation (not hardcoded to `PassthroughFs`)
3. `FileSystem` trait publicly exported from the `devices` crate for external implementors (e.g., holodeck chunk-store)
4. macOS-specific `map_sender` removed from `FileSystem` trait; `setupmapping`/`removemapping` take `&dyn DaxMapper` instead
5. `LinuxDaxMapper` implements `DaxMapper` with `mmap(MAP_FIXED)` semantics
6. `PassthroughFs` updated to use `DaxMapper` and works through the generic `Fs` device as demonstration/test
7. `Builder` API supports registering custom `FileSystem` impls with optional DAX window size
8. Existing integration tests pass through the new generic path

## Acceptance Criteria

### generic-virtiofs.AC1: DaxMapper trait abstracts DAX operations
- **generic-virtiofs.AC1.1 Success:** `map_file` maps a file region into the DAX window at the specified offset
- **generic-virtiofs.AC1.2 Success:** `map_data` maps anonymous memory with provided data at the specified offset
- **generic-virtiofs.AC1.3 Success:** `unmap` replaces a DAX range with inaccessible pages
- **generic-virtiofs.AC1.4 Failure:** `map_file` rejects mapping that would exceed DAX window bounds
- **generic-virtiofs.AC1.5 Failure:** `unmap` rejects range that would exceed DAX window bounds

### generic-virtiofs.AC2: Fs device accepts any FileSystem impl
- **generic-virtiofs.AC2.1 Success:** `Fs::new()` accepts `Box<dyn FileSystem>` and stores it
- **generic-virtiofs.AC2.2 Success:** `Fs::activate()` transfers backend ownership to FsWorker via `Option::take()`

### generic-virtiofs.AC3: FileSystem trait is publicly exported
- **generic-virtiofs.AC3.1 Success:** External crate can import `FileSystem` trait from `devices` crate
- **generic-virtiofs.AC3.2 Success:** All types referenced in FileSystem method signatures are also exported

### generic-virtiofs.AC4: FileSystem trait is object-safe
- **generic-virtiofs.AC4.1 Success:** `Box<dyn FileSystem>` compiles
- **generic-virtiofs.AC4.2 Success:** `setupmapping` and `removemapping` accept `&dyn DaxMapper` (no platform-specific params)
- **generic-virtiofs.AC4.3 Success:** `Inode` and `Handle` are shared newtypes preventing mix-ups at the type level

### generic-virtiofs.AC5: PassthroughFs uses DaxMapper
- **generic-virtiofs.AC5.1 Success:** PassthroughFs `setupmapping` calls `mapper.map_file()` for regular files
- **generic-virtiofs.AC5.2 Success:** PassthroughFs `setupmapping` calls `mapper.map_data()` for init_inode
- **generic-virtiofs.AC5.3 Success:** PassthroughFs `removemapping` calls `mapper.unmap()` for each request
- **generic-virtiofs.AC5.4 Success:** PassthroughFs no longer contains any direct `libc::mmap` calls for DAX

### generic-virtiofs.AC6: Server holds Box\<dyn FileSystem\>
- **generic-virtiofs.AC6.1 Success:** `Server` is no longer generic (`Server` not `Server<F>`)
- **generic-virtiofs.AC6.2 Success:** FUSE_SETUPMAPPING dispatch creates `LinuxDaxMapper` from `VirtioShmRegion` and passes to FileSystem

### generic-virtiofs.AC7: Builder API supports custom backends
- **generic-virtiofs.AC7.1 Success:** `Builder::add_virtiofs(tag, Box<dyn FileSystem>, Option<usize>)` registers a filesystem mount
- **generic-virtiofs.AC7.2 Success:** DAX window is allocated via ShmManager when `shm_size` is `Some`
- **generic-virtiofs.AC7.3 Success:** No DAX window allocated when `shm_size` is `None`
- **generic-virtiofs.AC7.4 Success:** Multiple `add_virtiofs()` calls create independent devices with separate DAX windows

### generic-virtiofs.AC8: Existing integration tests pass
- **generic-virtiofs.AC8.1 Success:** `make test FEATURE_FLAGS="--features embedded_init"` passes with virtiofs tests exercising the new generic path

## Glossary

- **virtiofs**: A VirtIO device type that exposes a shared filesystem from host to guest over the virtio transport. Uses FUSE as its internal protocol.
- **FUSE**: Filesystem in Userspace. A Linux kernel interface allowing filesystem logic to run in a userspace process. virtiofs reuses the FUSE wire protocol between the guest kernel and the host-side `Server`.
- **PassthroughFs**: The existing `FileSystem` implementation in this codebase. Passes filesystem operations through to a real host directory using Linux file descriptors and `O_PATH` handles.
- **DAX (Direct Access)**: A mechanism where the guest maps host file data directly into its physical address space via a shared memory window, bypassing the virtqueue copy path. Controlled by `FUSE_SETUPMAPPING` and `FUSE_REMOVEMAPPING` opcodes.
- **DAX window**: A fixed-size physical memory region (GPA range) pre-allocated in the guest address space for DAX mappings. Managed by `ShmManager` and represented as `VirtioShmRegion`.
- **GPA (Guest Physical Address)**: The physical memory address as seen by the guest VM, distinct from the host virtual address.
- **`mmap(MAP_FIXED)`**: A Linux syscall that maps a file or anonymous memory into a specific virtual address. Used by `LinuxDaxMapper` to place file data at a precise offset inside the DAX window.
- **object safety**: A Rust rule that determines whether a trait can be used as `dyn Trait`. Traits with associated types or generic methods are not object-safe and cannot be type-erased.
- **newtype wrapper**: A Rust idiom of wrapping a primitive in a single-field struct (`struct Inode(u64)`) to create a distinct type that cannot be accidentally mixed with the underlying primitive.
- **`Server`**: The in-process FUSE protocol dispatcher. Reads FUSE request buffers from the virtqueue, decodes opcodes, and calls the corresponding `FileSystem` method.
- **`FsWorker`**: The thread that owns the `Server` and drives the virtqueue processing loop for a virtiofs device.
- **`ShmManager`**: Internal component that allocates GPA ranges for shared memory regions across devices (virtiofs DAX windows, GPU framebuffer, etc.).
- **`ZeroCopyWriter` / `ZeroCopyReader`**: Traits that allow FUSE `read`/`write` data to flow directly into/out of virtqueue buffers without an intermediate heap copy.
- **vhost-user-fs**: A separate virtiofs backend mode where the filesystem server runs in a separate process over a Unix socket. Distinct from the in-process path this design modifies.

## Architecture

The in-process virtiofs device (`Fs`) is refactored from a monomorphic PassthroughFs wrapper into a generic device that accepts any `FileSystem` trait implementation via `Box<dyn FileSystem>`.

### Core Traits

**`FileSystem`** (`src/devices/src/virtio/fs/filesystem.rs`) — the main trait connecting a filesystem backend with the FUSE protocol server. Currently has associated types and generic methods that prevent object safety. Refactored to:

- Replace associated types `type Inode` and `type Handle` with shared newtype wrappers (`Inode(u64)` and `Handle(u64)`) defined at the trait level. All implementations use the same types, preventing inode/handle mix-ups while remaining object-safe.
- Replace generic method parameters with trait objects: `read<W: Write + ZeroCopyWriter>(w: W)` becomes `read(w: &mut dyn ZeroCopyWriter)` where `ZeroCopyWriter: Write`. Same for `write`, `readdir`, and `readdirplus`.
- Remove all `#[cfg(target_os = "macos")]` parameters (`map_sender`).
- Replace raw `host_shm_base: u64, shm_size: u64` in `setupmapping`/`removemapping` with `&dyn DaxMapper`.

**`DaxMapper`** (new, `src/devices/src/virtio/fs/dax_mapper.rs`) — abstracts platform-specific DAX window manipulation. FileSystem implementations call `DaxMapper` methods instead of `libc::mmap` directly:

```rust
pub trait DaxMapper: Send + Sync {
    fn map_file(&self, dax_offset: u64, len: u64, fd: RawFd, file_offset: u64, writable: bool) -> io::Result<()>;
    fn map_data(&self, dax_offset: u64, data: &[u8]) -> io::Result<()>;
    fn unmap(&self, dax_offset: u64, len: u64) -> io::Result<()>;
}
```

`LinuxDaxMapper` (internal, not exported) implements this with `mmap(MAP_FIXED)` and bounds checking. Created by `Server` from the `VirtioShmRegion` when dispatching FUSE_SETUPMAPPING/REMOVEMAPPING.

**`ZeroCopyWriter`** and **`ZeroCopyReader`** — existing traits, modified to add supertrait bounds (`ZeroCopyWriter: io::Write`, `ZeroCopyReader: io::Read`) so they can be used as `dyn` trait objects.

### Device Stack

```
Builder::add_virtiofs(tag, Box<dyn FileSystem>, shm_size)
  → VmResources stores FsMount { tag, fs: Box<dyn FileSystem>, shm_size }
  → build_microvm() calls attach_fs_devices()
    → Fs::new(tag, Box<dyn FileSystem>, exit_code)
    → Fs::activate() takes fs_backend via Option::take(), passes to FsWorker
      → FsWorker holds Server { fs: Box<dyn FileSystem> }
        → Server dispatches FUSE opcodes to FileSystem methods
        → For SETUPMAPPING/REMOVEMAPPING: creates LinuxDaxMapper, passes as &dyn DaxMapper
```

### Public API Surface

Exported from `devices` crate for external implementors:

- Traits: `FileSystem`, `DaxMapper`, `ZeroCopyReader`, `ZeroCopyWriter`
- Types: `Inode`, `Handle`, `Context`, `Entry`, `DirEntry`, `Extensions`, `FsOptions`, `OpenOptions`, `SetattrValid`, `RemovemappingOne`, `FileLock`, `GetxattrReply`, `ListxattrReply`
- Bindings: `stat64`, `statvfs64`, `ino64_t`
- Concrete impl: `PassthroughFs` (reference implementation)

Not exported (internal): `LinuxDaxMapper`, `Server`, `FsWorker`, `Fs`, FUSE wire types.

## Existing Patterns

The `VirtioDevice` trait already uses dynamic dispatch — devices are stored as `Arc<Mutex<dyn VirtioDevice>>` throughout the MMIO device manager and transport layer. This design follows that pattern: the `FileSystem` backend is type-erased inside the `Fs` device, which itself is type-erased at the `VirtioDevice` boundary.

The `Server<F: FileSystem + Sync>` is currently generic but only ever instantiated with `PassthroughFs`. Making `Server` hold `Box<dyn FileSystem>` instead follows the existing dynamic dispatch pattern rather than the monomorphization pattern.

The `ShmManager` already allocates GPA ranges for per-device shared memory regions (used by both virtiofs and GPU). DAX window setup for in-process virtiofs already works: `create_fs_region()` allocates GPAs, regions become part of `GuestMemoryMmap`, and `attach_fs_devices()` calls `set_shm_region()`. This design preserves that flow unchanged.

The vhost-user-fs device (`VhostUserFs`) is a completely separate code path with its own DAX memfd management and is not affected by this design.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: DaxMapper Trait and LinuxDaxMapper

**Goal:** Introduce the `DaxMapper` abstraction and its Linux implementation.

**Components:**
- New `src/devices/src/virtio/fs/dax_mapper.rs` — `DaxMapper` trait definition and `LinuxDaxMapper` struct
- `src/devices/src/virtio/fs/mod.rs` — add `pub mod dax_mapper`, export `DaxMapper` trait

**Dependencies:** None (first phase)

**Done when:** `DaxMapper` trait compiles, `LinuxDaxMapper` handles bounds checking and `mmap(MAP_FIXED)` for all three operations (`map_file`, `map_data`, `unmap`). Unit tests verify bounds checking rejects out-of-range offsets. Covers `generic-virtiofs.AC1`.
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: Make FileSystem Trait Object-Safe

**Goal:** Remove all object-safety barriers from the `FileSystem` trait.

**Components:**
- `src/devices/src/virtio/fs/filesystem.rs` — replace associated types with shared `Inode`/`Handle` newtypes, change generic methods to trait-object methods, remove macOS `map_sender` params, replace `host_shm_base`/`shm_size` with `&dyn DaxMapper`
- `src/devices/src/virtio/fs/server.rs` — update all dispatch calls to use new trait signatures, create `LinuxDaxMapper` for SETUPMAPPING/REMOVEMAPPING dispatch, remove macOS `map_sender` threading
- `src/devices/src/virtio/fs/mod.rs` — make `filesystem` module public, add re-exports

**Dependencies:** Phase 1 (DaxMapper trait)

**Done when:** `FileSystem` trait compiles as `dyn FileSystem`. `Server` compiles with updated dispatch. All re-exports accessible from outside the crate. Covers `generic-virtiofs.AC2`, `generic-virtiofs.AC3`, `generic-virtiofs.AC4`.
<!-- END_PHASE_2 -->

<!-- START_PHASE_3 -->
### Phase 3: Update PassthroughFs

**Goal:** Adapt the Linux PassthroughFs to the new trait signatures.

**Components:**
- `src/devices/src/virtio/fs/linux/passthrough.rs` — update `impl FileSystem for PassthroughFs`: use `Inode`/`Handle` newtypes, change `read`/`write`/`readdir`/`readdirplus` signatures, use `DaxMapper` in `setupmapping`/`removemapping` instead of raw `libc::mmap`

**Dependencies:** Phase 2 (object-safe FileSystem trait)

**Done when:** `PassthroughFs` compiles against the new trait. DAX mapping uses `mapper.map_file()` / `mapper.map_data()` / `mapper.unmap()`. Covers `generic-virtiofs.AC5`.
<!-- END_PHASE_3 -->

<!-- START_PHASE_4 -->
### Phase 4: Strip macOS Virtiofs Code

**Goal:** Remove all macOS-specific virtiofs code.

**Components:**
- Delete `src/devices/src/virtio/fs/macos/` directory (passthrough.rs, fs_utils.rs)
- `src/devices/src/virtio/fs/mod.rs` — remove `#[cfg(target_os = "macos")]` re-exports
- `src/devices/src/virtio/fs/device.rs` — remove `map_sender` field and `set_map_sender()` method
- `src/devices/src/virtio/fs/worker.rs` — remove `map_sender` field and threading

**Dependencies:** Phase 3 (PassthroughFs updated, macOS code no longer referenced)

**Done when:** No `#[cfg(target_os = "macos")]` remains in `src/devices/src/virtio/fs/`. Build succeeds on Linux.
<!-- END_PHASE_4 -->

<!-- START_PHASE_5 -->
### Phase 5: Generic Fs Device and FsWorker

**Goal:** Make the `Fs` device and `FsWorker` accept any `FileSystem` via `Box<dyn FileSystem>`.

**Components:**
- `src/devices/src/virtio/fs/device.rs` — replace `passthrough_cfg` field with `fs_backend: Option<Box<dyn FileSystem>>`, update `Fs::new()` to accept `Box<dyn FileSystem>`, `activate()` takes backend via `.take()`
- `src/devices/src/virtio/fs/worker.rs` — change `server: Server<PassthroughFs>` to `server: Server`, accept `Box<dyn FileSystem>` instead of `passthrough::Config`, remove `PassthroughFs::new()` call

**Dependencies:** Phase 3 (PassthroughFs works with new trait), Phase 4 (macOS code removed)

**Done when:** `Fs::new(tag, Box::new(PassthroughFs::new(cfg)?), exit_code)` compiles and works. `Server` holds `Box<dyn FileSystem>`. Covers `generic-virtiofs.AC6`.
<!-- END_PHASE_5 -->

<!-- START_PHASE_6 -->
### Phase 6: Builder API and VMM Integration

**Goal:** Wire up the Builder API and VMM builder to pass `Box<dyn FileSystem>` through to device creation.

**Components:**
- `src/libkrun/src/lib.rs` — replace `add_virtiofs(tag, path)` with `add_virtiofs(tag, Box<dyn FileSystem>, Option<usize>)`, remove C API functions (`krun_add_virtiofs`, `krun_add_virtiofs2`)
- `src/vmm/src/resources.rs` — replace `fs: Vec<FsDeviceConfig>` with `fs: Vec<FsMount>` where `FsMount { tag, fs: Box<dyn FileSystem>, shm_size }`
- `src/vmm/src/vmm_config/fs.rs` — remove `FsDeviceConfig` (or repurpose file for `FsMount`)
- `src/vmm/src/builder.rs` — update `attach_fs_devices()` to take `Vec<FsMount>`, drain `Box<dyn FileSystem>` instances, pass to `Fs::new()`

**Dependencies:** Phase 5 (generic Fs device)

**Done when:** `Builder::add_virtiofs("tag", Box::new(PassthroughFs::new(cfg)?), Some(256 * 1024 * 1024))` works end-to-end. `build_microvm()` creates `Fs` with the provided backend. Covers `generic-virtiofs.AC7`.
<!-- END_PHASE_6 -->

<!-- START_PHASE_7 -->
### Phase 7: Integration Test Validation

**Goal:** Verify existing integration tests pass through the new generic path.

**Components:**
- `tests/` workspace — update any test code that constructs virtiofs devices directly (if applicable)
- Fix any compilation or runtime issues surfaced by the refactor

**Dependencies:** Phase 6 (full pipeline wired up)

**Done when:** `make test FEATURE_FLAGS="--features embedded_init"` passes. Existing virtiofs integration tests exercise PassthroughFs through the generic `Fs` device with DAX. Covers `generic-virtiofs.AC8`.
<!-- END_PHASE_7 -->

## Additional Considerations

**macOS re-add path:** If macOS virtiofs DAX support is needed later, implement an `HvfDaxMapper` that sends HVF mapping messages instead of calling `mmap(MAP_FIXED)`. The `FileSystem` trait does not need to change — the `DaxMapper` abstraction was designed for this.

**Snapshot/restore:** The in-process virtiofs `Fs` device does not currently support snapshot/restore (unlike `VhostUserFs`). This design does not change that. If snapshot support is added later, `FileSystem` implementations would need to be `Serialize`/`Deserialize` or provide a factory for reconstruction — that's a separate design concern.

**Thread safety:** `FileSystem: Send + Sync` is required because the trait object is moved to the worker thread (`Send`) and called from it (`Sync` not strictly needed for single-threaded worker, but matches existing `Server<F: FileSystem + Sync>` bound). External implementors must ensure their `FileSystem` is `Send + Sync`.
