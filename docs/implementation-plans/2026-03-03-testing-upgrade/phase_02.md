# Phase 2: Pure Logic Extraction

## Overview

**Goal:** Separate pure Rust logic from syscall-dependent code to enable Miri and loom testing in Phase 3.

**Design reference:** `docs/design-plans/2026-03-03-testing-upgrade.md` — `<!-- START_PHASE_2 -->`

**Acceptance criteria addressed (prerequisites established for Phase 3):**
- testing-upgrade.AC2.3: `just loom` runs on `DirtyBitmap`, `ReclaimedBitmap`, `PageTracker` — loom shims added here
- testing-upgrade.AC2.5: `just proptest` on bitmap invariants, address translation — extracted modules enable this

**Done when:** `just test` passes (refactoring is behavior-preserving). Extracted modules compile independently of their syscall-dependent siblings.

**Dependencies:** Phase 1 complete (justfile exists; `just test` target available).

---

## Investigation Findings

| File | Lines | Pure logic | Syscall-dependent |
|------|-------|------------|-------------------|
| `src/vmm/src/uffd.rs` | 1822 | `PageTracker`, `PageTrackerStats`, `LoadSource`, `guest_to_host`, `guest_addr_to_page_index`, `host_to_guest`, `is_eexist`, `UffdRegion` | `UffdHandler`, `preload_task`, `fault_loop`, `signal_error` |
| `src/devices/src/virtio/block/async_worker.rs` | 3339 | `AsyncWorkerMetrics`, `RequestError`, `RequestHeader`, `DiscardWriteData`, `Request` enum, `RequestResult`, `ParsedRequest`, `QueuedWrite`, `BatchWriteResult` (lines 22–143) | `AsyncBlockWorker` and all tokio async code |
| `src/devices/src/virtio/fs/server.rs` | 1660 | Length validation (line 87–92), opcode routing (lines 95–156) | All individual handler methods (`lookup`, `getattr`, etc.) — delegate to `FileSystem` trait |
| `src/vmm/src/dirty_bitmap.rs` | 230 | Entire file — `AtomicU64` bitmap ops, no syscalls | — |
| `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` | 308 | Entire file — `AtomicU64` bitmap ops, no syscalls | — |

**Module structure:**
- `src/vmm/src/lib.rs` line 34: `pub mod uffd;` (gated on `#[cfg(all(target_os = "linux", feature = "uffd"))]`) — Rust resolves both `uffd.rs` and `uffd/mod.rs` from this declaration; no change needed
- `src/devices/src/virtio/block/mod.rs`: `pub mod async_worker;` (line 4) — add `pub mod request;` here
- `src/devices/src/virtio/fs/mod.rs`: `mod server;` (line 7) — add `mod fuse_dispatch;` here
- `Server` struct in server.rs is NOT generic (holds `Box<dyn FileSystem + Send + Sync>`)

**loom research:** Canonical shim uses `#[cfg(not(loom))]`/`#[cfg(loom)]` to redirect atomic imports; configured via `[target.'cfg(loom)'.dependencies]` in Cargo.toml; run with `RUSTFLAGS="--cfg loom" cargo test --release`.

**Miri research:** `GuestMemoryMmap` uses `mmap` syscall — Miri cannot interpret it. Pure bitmap modules (`dirty_bitmap`, `reclaimed_bitmap`, `page_tracker`) will work. Mark `GuestMemoryMmap`-dependent tests with `#[cfg_attr(miri, ignore)]`.

---

## Task 1: Convert `uffd.rs` to `uffd/` subdirectory

**Purpose:** Isolate `PageTracker` (pure atomic bitmap) from `UffdHandler` (uffd syscalls) so Phase 3 can run Miri and loom against `PageTracker` without linker-level uffd dependencies.

### Step 1.1 — Create `src/vmm/src/uffd/page_tracker.rs`

This file contains all pure logic from `uffd.rs`. Copy the following items verbatim from `uffd.rs`, replacing only the atomic import line:

