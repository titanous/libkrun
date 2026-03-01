# Generic Virtiofs Implementation Plan — Phase 2

**Goal:** Make the `FileSystem` trait object-safe, update `PassthroughFs` and `Server` to the new signatures, and publicly export the trait for external implementors.

**Architecture:** Replace associated types with shared newtypes, change generic method parameters to trait objects, replace raw DAX params with `&dyn DaxMapper`, remove macOS-specific params. Update all callers atomically so the crate compiles. This phase combines design phases 2 and 3 because trait signature changes and implementation updates must happen together.

**Tech Stack:** Rust (trait objects, newtypes, dyn dispatch)

**Scope:** 6 phases from original design (phase 2 of 6, covers design phases 2+3)

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase implements and tests:

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

---

**NOTE: Tasks 1–4 form an atomic change. The crate will not compile until all four are applied. Task 5 adds exports, Task 6 verifies.**

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Define Inode/Handle newtypes and ZeroCopy supertrait changes

**Files:**
- Modify: `src/devices/src/virtio/fs/filesystem.rs:1-28` (imports), `src/devices/src/virtio/fs/filesystem.rs:127-306` (ZeroCopy traits), `src/devices/src/virtio/fs/filesystem.rs:350-378` (trait header + associated types)

**Implementation:**

**1a. Add Inode and Handle newtypes** before the `Entry` struct (around line 29):

```rust
/// Newtype wrapper for filesystem inode numbers.
/// Prevents accidental mix-ups with raw u64 values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Inode(pub u64);

impl From<u64> for Inode {
    fn from(val: u64) -> Self {
        Inode(val)
    }
}

impl From<Inode> for u64 {
    fn from(val: Inode) -> Self {
        val.0
    }
}

/// Newtype wrapper for filesystem handle numbers.
/// Prevents accidental mix-ups with raw u64 values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Handle(pub u64);

impl From<u64> for Handle {
    fn from(val: u64) -> Self {
        Handle(val)
    }
}

impl From<Handle> for u64 {
    fn from(val: Handle) -> Self {
        val.0
    }
}
```

**1b. Add supertraits to ZeroCopyReader and ZeroCopyWriter:**

Change `pub trait ZeroCopyReader {` (line 127) to:
```rust
pub trait ZeroCopyReader: io::Read {
```

Change `pub trait ZeroCopyWriter {` (line 217) to:
```rust
pub trait ZeroCopyWriter: io::Write {
```

Note: The existing blanket impls `impl<R: ZeroCopyReader> ZeroCopyReader for &mut R` (line 203) and `impl<W: ZeroCopyWriter> ZeroCopyWriter for &mut W` (line 296) will continue to work because `io::Read` and `io::Write` already have blanket impls for `&mut R where R: Read` and `&mut W where W: Write`. Verify this compiles — if not, add explicit `io::Read`/`io::Write` impls for the `&mut` wrapper.

**1c. Remove the macOS imports** at the top of filesystem.rs (lines 5-8):
```rust
// DELETE these lines:
// #[cfg(target_os = "macos")]
// use crossbeam_channel::Sender;
// #[cfg(target_os = "macos")]
// use utils::worker_message::WorkerMessage;
```

**1d. Add DaxMapper import:**
```rust
use super::dax_mapper::DaxMapper;
```

**Verification:** N/A — file won't compile until Task 2 completes the trait changes.

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Make FileSystem trait object-safe

**Verifies:** generic-virtiofs.AC4.1, generic-virtiofs.AC4.2, generic-virtiofs.AC4.3

**Files:**
- Modify: `src/devices/src/virtio/fs/filesystem.rs:350-1240` (FileSystem trait)

**Implementation:**

Remove the associated types from the trait definition (delete lines 375-378):
```rust
// DELETE:
// type Inode: From<u64> + Into<u64>;
// type Handle: From<u64> + Into<u64>;
```

Then apply these systematic changes throughout the trait:

