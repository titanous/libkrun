# Virtio-FS Device

Last verified: 2026-03-03

## Purpose
Virtio-FS (FUSE-over-virtio) device implementation. Exposes a generic `FileSystem` trait for pluggable filesystem backends, with `PassthroughFs` as the built-in host directory backend. Linux-only (macOS virtiofs was removed).

## Contracts
- **Exposes**: `Fs` device, `FileSystem` trait, `DaxMapper` trait, `Server` (also re-exported from `devices::lib.rs`), `PassthroughFs`, `ExportTable`, newtype wrappers (`Inode`, `Handle`), FUSE types (`Context`, `Entry`, `DirEntry`, etc.)
- **Guarantees**:
  - `FileSystem` is object-safe (no associated types); uses concrete `Inode(u64)` and `Handle(u64)` newtypes
  - All `FileSystem` methods have default implementations returning `ENOSYS`
  - `FileSystem::set_export_table(&mut self, _) -> u64` defaults to no-op (returns 0)
  - `setupmapping`/`removemapping` accept `&dyn DaxMapper` (not raw addresses)
  - `DaxMapper` trait (`map_file`, `map_data`, `unmap`) abstracts platform-specific DAX window operations; is `Send + Sync`
  - `LinuxDaxMapper` performs bounds checking before every mmap call
  - `Fs::new(tag, Box<dyn FileSystem + Send + Sync>, exit_code)` -- device does not create the filesystem backend
  - `Server::new(Box<dyn FileSystem + Send + Sync>)` -- no longer generic over `F: FileSystem`
  - `ZeroCopyReader: io::Read` and `ZeroCopyWriter: io::Write` (supertraits added)
- **Expects**: `FileSystem` backend is constructed and boxed by the caller (Builder or VMM)

## Dependencies
- **Uses**: `vm-memory`, `libc` (mmap for LinuxDaxMapper), `bindings` (FUSE/Linux errno constants)
- **Used by**: `vmm::builder` (creates `Fs` device from `FsMount`), `libkrun` (re-exports `FileSystem`, `passthrough`, `dax_mapper`)
- **Boundary**: `filesystem`, `dax_mapper`, `fuse` modules are `pub`; `fuse_dispatch`, `worker`, `device` are crate-internal; `server` is `pub(crate)` but re-exported from `devices::lib.rs`

## Key Decisions
- `FileSystem` made object-safe by replacing associated types `Inode`/`Handle` with concrete newtypes -- enables `Box<dyn FileSystem>` throughout
- DAX operations abstracted behind `DaxMapper` trait instead of raw mmap calls -- enables future non-Linux DAX backends
- macOS virtiofs passthrough removed entirely (was 2600+ lines); `linux/` module no longer behind `#[cfg(target_os)]`
- `Fs` device takes ownership of `Box<dyn FileSystem>` via `Option` and `.take()`s it during activate

## Key Files
- `filesystem.rs` - `FileSystem` trait (object-safe), `Inode`/`Handle` newtypes, `ZeroCopyReader`/`ZeroCopyWriter`, `Entry`, `Context`, `ExportTable`
- `dax_mapper.rs` - `DaxMapper` trait, `LinuxDaxMapper` (mmap-based, bounds-checked)
- `device.rs` - `Fs` struct (VirtioDevice impl), takes `Box<dyn FileSystem + Send + Sync>`
- `server.rs` - `Server` (FUSE message dispatch entry point), creates `LinuxDaxMapper` for DAX operations
- `fuse_dispatch.rs` - FUSE opcode dispatch logic extracted from server.rs; maps FUSE opcodes to FileSystem trait methods
- `worker.rs` - `FsWorker` (queue processing thread)
- `linux/passthrough.rs` - `PassthroughFs` (host directory passthrough via O_PATH fds)
