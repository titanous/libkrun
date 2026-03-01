# Generic Virtiofs Implementation Plan — Phase 1

**Goal:** Introduce the `DaxMapper` trait abstraction and its Linux implementation (`LinuxDaxMapper`).

**Architecture:** A new `DaxMapper` trait provides `map_file`, `map_data`, and `unmap` operations that abstract platform-specific DAX window manipulation. `LinuxDaxMapper` implements these using `mmap(MAP_FIXED)` with bounds checking. This decouples `FileSystem` implementations from direct `libc::mmap` calls.

**Tech Stack:** Rust, libc crate (mmap, MAP_FIXED, PROT_NONE, etc.)

**Scope:** 6 phases from original design (phase 1 of 6, covers design phase 1)

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase implements and tests:

### generic-virtiofs.AC1: DaxMapper trait abstracts DAX operations
- **generic-virtiofs.AC1.1 Success:** `map_file` maps a file region into the DAX window at the specified offset
- **generic-virtiofs.AC1.2 Success:** `map_data` maps anonymous memory with provided data at the specified offset
- **generic-virtiofs.AC1.3 Success:** `unmap` replaces a DAX range with inaccessible pages
- **generic-virtiofs.AC1.4 Failure:** `map_file` rejects mapping that would exceed DAX window bounds
- **generic-virtiofs.AC1.5 Failure:** `unmap` rejects range that would exceed DAX window bounds

---

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->

<!-- START_TASK_1 -->
### Task 1: Create DaxMapper trait and LinuxDaxMapper implementation

**Verifies:** generic-virtiofs.AC1.1, generic-virtiofs.AC1.2, generic-virtiofs.AC1.3

**Files:**
- Create: `src/devices/src/virtio/fs/dax_mapper.rs`

**Implementation:**

Create the file with the `DaxMapper` trait and `LinuxDaxMapper` struct. The trait must be object-safe (`Send + Sync`) and define three operations. `LinuxDaxMapper` stores the host base address and window size, performs bounds checking on every call, then delegates to `libc::mmap`.

The three mmap patterns (extracted from the existing `linux/passthrough.rs:2184-2258`):

1. **`map_file`** — maps a file region: `mmap(host_addr + dax_offset, len, prot_flags, MAP_SHARED | MAP_FIXED, fd, file_offset)` where `prot_flags` is `PROT_READ | PROT_WRITE` if writable, else `PROT_READ`.

2. **`map_data`** — maps anonymous memory with data: `mmap(host_addr + dax_offset, data.len(), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0)` then `memcpy` the data in.

3. **`unmap`** — replaces region with inaccessible pages: `mmap(host_addr + dax_offset, len, PROT_NONE, MAP_ANONYMOUS | MAP_PRIVATE | MAP_FIXED, -1, 0)`.

All three must check `dax_offset + len <= self.size` before proceeding (for `map_data`, `len` is `data.len() as u64`). On bounds violation, return `io::Error::from_raw_os_error(libc::EINVAL)`. On mmap failure (returns `MAP_FAILED`), return `io::Error::last_os_error()`.

Note: The `as u64` cast from `usize` in `map_data` is lossless on 64-bit platforms. libkrun targets 64-bit architectures only (x86_64, aarch64, riscv64).