**Mechanical replacements (all ~50 methods):**
- Every `Self::Inode` → `Inode`
- Every `Self::Handle` → `Handle`
- Every `inode: Self::Inode` → `inode: Inode`
- Every `handle: Self::Handle` → `handle: Handle`
- Every `Option<Self::Handle>` → `Option<Handle>`
- Every `Vec<(Self::Inode, u64)>` → `Vec<(Inode, u64)>`
- Every `io::Result<(Option<Self::Handle>, OpenOptions)>` → `io::Result<(Option<Handle>, OpenOptions)>`
- Every `io::Result<(Entry, Option<Self::Handle>, OpenOptions)>` → `io::Result<(Entry, Option<Handle>, OpenOptions)>`

**Non-mechanical changes (4 methods):**

Change `read` (line 703) from:
```rust
fn read<W: io::Write + ZeroCopyWriter>(
    &self, ctx: Context, inode: Self::Inode, handle: Self::Handle,
    w: W, size: u32, offset: u64, lock_owner: Option<u64>, flags: u32,
) -> io::Result<usize>
```
to:
```rust
fn read(
    &self, ctx: Context, inode: Inode, handle: Handle,
    w: &mut dyn ZeroCopyWriter, size: u32, offset: u64, lock_owner: Option<u64>, flags: u32,
) -> io::Result<usize>
```

Change `write` (line 737) from:
```rust
fn write<R: io::Read + ZeroCopyReader>(
    &self, ctx: Context, inode: Self::Inode, handle: Self::Handle,
    r: R, size: u32, offset: u64, lock_owner: Option<u64>,
    delayed_write: bool, kill_priv: bool, flags: u32,
) -> io::Result<usize>
```
to:
```rust
fn write(
    &self, ctx: Context, inode: Inode, handle: Handle,
    r: &mut dyn ZeroCopyReader, size: u32, offset: u64, lock_owner: Option<u64>,
    delayed_write: bool, kill_priv: bool, flags: u32,
) -> io::Result<usize>
```

Change `readdir` (line 994) from:
```rust
fn readdir<F>(&self, ctx: Context, inode: Self::Inode, handle: Self::Handle,
    size: u32, offset: u64, add_entry: F) -> io::Result<()>
where F: FnMut(DirEntry) -> io::Result<usize>
```
to:
```rust
fn readdir(&self, ctx: Context, inode: Inode, handle: Handle,
    size: u32, offset: u64, add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
) -> io::Result<()>
```

Change `readdirplus` (line 1033) from:
```rust
fn readdirplus<F>(&self, ctx: Context, inode: Self::Inode, handle: Self::Handle,
    size: u32, offset: u64, add_entry: F) -> io::Result<()>
where F: FnMut(DirEntry, Entry) -> io::Result<usize>
```
to:
```rust
fn readdirplus(&self, ctx: Context, inode: Inode, handle: Handle,
    size: u32, offset: u64, add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
) -> io::Result<()>
```

Change `setupmapping` (line 1135) from:
```rust
fn setupmapping(
    &self, _ctx: Context, inode: Self::Inode, handle: Self::Handle,
    foffset: u64, len: u64, flags: u64, moffset: u64,
    host_shm_base: u64, shm_size: u64,
    #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
) -> io::Result<()>
```
to:
```rust
fn setupmapping(
    &self, _ctx: Context, inode: Inode, handle: Handle,
    foffset: u64, len: u64, flags: u64, moffset: u64,
    mapper: &dyn DaxMapper,
) -> io::Result<()>
```

Change `removemapping` (line 1151) from:
```rust
fn removemapping(
    &self, _ctx: Context, requests: Vec<RemovemappingOne>,
    host_shm_base: u64, shm_size: u64,
    #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
) -> io::Result<()>
```
to:
```rust
fn removemapping(
    &self, _ctx: Context, requests: Vec<RemovemappingOne>,
    mapper: &dyn DaxMapper,
) -> io::Result<()>
```

Also update `batch_forget`'s default implementation which calls `self.forget` — the types will match after the mechanical replacement.

**Add `set_export_table` method** to the trait with a default no-op. This is needed for the GPU feature's export table flow (Phase 4 will delegate through `Fs` to the backend):
```rust
fn set_export_table(&mut self, _export_table: ExportTable) -> u64 {
    0 // no-op for backends that don't support export tables
}
```