```rust
// src/vmm/src/uffd/page_tracker.rs
// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Pure page tracking logic: bitmap, statistics, and address translation.
//! No syscalls; testable under Miri and loom.

#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::snapshot_store::system_page_size;

/// Represents a guest memory region registered with UFFD.
/// Moved here so address translation functions are co-located with PageTracker.
#[derive(Clone)]
pub struct UffdRegion {
    pub guest_addr: u64,
    pub host_addr: u64,
    pub size: u64,
    pub page_offset: usize,
}

// --- Copy verbatim from uffd.rs lines 34–57 ---
// guest_to_host()
// guest_addr_to_page_index()

// --- Copy verbatim from uffd.rs lines 235–244 ---
// host_to_guest()

// --- Copy verbatim from uffd.rs lines 428–435 ---
// is_eexist() — keep as pub(crate) since it references userfaultfd::Error
//   (add `use userfaultfd;` import if needed, guarded by cfg(feature = "uffd"))

// --- Copy verbatim from uffd.rs lines 446–595 ---
// LoadSource enum
// PageTrackerStats struct
// PageTracker struct + impl block
```

**Key change:** Replace the single atomic import at the top of uffd.rs:
```rust
// OLD (in uffd.rs, move this to page_tracker.rs with loom shim):
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
```

With the `#[cfg(not(loom))]` / `#[cfg(loom)]` pair shown above.

**Note on `is_eexist`:** It references `userfaultfd::Error` and `libc::EEXIST`. Keep its `use userfaultfd;` import in page_tracker.rs but it remains part of pure logic since it only pattern-matches on an error enum (no syscalls). Add `use libc;` import if needed.

**Note on `UffdRegion` visibility:** `UffdRegion` was `struct` (private) in uffd.rs. Make it `pub struct` in page_tracker.rs — it needs to be accessible from handler.rs.

### Step 1.2 — Create `src/vmm/src/uffd/handler.rs`

This file contains all syscall-dependent code from `uffd.rs`:

```rust
// src/vmm/src/uffd/handler.rs
// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! UffdHandler: registers guest memory with userfaultfd and resolves page faults.

// Preserve all existing imports from uffd.rs EXCEPT the atomic ones
// (those are now in page_tracker.rs).
use super::page_tracker::{
    guest_addr_to_page_index, guest_to_host, host_to_guest, is_eexist,
    LoadSource, PageTracker, UffdRegion,
};

// Copy verbatim from uffd.rs:
//   UffdFd newtype (lines 420–426)
//   signal_error() (lines 437–444)
//   UffdHandler struct (lines 67–80)
//   UffdHandler impl block with new(), run(), and all private methods
//   preload_task() async fn
//   fault_loop() async fn
//   All remaining imports from the top of uffd.rs
```

### Step 1.3 — Create `src/vmm/src/uffd/mod.rs`

```rust
// src/vmm/src/uffd/mod.rs
// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

mod handler;
mod page_tracker;

// Re-export the same public surface that uffd.rs exported:
pub use handler::UffdHandler;
pub use page_tracker::{
    guest_addr_to_page_index, guest_to_host, host_to_guest, is_eexist,
    LoadSource, PageTracker, PageTrackerStats, UffdRegion,
};
```

### Step 1.4 — Delete `src/vmm/src/uffd.rs`

No changes to `src/vmm/src/lib.rs` — Rust automatically resolves `mod uffd;` to `uffd/mod.rs` once `uffd.rs` is gone.

### Step 1.5 — Verify

```bash
cargo build -p vmm --features uffd,snapshot
```

---

## Task 2: Extract block request types to `request.rs`

**Purpose:** Isolate `RequestHeader`, `DiscardWriteData`, and the other pure data structures from the 3339-line `async_worker.rs` so they can be tested under Miri and with proptest in Phase 3.

### Step 2.1 — Create `src/devices/src/virtio/block/request.rs`

Move lines 22–143 from `async_worker.rs` into this new file. The key change is updating visibility: types that were private to `async_worker.rs` become `pub(super)` (visible within the `block` module), and `pub` types stay `pub`.