```rust
use std::io;
use std::os::unix::io::RawFd;

/// Abstracts platform-specific DAX window operations.
///
/// FileSystem implementations call DaxMapper methods instead of
/// libc::mmap directly. This enables different DAX backends
/// (Linux mmap, HVF mapping messages, etc.) without changing the
/// FileSystem trait.
pub trait DaxMapper: Send + Sync {
    fn map_file(
        &self,
        dax_offset: u64,
        len: u64,
        fd: RawFd,
        file_offset: u64,
        writable: bool,
    ) -> io::Result<()>;

    fn map_data(&self, dax_offset: u64, data: &[u8]) -> io::Result<()>;

    fn unmap(&self, dax_offset: u64, len: u64) -> io::Result<()>;
}

/// Linux implementation of DaxMapper using mmap(MAP_FIXED).
///
/// Created from a VirtioShmRegion's host_addr and size. Performs
/// bounds checking before every mmap call.
pub(crate) struct LinuxDaxMapper {
    host_addr: u64,
    size: u64,
}

impl LinuxDaxMapper {
    pub fn new(host_addr: u64, size: u64) -> Self {
        Self { host_addr, size }
    }

    fn check_bounds(&self, offset: u64, len: u64) -> io::Result<()> {
        if offset.checked_add(len).map_or(true, |end| end > self.size) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        Ok(())
    }
}

impl DaxMapper for LinuxDaxMapper {
    fn map_file(
        &self,
        dax_offset: u64,
        len: u64,
        fd: RawFd,
        file_offset: u64,
        writable: bool,
    ) -> io::Result<()> {
        self.check_bounds(dax_offset, len)?;

        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        let addr = self.host_addr + dax_offset;

        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                len as usize,
                prot,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                file_offset as libc::off_t,
            )
        };
        if std::ptr::eq(ret, libc::MAP_FAILED) {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn map_data(&self, dax_offset: u64, data: &[u8]) -> io::Result<()> {
        let len = data.len() as u64;
        self.check_bounds(dax_offset, len)?;

        let addr = self.host_addr + dax_offset;

        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                data.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if std::ptr::eq(ret, libc::MAP_FAILED) {
            return Err(io::Error::last_os_error());
        }

        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len());
        }
        Ok(())
    }

    fn unmap(&self, dax_offset: u64, len: u64) -> io::Result<()> {
        self.check_bounds(dax_offset, len)?;

        let addr = self.host_addr + dax_offset;

        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                len as usize,
                libc::PROT_NONE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if std::ptr::eq(ret, libc::MAP_FAILED) {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
```

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles without errors

**Commit:** `feat(virtiofs): add DaxMapper trait and LinuxDaxMapper implementation`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add unit tests for DaxMapper bounds checking

**Verifies:** generic-virtiofs.AC1.4, generic-virtiofs.AC1.5

**Files:**
- Modify: `src/devices/src/virtio/fs/dax_mapper.rs` (append `#[cfg(test)] mod tests` block)

**Implementation:**

Add a `#[cfg(test)] mod tests` block at the bottom of `dax_mapper.rs` (following the project convention of inline test modules). Tests verify bounds checking logic on `LinuxDaxMapper`.

Tests needed:
- **generic-virtiofs.AC1.4:** `map_file` with `dax_offset + len > size` returns `EINVAL`. Test cases: offset at boundary, offset that causes overflow, zero-size window.
- **generic-virtiofs.AC1.5:** `unmap` with `dax_offset + len > size` returns `EINVAL`. Same boundary test patterns.
- Also test `map_data` bounds checking for completeness (data longer than remaining window).
- Verify that in-bounds offsets pass the bounds check (use offset 0, len == size).

Note: These tests only verify the bounds-checking error path. They use a `host_addr` of 0 and deliberately trigger the bounds check before any mmap syscall is reached, so no real memory mapping occurs. The success-path mmap behavior is verified by integration tests in Phase 7.

**Verification:**
Run: `cargo test -p devices -- dax_mapper`
Expected: All bounds-checking tests pass

**Commit:** `test(virtiofs): add DaxMapper bounds-checking unit tests`

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Register dax_mapper module and export DaxMapper trait

**Files:**
- Modify: `src/devices/src/virtio/fs/mod.rs:1-28` (add module declaration and re-export)

**Implementation:**

Add `pub mod dax_mapper;` to the module declarations in `mod.rs` and re-export the `DaxMapper` trait.

In `src/devices/src/virtio/fs/mod.rs`, add after the existing module declarations (after line 8 `mod worker;`):

```rust
pub mod dax_mapper;
```

And add a re-export near the existing `pub use` lines (after line 28):

```rust
pub use self::dax_mapper::DaxMapper;
```

**Verification:**
Run: `cargo check -p devices`
Expected: Compiles, `DaxMapper` is accessible as `devices::virtio::fs::DaxMapper`

**Commit:** `feat(virtiofs): export DaxMapper trait from fs module`

<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->