**Verification:** N/A — crate won't compile until PassthroughFs and Server are updated.

<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-4) -->

<!-- START_TASK_3 -->
### Task 3: Update PassthroughFs to new trait signatures

**Verifies:** generic-virtiofs.AC5.1, generic-virtiofs.AC5.2, generic-virtiofs.AC5.3, generic-virtiofs.AC5.4

**Files:**
- Modify: `src/devices/src/virtio/fs/linux/passthrough.rs`

**Implementation:**

**3a. Remove associated type aliases** (lines 42-43):
```rust
// DELETE:
// type Inode = u64;
// type Handle = u64;
```

**3b. Add DaxMapper import:**
```rust
use super::super::dax_mapper::DaxMapper;
```

**3c. Mechanical replacements throughout the `impl FileSystem for PassthroughFs` block:**

PassthroughFs already uses `Inode` and `Handle` in method signatures (they were type aliases for `u64`). After removing the associated types, `Inode` and `Handle` now refer to the newtypes from `filesystem.rs`. Update usage throughout:

- Where PassthroughFs compares inodes (e.g., `inode == self.init_inode`), change to `inode == Inode(self.init_inode)` or store `init_inode` as `Inode` type.
- Where PassthroughFs uses inode/handle as hash keys or passes to internal methods that take `u64`, extract the inner value with `.0` or `.into()`.
- The `InodeData` multikey map currently uses `u64` keys — adapt lookups to use `Inode(val)` or extract with `.0`.

**3d. Update read signature** (line 1318):
```rust
// FROM:
fn read<W: io::Write + ZeroCopyWriter>(
    &self, _ctx: Context, inode: Inode, handle: Handle,
    mut w: W, ...
// TO:
fn read(
    &self, _ctx: Context, inode: Inode, handle: Handle,
    w: &mut dyn ZeroCopyWriter, ...
```
The body uses `w.write(...)` and `w.write_from(...)` — both work via the `ZeroCopyWriter: io::Write` supertrait. Remove the `mut w: W` binding, use `w: &mut dyn ZeroCopyWriter` directly.

**3e. Update write signature** (line 1355):
```rust
// FROM:
fn write<R: io::Read + ZeroCopyReader>(
    &self, _ctx: Context, inode: Inode, handle: Handle,
    mut r: R, ...
// TO:
fn write(
    &self, _ctx: Context, inode: Inode, handle: Handle,
    r: &mut dyn ZeroCopyReader, ...
```
The body uses `r.read_exact_to(...)` — works through `ZeroCopyReader`. Remove `mut r: R`, use `r: &mut dyn ZeroCopyReader`.

**3f. Update readdir signature** (line 1177):
```rust
// FROM:
fn readdir<F>(&self, _ctx: Context, inode: Inode, handle: Handle,
    size: u32, offset: u64, add_entry: F) -> io::Result<()>
where F: FnMut(DirEntry) -> io::Result<usize>
// TO:
fn readdir(&self, _ctx: Context, inode: Inode, handle: Handle,
    size: u32, offset: u64, add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
) -> io::Result<()>
```
The `do_readdir` helper must also change its generic `F` parameter to `&mut dyn FnMut(DirEntry) -> io::Result<usize>`.

**3g. Update readdirplus signature** (line 1192):
```rust
// FROM:
fn readdirplus<F>(&self, _ctx: Context, inode: Inode, handle: Handle,
    size: u32, offset: u64, mut add_entry: F) -> io::Result<()>
where F: FnMut(DirEntry, Entry) -> io::Result<usize>
// TO:
fn readdirplus(&self, _ctx: Context, inode: Inode, handle: Handle,
    size: u32, offset: u64, add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
) -> io::Result<()>
```
The body wraps the closure to call `do_readdir` — adapt the wrapper closure and call.