```rust
// src/devices/src/virtio/block/request.rs
// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Pure VIRTIO block request data structures: headers, parsed requests, metrics.
//! No I/O; testable under Miri and with proptest.

#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};

use vm_memory::ByteValued;

use super::{AsyncBlockBackend, VolatileSliceGuard};

/// Metrics for the async block worker.
/// NOTE: `#[derive(Default)]` dropped; provide manual impl for loom compat.
pub struct AsyncWorkerMetrics {
    pub in_flight: AtomicU64,
    pub reads: AtomicU64,
    pub writes: AtomicU64,
    pub flushes: AtomicU64,
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
    pub read_latency_us: AtomicU64,
    pub write_latency_us: AtomicU64,
    pub concurrent_reads: AtomicU64,
    pub peak_concurrent_reads: AtomicU64,
}

impl Default for AsyncWorkerMetrics {
    fn default() -> Self {
        AsyncWorkerMetrics {
            in_flight: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            read_latency_us: AtomicU64::new(0),
            write_latency_us: AtomicU64::new(0),
            concurrent_reads: AtomicU64::new(0),
            peak_concurrent_reads: AtomicU64::new(0),
        }
    }
}

// Copy verbatim from async_worker.rs lines 47–58 (RequestError enum — keep pub)
// Copy verbatim from async_worker.rs lines 60–79 (RequestHeader, DiscardWriteData — keep pub)
// Copy verbatim from async_worker.rs lines 81–143 (Request, RequestResult, ParsedRequest,
//   QueuedWrite, BatchWriteResult — change from private to pub(super))
```

**Visibility changes summary:**

| Type | Before (async_worker.rs) | After (request.rs) |
|------|--------------------------|--------------------|
| `AsyncWorkerMetrics` | `pub struct` | `pub struct` |
| `RequestError` | `pub enum` | `pub enum` |
| `RequestHeader` | `pub struct` | `pub struct` |
| `DiscardWriteData` | `pub struct` | `pub struct` |
| `Request` | `enum` (private) | `pub(super) enum` |
| `RequestResult` | `struct` (private) | `pub(super) struct` |
| `ParsedRequest` | `struct` (private) | `pub(super) struct` |
| `QueuedWrite` | `struct` (private) | `pub(super) struct` |
| `BatchWriteResult` | `struct` (private) | `pub(super) struct` |

### Step 2.2 — Add module declaration to `src/devices/src/virtio/block/mod.rs`

```rust
// In block/mod.rs, add after existing mod declarations (e.g., after line 4):
pub mod request;
```

### Step 2.3 — Update `async_worker.rs` imports

At the top of `async_worker.rs`, replace the struct definitions (lines 22–143) with imports from the new module:

```rust
// In async_worker.rs, replace lines 22–143 with:
use super::request::{
    AsyncWorkerMetrics, BatchWriteResult, DiscardWriteData, ParsedRequest, QueuedWrite,
    Request, RequestError, RequestHeader, RequestResult,
};
```

Keep all other imports at the top of `async_worker.rs` unchanged.

### Step 2.4 — Verify

```bash
cargo build -p devices --features blk
```

---

## Task 3: Extract FUSE dispatch to `fuse_dispatch.rs`

**Purpose:** Extract the opcode routing and header validation from `Server::handle_message` into a pure, dependency-free module so Phase 3 can test the routing logic under Miri without constructing a full `Server`.

**Design approach:** `fuse_dispatch.rs` contains pure free functions only — no `impl Server`. The `Server::handle_message` in server.rs calls these helpers. This avoids Rust module visibility issues (impl blocks for a type defined in a sibling module) while achieving the testability goal.

### Step 3.1 — Create `src/devices/src/virtio/fs/fuse_dispatch.rs`

```rust
// src/devices/src/virtio/fs/fuse_dispatch.rs
// Copyright 2019 The Chromium OS Authors. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