**3h. Rewrite setupmapping** (lines 2151-2231) to use DaxMapper:
```rust
fn setupmapping(
    &self,
    _ctx: Context,
    inode: Inode,
    _handle: Handle,
    foffset: u64,
    len: u64,
    flags: u64,
    moffset: u64,
    mapper: &dyn DaxMapper,
) -> io::Result<()> {
    let writable = (flags & fuse::SetupmappingFlags::WRITE.bits()) != 0;

    if inode == Inode(self.init_inode) {
        let to_copy = std::cmp::min(len as usize, INIT_BINARY.len());
        mapper.map_data(moffset, &INIT_BINARY[..to_copy])?;
        return Ok(());
    }

    let open_flags = if writable { libc::O_RDWR } else { libc::O_RDONLY };
    let file = self.open_inode(inode, open_flags)?;
    mapper.map_file(moffset, len, file.as_raw_fd(), foffset, writable)?;
    Ok(())
}
```

**3i. Rewrite removemapping** (lines 2233-2262) to use DaxMapper:
```rust
fn removemapping(
    &self,
    _ctx: Context,
    requests: Vec<fuse::RemovemappingOne>,
    mapper: &dyn DaxMapper,
) -> io::Result<()> {
    for req in requests {
        mapper.unmap(req.moffset, req.len)?;
    }
    Ok(())
}
```

Note: `open_inode` is a private helper on `PassthroughFs` that takes an inode as `u64` internally. You'll need to adapt calls: `self.open_inode(inode, ...)` may need `self.open_inode(inode.0, ...)` depending on the internal signature. Check and update `open_inode` and other internal helpers that take raw `u64` inode parameters.

**3j. Implement `set_export_table`** on PassthroughFs. Move the logic that was previously on `Fs::set_export_table` (device.rs:99-106) into PassthroughFs:
```rust
fn set_export_table(&mut self, export_table: ExportTable) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static FS_UNIQUE_ID: AtomicU64 = AtomicU64::new(0);

    self.cfg.export_fsid = FS_UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
    self.cfg.export_table = Some(export_table);
    self.cfg.export_fsid
}
```
This requires `PassthroughFs` to have `&mut self` access to its config. Check that `self.cfg` (or however the config is stored) is accessible. The `PassthroughFs` struct stores its config — verify the field name and adjust accordingly.

**Verification:** N/A — crate won't compile until Server is updated.

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Update Server dispatch to new FileSystem signatures

**Files:**
- Modify: `src/devices/src/virtio/fs/server.rs`

**Implementation:**

**4a. Remove macOS imports** (lines 5-8):
```rust
// DELETE:
// #[cfg(target_os = "macos")]
// use crossbeam_channel::Sender;
// #[cfg(target_os = "macos")]
// use utils::worker_message::WorkerMessage;
```

**4b. Add imports for newtypes and DaxMapper:**
```rust
use super::dax_mapper::LinuxDaxMapper;
use super::filesystem::{Inode, Handle};
```
(Add `Inode` and `Handle` to the existing import from `super::filesystem`)

**4c. Update `handle_message`** (line 82):
Remove the `#[cfg(target_os = "macos")] map_sender` parameter.

**4d. Update SETUPMAPPING dispatch** (around lines 146-160):
Replace the current dispatch that passes `shm_base_addr` and `shm.size` with:
```rust
x if (x == Opcode::SetupMapping as u32) && shm_region.is_some() => {
    let shm = shm_region.as_ref().unwrap();
    let mapper = LinuxDaxMapper::new(shm.host_addr, shm.size as u64);
    self.setupmapping(in_header, r, w, &mapper)
}
```

**4e. Update REMOVEMAPPING dispatch** similarly:
```rust
x if (x == Opcode::RemoveMapping as u32) && shm_region.is_some() => {
    let shm = shm_region.as_ref().unwrap();
    let mapper = LinuxDaxMapper::new(shm.host_addr, shm.size as u64);
    self.removemapping(in_header, r, w, &mapper)
}
```

**4f. Update Server::setupmapping method** (line 1393):
Change signature from:
```rust
fn setupmapping(&self, in_header: InHeader, mut r: Reader, w: Writer,
    host_shm_base: u64, shm_size: u64,
    #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
) -> Result<usize>
```
to:
```rust
fn setupmapping(&self, in_header: InHeader, mut r: Reader, w: Writer,
    mapper: &dyn DaxMapper,
) -> Result<usize>
```
Update the body to pass `mapper` to `self.fs.setupmapping(...)` instead of `host_shm_base, shm_size`.

**4g. Update Server::removemapping method** (line 1428):
Same pattern — replace `host_shm_base, shm_size, map_sender` with `mapper: &dyn DaxMapper`, pass through.

**4h. Update Server::read** (line 540):
Change from passing `data_writer` by value to `&mut data_writer`:
```rust
let mut data_writer = ZCWriter(w.split_at(size_of::<OutHeader>()).unwrap());
match self.fs.read(
    Context::from(in_header),
    in_header.nodeid.into(),
    fh.into(),
    &mut data_writer,  // was: data_writer
    ...
```

**4i. Update Server::write** (line 594):
Same pattern — `&mut data_reader` instead of `data_reader`:
```rust
let mut data_reader = ZCReader(r);
match self.fs.write(
    Context::from(in_header),
    in_header.nodeid.into(),
    fh.into(),
    &mut data_reader,  // was: data_reader
    ...
```

**4j. Update Server::do_readdir** (line 957):
Change closure passing from generic to `&mut dyn FnMut`:
```rust
let res = if plus {
    self.fs.readdirplus(
        Context::from(in_header),
        in_header.nodeid.into(),
        fh.into(),
        size,
        offset,
        &mut |d, e| add_dirent(&mut cursor, size, d, Some(e)),
    )
} else {
    self.fs.readdir(
        Context::from(in_header),
        in_header.nodeid.into(),
        fh.into(),
        size,
        offset,
        &mut |d| add_dirent(&mut cursor, size, d, None),
    )
};
```

**4k. Update all other `handle_message` dispatch calls** that use `.into()` for Inode/Handle:
These should continue to work since `u64::into()` returns `Inode` or `Handle` via the `From` impls. Verify that `in_header.nodeid.into()` produces `Inode` and `fh.into()` produces `Handle` at each call site (the compiler will enforce this).

**4l. Remove all remaining `#[cfg(target_os = "macos")]` blocks** in server.rs that reference `map_sender`.

**Verification:** N/A — verify with Task 6.

<!-- END_TASK_4 -->

<!-- END_SUBCOMPONENT_B -->

<!-- START_TASK_5 -->
### Task 5: Make filesystem module public and add exports

**Verifies:** generic-virtiofs.AC3.1, generic-virtiofs.AC3.2

**Files:**
- Modify: `src/devices/src/virtio/fs/mod.rs:1-28`

**Implementation:**

Change `mod filesystem;` (line 2, currently has `#[allow(dead_code)]`) to:
```rust
pub mod filesystem;
```
Remove the `#[allow(dead_code)]` attribute.

Add re-exports after the existing `pub use` lines (after line 28):

```rust
pub use self::dax_mapper::DaxMapper;
pub use self::filesystem::{
    Context, DirEntry, Entry, Extensions, FileSystem, GetxattrReply, Handle, Inode,
    ListxattrReply, SecContext, ZeroCopyReader, ZeroCopyWriter,
};
```

The `FsOptions`, `FileLock`, `OpenOptions`, `RemovemappingOne`, `SetattrValid` types are already re-exported from `filesystem.rs` via `pub use fuse::*` imports, and `fuse` is already a public module. Verify they are accessible.

The `bindings` module (`stat64`, `statvfs64`, `ino64_t`) is already accessible via `super::bindings` — ensure it's public. Check `src/devices/src/virtio/mod.rs` for the bindings module visibility.

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(virtiofs): make FileSystem trait object-safe and publicly exported`

<!-- END_TASK_5 -->

<!-- START_TASK_6 -->
### Task 6: Verify Box<dyn FileSystem> compiles

**Verifies:** generic-virtiofs.AC4.1

**Files:**
- No file changes (verification only)

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles cleanly

Additionally verify object safety by checking that `Box<dyn FileSystem>` is accepted. The executor should confirm this compiles — if it doesn't, there are remaining object-safety barriers to fix (e.g., methods with `Self` in return position, or remaining generic parameters).

Run: `cargo test -p devices` (if any unit tests exist in the devices crate)
Expected: All tests pass

**Commit:** If any fixups were needed, commit as `fix(virtiofs): resolve remaining object-safety issues`

<!-- END_TASK_6 -->