//! Pure FUSE protocol routing: header length validation and opcode classification.
//! No FileSystem interaction; no I/O; testable under Miri.

use super::fuse::Opcode;

pub(super) const MAX_BUFFER_SIZE: u32 = 1 << 20;
pub(super) const BUFFER_HEADER_SIZE: u32 = 0x1000;

/// Validate that a FUSE message length is within the allowed limit.
///
/// Returns `true` if the message is within bounds and can be processed.
pub(super) fn is_valid_len(len: u32) -> bool {
    len <= MAX_BUFFER_SIZE + BUFFER_HEADER_SIZE
}

/// Map a raw FUSE opcode integer to the typed `Opcode` enum.
///
/// Returns `None` for unknown opcodes; callers should reply with ENOSYS.
pub(super) fn classify_opcode(opcode: u32) -> Option<Opcode> {
    // Each arm maps the typed enum variant to its u32 wire value.
    // Copy match arms from Server::handle_message (server.rs lines 95–156),
    // changing `x if x == Opcode::Foo as u32 => self.foo(...)` to
    //          `x if x == Opcode::Foo as u32 => Some(Opcode::Foo)`.
    match opcode {
        x if x == Opcode::Lookup as u32 => Some(Opcode::Lookup),
        x if x == Opcode::Forget as u32 => Some(Opcode::Forget),
        x if x == Opcode::Getattr as u32 => Some(Opcode::Getattr),
        x if x == Opcode::Setattr as u32 => Some(Opcode::Setattr),
        x if x == Opcode::Readlink as u32 => Some(Opcode::Readlink),
        x if x == Opcode::Symlink as u32 => Some(Opcode::Symlink),
        x if x == Opcode::Mknod as u32 => Some(Opcode::Mknod),
        x if x == Opcode::Mkdir as u32 => Some(Opcode::Mkdir),
        x if x == Opcode::Unlink as u32 => Some(Opcode::Unlink),
        x if x == Opcode::Rmdir as u32 => Some(Opcode::Rmdir),
        x if x == Opcode::Rename as u32 => Some(Opcode::Rename),
        x if x == Opcode::Link as u32 => Some(Opcode::Link),
        x if x == Opcode::Open as u32 => Some(Opcode::Open),
        x if x == Opcode::Read as u32 => Some(Opcode::Read),
        x if x == Opcode::Write as u32 => Some(Opcode::Write),
        x if x == Opcode::Statfs as u32 => Some(Opcode::Statfs),
        x if x == Opcode::Release as u32 => Some(Opcode::Release),
        x if x == Opcode::Fsync as u32 => Some(Opcode::Fsync),
        x if x == Opcode::Setxattr as u32 => Some(Opcode::Setxattr),
        x if x == Opcode::Getxattr as u32 => Some(Opcode::Getxattr),
        x if x == Opcode::Listxattr as u32 => Some(Opcode::Listxattr),
        x if x == Opcode::Removexattr as u32 => Some(Opcode::Removexattr),
        x if x == Opcode::Flush as u32 => Some(Opcode::Flush),
        x if x == Opcode::Init as u32 => Some(Opcode::Init),
        x if x == Opcode::Opendir as u32 => Some(Opcode::Opendir),
        x if x == Opcode::Readdir as u32 => Some(Opcode::Readdir),
        x if x == Opcode::Releasedir as u32 => Some(Opcode::Releasedir),
        x if x == Opcode::Fsyncdir as u32 => Some(Opcode::Fsyncdir),
        x if x == Opcode::Getlk as u32 => Some(Opcode::Getlk),
        x if x == Opcode::Setlk as u32 => Some(Opcode::Setlk),
        x if x == Opcode::Setlkw as u32 => Some(Opcode::Setlkw),
        x if x == Opcode::Access as u32 => Some(Opcode::Access),
        x if x == Opcode::Create as u32 => Some(Opcode::Create),
        x if x == Opcode::Interrupt as u32 => Some(Opcode::Interrupt),
        x if x == Opcode::Bmap as u32 => Some(Opcode::Bmap),
        x if x == Opcode::Destroy as u32 => Some(Opcode::Destroy),
        x if x == Opcode::Ioctl as u32 => Some(Opcode::Ioctl),
        x if x == Opcode::Poll as u32 => Some(Opcode::Poll),
        x if x == Opcode::NotifyReply as u32 => Some(Opcode::NotifyReply),
        x if x == Opcode::BatchForget as u32 => Some(Opcode::BatchForget),
        x if x == Opcode::Fallocate as u32 => Some(Opcode::Fallocate),
        x if x == Opcode::Readdirplus as u32 => Some(Opcode::Readdirplus),
        x if x == Opcode::Rename2 as u32 => Some(Opcode::Rename2),
        x if x == Opcode::Lseek as u32 => Some(Opcode::Lseek),
        x if x == Opcode::CopyFileRange as u32 => Some(Opcode::CopyFileRange),
        x if x == Opcode::SetupMapping as u32 => Some(Opcode::SetupMapping),
        x if x == Opcode::RemoveMapping as u32 => Some(Opcode::RemoveMapping),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_len_boundary() {
        assert!(is_valid_len(0));
        assert!(is_valid_len(MAX_BUFFER_SIZE + BUFFER_HEADER_SIZE));
        assert!(!is_valid_len(MAX_BUFFER_SIZE + BUFFER_HEADER_SIZE + 1));
        assert!(!is_valid_len(u32::MAX));
    }

    #[test]
    fn test_classify_opcode_known() {
        // Opcode::Lookup == 1 per FUSE protocol
        assert_eq!(classify_opcode(Opcode::Lookup as u32), Some(Opcode::Lookup));
        assert_eq!(classify_opcode(Opcode::Init as u32), Some(Opcode::Init));
        assert_eq!(classify_opcode(Opcode::Destroy as u32), Some(Opcode::Destroy));
    }

    #[test]
    fn test_classify_opcode_unknown() {
        assert_eq!(classify_opcode(0), None);
        assert_eq!(classify_opcode(9999), None);
    }
}
```

**Note:** Verify the exact opcode list by reading `server.rs` lines 95–156 — if any opcodes in the dispatch are missing from this list, add them. The list above covers the 38 known opcodes reported by the codebase investigation.

### Step 3.2 — Add module declaration to `src/devices/src/virtio/fs/mod.rs`

```rust
// In fs/mod.rs, add after the existing mod declarations (e.g., after `mod server;` on line 7):
mod fuse_dispatch;
```

### Step 3.3 — Update `Server::handle_message` in `server.rs`

Move the constants and replace the inline length check and match with calls to `fuse_dispatch`:

```rust
// In server.rs:
// 1. Remove the two constant declarations (MAX_BUFFER_SIZE, BUFFER_HEADER_SIZE)
//    from the top of server.rs (lines 28–29) — they are now in fuse_dispatch.rs.

// 2. Replace the body of handle_message (lines ~78–157) with:
pub fn handle_message(
    &self,
    mut r: Reader,
    w: Writer,
    shm_region: &Option<VirtioShmRegion>,
    exit_code: &Arc<AtomicI32>,
) -> Result<usize> {
    let in_header: InHeader = r.read_obj().map_err(Error::DecodeMessage)?;

    if !fuse_dispatch::is_valid_len(in_header.len) {
        return reply_error(
            linux_error(libc::ENOMEM),
            in_header.unique,
            w,
        );
    }

    debug!("opcode: {}", in_header.opcode);
    match fuse_dispatch::classify_opcode(in_header.opcode) {
        Some(Opcode::Lookup) => self.lookup(in_header, r, w),
        Some(Opcode::Forget) => self.forget(in_header, r),
        Some(Opcode::Getattr) => self.getattr(in_header, r, w),
        Some(Opcode::Setattr) => self.setattr(in_header, r, w),
        Some(Opcode::Readlink) => self.readlink(in_header, w),
        Some(Opcode::Symlink) => self.symlink(in_header, r, w),
        Some(Opcode::Mknod) => self.mknod(in_header, r, w),
        Some(Opcode::Mkdir) => self.mkdir(in_header, r, w),
        Some(Opcode::Unlink) => self.unlink(in_header, r, w),
        Some(Opcode::Rmdir) => self.rmdir(in_header, r, w),
        Some(Opcode::Rename) => self.rename(in_header, r, w),
        Some(Opcode::Link) => self.link(in_header, r, w),
        Some(Opcode::Open) => self.open(in_header, r, w),
        Some(Opcode::Read) => self.read(in_header, r, w, shm_region),
        Some(Opcode::Write) => self.write(in_header, r, w, shm_region),
        Some(Opcode::Statfs) => self.statfs(in_header, w),
        Some(Opcode::Release) => self.release(in_header, r, w),
        Some(Opcode::Fsync) => self.fsync(in_header, r, w),
        Some(Opcode::Setxattr) => self.setxattr(in_header, r, w),
        Some(Opcode::Getxattr) => self.getxattr(in_header, r, w),
        Some(Opcode::Listxattr) => self.listxattr(in_header, r, w),
        Some(Opcode::Removexattr) => self.removexattr(in_header, r, w),
        Some(Opcode::Flush) => self.flush(in_header, r, w),
        Some(Opcode::Init) => self.init(in_header, r, w),
        Some(Opcode::Opendir) => self.opendir(in_header, r, w),
        Some(Opcode::Readdir) => self.readdir(in_header, r, w),
        Some(Opcode::Releasedir) => self.releasedir(in_header, r, w),
        Some(Opcode::Fsyncdir) => self.fsyncdir(in_header, r, w),
        Some(Opcode::Getlk) => self.getlk(in_header, r, w),
        Some(Opcode::Setlk) => self.setlk(in_header, r, w),
        Some(Opcode::Setlkw) => self.setlkw(in_header, r, w),
        Some(Opcode::Access) => self.access(in_header, r, w),
        Some(Opcode::Create) => self.create(in_header, r, w),
        Some(Opcode::Interrupt) => self.interrupt(in_header),
        Some(Opcode::Bmap) => self.bmap(in_header, r, w),
        Some(Opcode::Destroy) => self.destroy(),
        Some(Opcode::Ioctl) => self.ioctl(in_header, r, w),
        Some(Opcode::Poll) => self.poll(in_header, r, w),
        Some(Opcode::NotifyReply) => self.notify_reply(in_header, r, w),
        Some(Opcode::BatchForget) => self.batch_forget(in_header, r),
        Some(Opcode::Fallocate) => self.fallocate(in_header, r, w),
        Some(Opcode::Readdirplus) => self.readdirplus(in_header, r, w),
        Some(Opcode::Rename2) => self.rename2(in_header, r, w),
        Some(Opcode::Lseek) => self.lseek(in_header, r, w),
        Some(Opcode::CopyFileRange) => self.copy_file_range(in_header, r, w),
        Some(Opcode::SetupMapping) => self.setupmapping(in_header, r, w, shm_region),
        Some(Opcode::RemoveMapping) => self.removemapping(in_header, r, w, shm_region, exit_code),
        None => reply_error(
            linux_error(libc::ENOSYS),
            in_header.unique,
            w,
        ),
    }
}
```

**Note:** Adapt method signatures for `Some(Opcode::X)` to match the exact signatures already in server.rs. If some handler methods have different parameter sets (e.g., `shm_region` only for mapping ops), copy the original dispatch pattern faithfully.

### Step 3.4 — Verify

```bash
cargo build -p devices --features vhost-user
cargo test -p devices --features vhost-user -- fs
```

---

## Task 4: Add loom shims to existing bitmap files

**Purpose:** Prepare `dirty_bitmap.rs` and `reclaimed_bitmap.rs` for loom concurrency testing in Phase 3.

### Step 4.1 — Update `src/vmm/Cargo.toml`

Add loom as a conditional dependency (compiled only when `--cfg loom` is set):

```toml
# In src/vmm/Cargo.toml, after the [dev-dependencies] section:
[target.'cfg(loom)'.dependencies]
loom = { version = "0.7", features = ["checkpoint"] }
```

### Step 4.2 — Update `src/devices/Cargo.toml`

```toml
# In src/devices/Cargo.toml, add a new section:
[target.'cfg(loom)'.dependencies]
loom = { version = "0.7", features = ["checkpoint"] }
```

### Step 4.3 — Update `src/vmm/src/dirty_bitmap.rs`

Replace the atomic import at line 9:

```rust
// OLD:
use std::sync::atomic::{AtomicU64, Ordering};

// NEW:
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
```

No other changes to dirty_bitmap.rs.

### Step 4.4 — Update `src/devices/src/virtio/balloon/reclaimed_bitmap.rs`

Replace the atomic import at line 8:

```rust
// OLD:
use std::sync::atomic::{AtomicU64, Ordering};

// NEW:
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
```

No other changes to reclaimed_bitmap.rs.

### Step 4.5 — Verify both files still build normally

```bash
cargo build -p vmm
cargo build -p devices --features net
```

**Note on loom test structure:** The existing `#[cfg(test)]` modules in dirty_bitmap.rs and reclaimed_bitmap.rs already have unit tests. In Phase 3, loom-specific tests will be added in new `#[cfg(all(test, loom))]` modules. No loom tests are added here — Phase 2 only adds the import shim.

---

## Task 5: Miri spike — document GuestMemoryMmap compatibility

**Purpose:** Determine whether `vm-memory::GuestMemoryMmap` tests can run under Miri. Result gates which modules get Miri coverage in Phase 3.

### Step 5.1 — Write a canary test in `dirty_bitmap.rs`

Add to the `#[cfg(test)]` block at the bottom of `src/vmm/src/dirty_bitmap.rs`:

```rust
/// Miri spike: verify DirtyBitmap pure atomic ops are Miri-compatible.
#[test]
fn miri_spike_pure_atomics() {
    let bitmap = DirtyBitmap::new(0x0, 4);
    bitmap.mark_dirty(0x0);
    bitmap.mark_dirty(0x1000);
    let pages = bitmap.drain_dirty_pages();
    assert!(!pages.is_empty());
}
```

### Step 5.2 — Run dirty_bitmap under Miri

```bash
cargo +nightly miri test -p vmm -- dirty_bitmap
```

**Expected result:** PASSES. All DirtyBitmap tests pass under Miri because they only use AtomicU64 bitops with no file I/O or mmap.

### Step 5.3 — Run balloon under Miri

```bash
cargo +nightly miri test -p devices --features net -- balloon::reclaimed_bitmap
```

**Expected result:** PASSES for the same reason — pure AtomicU64 ops.

### Step 5.4 — Attempt GuestMemoryMmap under Miri

Write a canary test in any file that already imports `GuestMemoryMmap` (e.g., `src/vmm/src/snapshot.rs` or add a temporary test file):

```rust
#[cfg(test)]
mod miri_spike_tests {
    #[test]
    #[cfg_attr(miri, ignore)]  // Add this after confirming failure
    fn miri_spike_guest_memory() {
        use vm_memory::{GuestAddress, GuestMemoryMmap};
        let regions = vec![(GuestAddress(0), 4096usize)];
        let _mem = GuestMemoryMmap::from_ranges(&regions).unwrap();
        // If Miri reaches here, GuestMemoryMmap is Miri-compatible
    }
}
```

Run:
```bash
cargo +nightly miri test -p vmm -- miri_spike_guest_memory
```

**Expected result:** FAILS with error similar to:
```
error: Miri evaluation error: unsupported Miri functionality: can't call foreign function `mmap64`
```

### Step 5.5 — Document the outcome and mark tests

After confirming the outcome:

1. **Pure bitmap files** (`dirty_bitmap.rs`, `reclaimed_bitmap.rs`, `page_tracker.rs`): Miri-compatible. Phase 3 will run their full test suites under Miri.

2. **`GuestMemoryMmap`-dependent tests**: NOT Miri-compatible. Mark them with `#[cfg_attr(miri, ignore)]` so `cargo miri test` skips them without failure.

3. **`descriptor_utils.rs`**: Per the design plan, tested via fuzzing in Phase 4 instead of Miri.

Remove the temporary `miri_spike_guest_memory` test after documenting the result.

### Step 5.6 — Verify Miri infrastructure works

```bash
cargo +nightly miri test -p vmm -- dirty_bitmap
```

This should pass cleanly and serves as the baseline to confirm Miri is installed and working.

---

## Justfile Targets Added (Phase 2)

These targets are added to the justfile created in Phase 1. They stub out the commands that Phase 3 will make functional — adding them now keeps the `just all` target honest about what will run.

```just
# Miri: run pure-logic unit tests under Miri (requires nightly)
miri:
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p vmm -- dirty_bitmap
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p devices --features net -- balloon::reclaimed_bitmap

# Loom: exhaustive concurrency testing on bitmap types (Phase 3 adds tests)
loom:
    RUSTFLAGS="--cfg loom" cargo test --release -p vmm -- dirty_bitmap
    RUSTFLAGS="--cfg loom" cargo test --release -p devices --features net -- balloon::reclaimed_bitmap

# proptest: property-based tests for bitmap invariants and address translation (Phase 3 adds tests)
proptest:
    cargo test -p vmm --features snapshot -- proptest
    cargo test -p devices --features net -- proptest

# proptest-long: extended proptest runs (10x cases)
proptest-long:
    PROPTEST_CASES=10000 cargo test -p vmm --features snapshot -- proptest
    PROPTEST_CASES=10000 cargo test -p devices --features net -- proptest
```

Update `just all` to include the new targets:

```just
all: build test miri loom proptest
```

---

## Verification

After completing all tasks, verify the full refactoring is behavior-preserving:

```bash
# 1. Full build (no feature omissions)
cargo build --features embedded_init,snapshot,uffd,blk,vhost-user

# 2. Unit tests unchanged
cargo test -p vmm --features snapshot
cargo test -p devices --features net,snapshot,blk

# 3. justfile integration test (requires nix shell)
just test

# 4. Miri smoke test (confirms shims compile)
cargo +nightly miri test -p vmm -- dirty_bitmap

# 5. Confirm loom shim compiles (no loom tests yet)
RUSTFLAGS="--cfg loom" cargo test --release -p vmm -- dirty_bitmap
RUSTFLAGS="--cfg loom" cargo test --release -p devices --features net -- balloon::reclaimed_bitmap
```

All unit tests must pass identically before and after Phase 2. The refactoring is behavior-preserving — no logic changes.

---

## Design Discrepancy Notes

- **`UffdRegion` visibility:** Was private (`struct UffdRegion`) in uffd.rs. The split requires it to be visible across `page_tracker.rs` and `handler.rs`. Make it `pub struct UffdRegion` in page_tracker.rs.

- **`is_eexist` in page_tracker.rs:** References `userfaultfd::Error` and `libc::EEXIST`. This creates a compile-time dependency on `userfaultfd` in page_tracker.rs. This is acceptable since page_tracker.rs is already gated behind `#[cfg(all(target_os = "linux", feature = "uffd"))]` via the parent uffd module. If loom testing wants to avoid the uffd dependency, `is_eexist` could alternatively stay in handler.rs, but this splits the pure-logic goal.

- **`Opcode` enum variants:** The exact set of opcodes dispatched in `handle_message` must be verified against the actual server.rs lines 95–156 before writing fuse_dispatch.rs. The 44 variants listed in Task 3.1 were identified by the codebase investigator; some may not appear in handle_message's match if they're handled elsewhere.

- **`just miri` target in Phase 2:** The Phase 2 `just miri` stub only covers dirty_bitmap and reclaimed_bitmap. Phase 3 extends it to cover page_tracker.rs and request.rs once those modules have their test suites.
