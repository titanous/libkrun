# Phase 4: Fuzzing

## Overview

**Goal:** Set up cargo-fuzz with 5 harnesses targeting unsafe boundaries in FUSE parsing,
virtio descriptor chains, snapshot deserialization, vhost-user message parsing, and block
request header parsing.

**Design reference:** `docs/design-plans/2026-03-03-testing-upgrade.md` — `<!-- START_PHASE_4 -->`

**Acceptance criteria addressed:**
- testing-upgrade.AC2.2: `just fuzz <target>` runs a named fuzz target
- testing-upgrade.AC2.2: `just fuzz-all [duration=60]` runs all 5 targets sequentially
- testing-upgrade.AC2.2: `just fuzz-list` lists available targets
- testing-upgrade.AC2.2: `just fuzz-corpus <target>` shows corpus for a target
- testing-upgrade.AC2.2: All 5 targets build without errors
- testing-upgrade.AC2.10: Initial 60-second runs produce no crashes

**Done when:** All 5 fuzz targets build (`cargo +nightly fuzz build`). Initial 60-second
runs of all 5 targets produce no crashes. Justfile targets are functional.

**Dependencies:** Phase 1 complete (justfile exists at project root with stub `fuzz` targets).

---

## Investigation Findings

| Item | Finding |
|------|---------|
| Existing `fuzz/` directory | None — must create from scratch |
| Root workspace members | `["src/libkrun", "src/krun_input"]` — fuzz must be excluded or be a separate workspace managed by cargo-fuzz |
| `Server::handle_message` signature | `(&self, mut r: Reader, w: Writer, shm_region: &Option<VirtioShmRegion>, exit_code: &Arc<AtomicI32>) -> Result<usize>` in `src/devices/src/virtio/fs/server.rs:78` |
| `FileSystem` default impls | All methods return `ENOSYS` by default — a zero-method struct suffices as a mock |
| `DescriptorChain::checked_new` | `(mem: &GuestMemoryMmap, desc_table: GuestAddress, queue_size: u16, index: u16) -> Option<DescriptorChain<'_>>` in `src/devices/src/virtio/queue.rs:223` |
| `Reader::new` / `Writer::new` | Both in `src/devices/src/virtio/descriptor_utils.rs:211,362`; take `(mem: &'a GuestMemoryMmap, chain: DescriptorChain<'a>)` |
| `create_descriptor_chain` | Test utility in `src/devices/src/virtio/descriptor_utils.rs:542`; gated behind `#[cfg(test)]` — fuzz targets must replicate the setup directly |
| `RequestHeader` location | `src/devices/src/virtio/block/worker.rs:42` — `pub struct RequestHeader { request_type: u32, _reserved: u32, sector: u64 }` implements `ByteValued` |
| `VmSnapshot` / `IncrementalSnapshot` | `src/vmm/src/snapshot.rs:157,327` — behind `feature = "snapshot"` (serde + bincode) |
| `VhostUserMsgHeader` visibility | `pub(super)` in `vendor/vhost/src/vhost_user/message.rs:234` — not directly accessible from outside the `vhost` crate; fuzz target must test `ByteValued` deserialization via raw bytes instead |
| `GuestMemoryMmap` setup | `GuestMemoryMmap::from_ranges(&[(GuestAddress(0x0), 0x10000)]).unwrap()` — standard test pattern |
| `devices` crate `test_utils` feature | Exists in `src/devices/Cargo.toml:20`; `create_descriptor_chain` is behind `#[cfg(test)]` not `test_utils`, so the fuzz targets replicate setup inline |
| cargo-fuzz workspace | cargo-fuzz creates its own `fuzz/Cargo.toml` workspace that is separate from the root workspace; nightly toolchain required |
| `SNAPSHOT_MAGIC` constant | `0x4B52_534E` defined at `src/vmm/src/snapshot.rs:19` |
| `validate_magic_and_version` | Private function (`fn`, not `pub`); the fuzz target calls `bincode::deserialize::<VmSnapshot>()` directly, which exercises the same code path as `load_vmstate` |

---

<!-- START_TASK_1 -->
## Task 1: Create `fuzz/` directory infrastructure

**Verifies:** testing-upgrade.AC2.2 (fuzz targets build)

**Files:**
- Create: `fuzz/Cargo.toml`
- Create: `fuzz/.gitignore`
- Create: `fuzz/fuzz_targets/` directory (via first fuzz target file)

**Implementation:**

**Step 1: Initialize the fuzz directory**

cargo-fuzz normally generates `fuzz/Cargo.toml` via `cargo fuzz init`, but since the
workspace uses a non-standard layout, create it manually.

Create `fuzz/Cargo.toml`:

```toml
[package]
name = "libkrun-fuzz"
version = "0.0.1"
publish = false
edition = "2021"

# cargo-fuzz requires nightly and uses a separate workspace.
# This Cargo.toml is intentionally NOT listed in the root workspace members.
# Run with: cargo +nightly fuzz run <target>

[dependencies]
libfuzzer-sys = "0.4"

# Crates under test.
# Include all features needed by the fuzz targets.
devices = { path = "../src/devices", features = ["blk", "vhost-user"] }
vmm = { path = "../src/vmm", features = ["snapshot"] }

[[bin]]
name = "fuzz_snapshot_deser"
path = "fuzz_targets/fuzz_snapshot_deser.rs"
test = false
doc = false

[[bin]]
name = "fuzz_block_request"
path = "fuzz_targets/fuzz_block_request.rs"
test = false
doc = false

[[bin]]
name = "fuzz_descriptor_chain"
path = "fuzz_targets/fuzz_descriptor_chain.rs"
test = false
doc = false

[[bin]]
name = "fuzz_fuse_parsing"
path = "fuzz_targets/fuzz_fuse_parsing.rs"
test = false
doc = false

[[bin]]
name = "fuzz_vhost_user_msg"
path = "fuzz_targets/fuzz_vhost_user_msg.rs"
test = false
doc = false
```

**Step 2: Create `fuzz/.gitignore`**

Corpus directories can grow large and contain sensitive system data; exclude them from git.
Artifact directories (crashes, timeouts) should be committed once triaged.

Create `fuzz/.gitignore`:

```gitignore
# Corpus directories grow large — store locally, not in git.
# Seed corpora in fuzz/corpus/<target>/ ARE committed (manually curated).
corpus/
# cargo-fuzz artifact directories (crashes, slow inputs).
# Commit triaged reproducers to fuzz/artifacts/<target>/ manually.
artifacts/
# Build output.
target/
```

**Step 3: Verify the fuzz workspace is excluded from the root workspace**

The root `Cargo.toml` at the project root uses `members = ["src/libkrun", "src/krun_input"]`.
cargo-fuzz's separate workspace in `fuzz/` is automatically excluded because it is not listed
in `members` and has its own `[package]` section. No changes to the root `Cargo.toml` are
needed.

**Verification:**

```bash
# Verify fuzz/Cargo.toml parses correctly as a standalone workspace
cd fuzz && cargo +nightly metadata --no-deps 2>&1 | head -5
```

Expected: Prints JSON metadata without errors. The `libkrun-fuzz` package appears in the
output.

**Do not commit yet** — all 5 targets must exist before the build can succeed.
<!-- END_TASK_1 -->

---

<!-- START_TASK_2 -->
## Task 2: `fuzz_snapshot_deser.rs` — snapshot deserialization fuzzing

**Verifies:** testing-upgrade.AC2.2, testing-upgrade.AC2.10

**Files:**
- Create: `fuzz/fuzz_targets/fuzz_snapshot_deser.rs`
- Create: `fuzz/corpus/fuzz_snapshot_deser/` (seed corpus directory)

**Background:**

`VmSnapshot` and `IncrementalSnapshot` are deserialized from untrusted bytes using bincode.
This fuzz target exercises bincode's `deserialize` for both types. There are no unsafe blocks
in the deserialization path itself, but bincode's `Vec` allocation from length-prefixed data
could trigger OOM or panics on adversarial inputs without the 10MB size limit enforced by
`load_vmstate`. The fuzz target tests the raw deserialization path without the file size gate.

This is the simplest fuzz target — pure data deserialization with no external dependencies.

**Implementation:**

Create `fuzz/fuzz_targets/fuzz_snapshot_deser.rs`:

```rust
#![no_main]

use libfuzzer_sys::fuzz_target;
use vmm::snapshot::{IncrementalSnapshot, VmSnapshot};

fuzz_target!(|data: &[u8]| {
    // Attempt to deserialize as VmSnapshot.
    // bincode::deserialize must not panic on arbitrary input.
    // Errors (e.g., unexpected end of input, invalid enum variant) are expected and fine.
    let _ = bincode::deserialize::<VmSnapshot>(data);

    // Also attempt to deserialize as IncrementalSnapshot.
    let _ = bincode::deserialize::<IncrementalSnapshot>(data);
});
```

**Seed corpus:**

Create a seed corpus entry with a minimal valid `VmSnapshot` serialized by bincode. The seed
helps the fuzzer find interesting code paths faster.

Create `fuzz/corpus/fuzz_snapshot_deser/seed_valid_vmsnapshot`:

Generate the seed by running this one-time script (add to `fuzz/gen_seeds.rs` for reference):

```rust
// One-time seed generation (run manually, not part of fuzz target):
// cargo +nightly run --manifest-path fuzz/Cargo.toml --bin gen_seeds
//
// Or generate the bytes inline:
fn gen_vmsnapshot_seed() -> Vec<u8> {
    use vmm::snapshot::{SnapshotHeader, VmSnapshot, SNAPSHOT_MAGIC, SNAPSHOT_VERSION};
    let snapshot = VmSnapshot {
        header: SnapshotHeader {
            magic: SNAPSHOT_MAGIC,
            version: SNAPSHOT_VERSION,
            vcpu_count: 1,
            ram_regions: vec![(0, 0x10000000)],
            nested_enabled: false,
        },
        vcpu_states: vec![vec![0u8; 32]],
        device_states: vec![("serial".to_string(), vec![0u8; 8])],
        gic_state: None,
        vm_state: None,
        excluded_pages: vec![],
    };
    bincode::serialize(&snapshot).unwrap()
}
```

To actually produce the seed file, run (from the project root):

```bash
# Write the seed corpus file manually.
# Use cargo +nightly to access the vmm crate with snapshot feature.
# A simpler approach: write a small script in fuzz/gen_seeds.sh

# Option A: produce the corpus via a simple cargo test (quickest for a CI setup):
cargo +nightly test -p vmm --features snapshot -- --ignored gen_fuzz_seeds 2>/dev/null

# Option B: hand-craft a binary seed. The bincode encoding of VmSnapshot
# begins with the SnapshotHeader fields. A valid minimal encoding is:
# magic(u32-le) version(u32-le) vcpu_count(u32-le) ram_regions_len(u64-le) ...
# This is non-trivial to hand-craft. Use Option A or simply let the fuzzer
# discover valid inputs from random exploration.
#
# For the initial run, an empty corpus directory is acceptable.
# LibFuzzer will generate its own starting corpus from scratch.
mkdir -p fuzz/corpus/fuzz_snapshot_deser
```

**Note:** For seeding, the simplest approach is to add a `#[test]` to `src/vmm/src/snapshot.rs`
that generates and writes the seed file. This is documented in Task 7 (justfile). For initial
bring-up, an empty corpus directory is acceptable — libfuzzer starts from random bytes and
will explore the bincode format quickly.

**Verification:**

```bash
cargo +nightly fuzz build fuzz_snapshot_deser
```

Expected: Compiles without errors. May print warnings about unused imports — those are fine.

```bash
cargo +nightly fuzz run fuzz_snapshot_deser -- -max_total_time=10
```

Expected: Runs for 10 seconds, produces no crashes (`SUMMARY: libFuzzer: no crashes`).
<!-- END_TASK_2 -->

---

<!-- START_TASK_3 -->
## Task 3: `fuzz_block_request.rs` — virtio block request header fuzzing

**Verifies:** testing-upgrade.AC2.2, testing-upgrade.AC2.10

**Files:**
- Create: `fuzz/fuzz_targets/fuzz_block_request.rs`
- Create: `fuzz/corpus/fuzz_block_request/` (seed corpus directory)

**Background:**

The virtio block worker reads a `RequestHeader` from a descriptor chain using
`reader.read_obj::<RequestHeader>()`. `RequestHeader` is a `#[repr(C)]` struct that
implements `ByteValued`. The fuzz target exercises the request type discrimination logic
(the `match req_type { VIRTIO_BLK_T_IN => ... }` in `process_request`) with arbitrary
`request_type`, `_reserved`, and `sector` values.

`RequestHeader` is defined in `src/devices/src/virtio/block/worker.rs` as `pub struct`
but `worker.rs` is not a `pub mod` from `block/mod.rs`. To access it, the fuzz target
re-declares an equivalent struct using `ByteValued`. This is safe because `ByteValued`
is only valid for types with no padding and only POD fields, and we verify the size matches.

**Implementation:**

Create `fuzz/fuzz_targets/fuzz_block_request.rs`:

```rust
#![no_main]

use libfuzzer_sys::fuzz_target;
use vm_memory::ByteValued;

/// Replicates `RequestHeader` from `devices::virtio::block::worker`.
/// Fields are `pub` here for direct construction in corpus seeding.
///
/// Layout: request_type(u32) + _reserved(u32) + sector(u64) = 16 bytes total.
#[derive(Copy, Clone, Default)]
#[repr(C)]
struct RequestHeader {
    request_type: u32,
    _reserved: u32,
    sector: u64,
}

// SAFETY: RequestHeader is #[repr(C)] with only POD fields and no padding.
unsafe impl ByteValued for RequestHeader {}

// Virtio block request type constants (from virtio_bindings::virtio_blk).
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;
const VIRTIO_BLK_T_DISCARD: u32 = 11;
const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

fuzz_target!(|data: &[u8]| {
    // RequestHeader is 16 bytes. If the input is too short, pad with zeros.
    let mut buf = [0u8; std::mem::size_of::<RequestHeader>()];
    let copy_len = data.len().min(buf.len());
    buf[..copy_len].copy_from_slice(&data[..copy_len]);

    // SAFETY: Any bit pattern is valid for RequestHeader (it's ByteValued).
    let header = RequestHeader {
        request_type: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
        _reserved: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        sector: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
    };

    // Exercise the request type discrimination logic.
    // This mirrors the match in BlockWorker::process_request.
    let _type_name = match header.request_type {
        VIRTIO_BLK_T_IN => "READ",
        VIRTIO_BLK_T_OUT => "WRITE",
        VIRTIO_BLK_T_FLUSH => "FLUSH",
        VIRTIO_BLK_T_GET_ID => "GET_ID",
        VIRTIO_BLK_T_DISCARD => "DISCARD",
        VIRTIO_BLK_T_WRITE_ZEROES => "WRITE_ZEROES",
        _ => "UNKNOWN",
    };

    // Verify sector bounds check (mirrors what process_request does):
    // sector * 512 must not overflow u64.
    let _sector_byte_offset = header.sector.checked_mul(512);
});
```

**Seed corpus:**

Create seed files covering the valid request types. Each seed is exactly 16 bytes.

Create `fuzz/corpus/fuzz_block_request/seed_read`:

```
# 16 bytes: type=READ(0), reserved=0, sector=0
\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00
```

Create the seed files using a shell script:

```bash
mkdir -p fuzz/corpus/fuzz_block_request

# VIRTIO_BLK_T_IN (0) sector=0
printf '\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_block_request/seed_read

# VIRTIO_BLK_T_OUT (1) sector=1
printf '\x01\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_block_request/seed_write

# VIRTIO_BLK_T_FLUSH (4)
printf '\x04\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_block_request/seed_flush

# VIRTIO_BLK_T_GET_ID (8)
printf '\x08\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_block_request/seed_get_id

# Unknown type (0xFF)
printf '\xff\x00\x00\x00\x00\x00\x00\x00\xff\xff\xff\xff\xff\xff\xff\xff' \
    > fuzz/corpus/fuzz_block_request/seed_unknown
```

**Verification:**

```bash
cargo +nightly fuzz build fuzz_block_request
```

Expected: Compiles without errors.

```bash
cargo +nightly fuzz run fuzz_block_request -- -max_total_time=10
```

Expected: Runs 10 seconds, no crashes.
<!-- END_TASK_3 -->

---

<!-- START_TASK_4 -->
## Task 4: `fuzz_descriptor_chain.rs` — virtio descriptor chain fuzzing

**Verifies:** testing-upgrade.AC2.2, testing-upgrade.AC2.10

**Files:**
- Create: `fuzz/fuzz_targets/fuzz_descriptor_chain.rs`
- Create: `fuzz/corpus/fuzz_descriptor_chain/`

**Background:**

The virtio descriptor chain parsing code in `src/devices/src/virtio/queue.rs` and
`src/devices/src/virtio/descriptor_utils.rs` contains unsafe code inside `Reader::new` and
`Writer::new` (pointer arithmetic into guest memory). `DescriptorChain::checked_new` reads
a `Descriptor` struct from guest memory at the provided `desc_table` address and follows the
`next` chain pointer. Arbitrary bytes in the descriptor table region could trigger edge cases
in chain traversal (cycles, out-of-bounds `next` indices, overflow in total length
accumulation).

The fuzz target writes arbitrary bytes into a fixed guest memory region as the descriptor
table, then calls `DescriptorChain::checked_new` and attempts to construct `Reader` and
`Writer`. All panics would be a bug — the code must handle all inputs gracefully.

**Implementation:**

Create `fuzz/fuzz_targets/fuzz_descriptor_chain.rs`:

```rust
#![no_main]

use libfuzzer_sys::fuzz_target;

use devices::virtio::descriptor_utils::{Reader, Writer};
use devices::virtio::queue::DescriptorChain;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

/// Guest memory layout:
///   [0x0000, 0x0100): descriptor table (16 bytes per descriptor * 16 = 256 bytes)
///   [0x0100, 0x8000): data buffers (pointed to by descriptors)
///
/// We write the fuzzer input into the descriptor table region, then let the
/// descriptor chain parser interpret whatever bytes were placed there.
const DESC_TABLE_ADDR: u64 = 0x0;
const DATA_START_ADDR: u64 = 0x100;
const QUEUE_SIZE: u16 = 16;
const MEM_SIZE: usize = 0x8000;

fuzz_target!(|data: &[u8]| {
    // Allocate a fresh guest memory region for each fuzzing iteration.
    let mem = match GuestMemoryMmap::from_ranges(&[(GuestAddress(0x0), MEM_SIZE)]) {
        Ok(m) => m,
        Err(_) => return,
    };

    // Write the fuzzer-provided bytes into the descriptor table region.
    // Limit to the descriptor table area (QUEUE_SIZE * 16 bytes = 256 bytes).
    let desc_table_size = (QUEUE_SIZE as usize) * 16;
    let write_len = data.len().min(desc_table_size);
    if write_len > 0 {
        // Ignore write errors — the memory region is always valid.
        let _ = mem.write_slice(&data[..write_len], GuestAddress(DESC_TABLE_ADDR));
    }

    // Attempt to construct a descriptor chain from index 0.
    // checked_new returns None for invalid chains — that's expected.
    let chain = DescriptorChain::checked_new(
        &mem,
        GuestAddress(DESC_TABLE_ADDR),
        QUEUE_SIZE,
        0, // start at index 0
    );

    let Some(chain) = chain else {
        // Invalid descriptor table — this is expected for most random inputs.
        return;
    };

    // Try constructing a Reader over the chain.
    // Reader::new contains unsafe code (VolatileSlice pointer arithmetic).
    // Any panic here is a bug.
    let chain_for_reader = chain.clone();
    let reader_result = Reader::new(&mem, chain_for_reader);

    // Try constructing a Writer over the chain.
    let writer_result = Writer::new(&mem, chain);

    // If both succeeded, exercise the Reader by reading bytes.
    if let (Ok(mut reader), Ok(_writer)) = (reader_result, writer_result) {
        // Read up to 64 bytes; ignore I/O errors (expected for malformed chains).
        let mut buf = [0u8; 64];
        let _ = reader.read(&mut buf);
    }
});
```

Note: The `read` method on `Reader` implements `std::io::Read`. The import must be in scope:

Add `use std::io::Read;` at the top of the file:

```rust
#![no_main]

use std::io::Read;

use libfuzzer_sys::fuzz_target;

use devices::virtio::descriptor_utils::{Reader, Writer};
use devices::virtio::queue::DescriptorChain;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
```

**Verify the import path:** `devices::virtio::descriptor_utils` and `devices::virtio::queue`
must be `pub` from the `devices` crate root. Check `src/devices/src/lib.rs` to confirm the
re-export path. If `virtio` is a `pub mod` in `lib.rs`, the imports are correct. If the
path differs, adjust accordingly.

**Seed corpus:**

Create seeds covering:
1. A valid single-descriptor chain (readable, not chained)
2. A chain with the NEXT flag set pointing to index 1

```bash
mkdir -p fuzz/corpus/fuzz_descriptor_chain

# Seed 1: a minimal valid read-only descriptor.
# Layout: addr(u64-le) + len(u32-le) + flags(u16-le) + next(u16-le)
# addr=0x100, len=64, flags=0 (readable, no NEXT), next=0
printf '\x00\x01\x00\x00\x00\x00\x00\x00\x40\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_descriptor_chain/seed_single_readable

# Seed 2: write-only descriptor (flags=VIRTQ_DESC_F_WRITE=0x2).
# addr=0x100, len=64, flags=2, next=0
printf '\x00\x01\x00\x00\x00\x00\x00\x00\x40\x00\x00\x00\x02\x00\x00\x00' \
    > fuzz/corpus/fuzz_descriptor_chain/seed_single_writable

# Seed 3: two-descriptor chain (NEXT flag set on first descriptor).
# Descriptor 0: addr=0x100, len=32, flags=NEXT(1), next=1
# Descriptor 1: addr=0x120, len=32, flags=0, next=0
printf '\x00\x01\x00\x00\x00\x00\x00\x00\x20\x00\x00\x00\x01\x00\x01\x00' \
       '\x20\x01\x00\x00\x00\x00\x00\x00\x20\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_descriptor_chain/seed_two_descriptor_chain
```

**Verification:**

```bash
cargo +nightly fuzz build fuzz_descriptor_chain
```

Expected: Compiles without errors.

```bash
cargo +nightly fuzz run fuzz_descriptor_chain -- -max_total_time=10
```

Expected: Runs 10 seconds, no crashes. The fuzzer will mostly return early (invalid chain),
which is correct.
<!-- END_TASK_4 -->

---

<!-- START_TASK_5 -->
## Task 5: `fuzz_fuse_parsing.rs` — FUSE message parsing fuzzing

**Verifies:** testing-upgrade.AC2.2, testing-upgrade.AC2.10

**Files:**
- Create: `fuzz/fuzz_targets/fuzz_fuse_parsing.rs`
- Create: `fuzz/corpus/fuzz_fuse_parsing/`

**Background:**

`Server::handle_message` in `src/devices/src/virtio/fs/server.rs` reads a FUSE `InHeader`
from a `Reader`, then dispatches to one of ~30 opcode handlers. Each handler reads additional
structs from the `Reader` using `r.read_obj::<T>()`, which has `unsafe` code internally
(`assume_init()` on an uninitialized buffer). The fuzz target exercises this entire dispatch
path with arbitrary bytes.

A zero-method `struct NullFs;` implementing `FileSystem` is used as the mock backend. All
`FileSystem` methods have default `ENOSYS` implementations, so the mock requires no method
implementations.

The `Server` also requires a `VirtioShmRegion` parameter (the DAX shared memory region) and
an `Arc<AtomicI32>` exit code. Both can be provided as `None` and a zeroed atomic respectively.

**Implementation:**

Create `fuzz/fuzz_targets/fuzz_fuse_parsing.rs`:

```rust
#![no_main]

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;

use devices::virtio::descriptor_utils::{Reader, Writer};
use devices::virtio::fs::filesystem::FileSystem;
use devices::virtio::fs::server::Server;
use devices::virtio::queue::DescriptorChain;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

/// A minimal FileSystem implementation that returns ENOSYS for all operations.
/// No methods need to be implemented — the FileSystem trait provides default ENOSYS impls.
struct NullFs;

impl FileSystem for NullFs {}

/// Guest memory layout:
///   [0x0000, 0x0100): descriptor table (1 readable + 1 writable descriptor)
///   [0x0100, 0x1100): readable data buffer (fuzz input — FUSE request)
///   [0x1100, 0x2100): writable data buffer (FUSE response)
const DESC_TABLE_ADDR: u64 = 0x0;
const READ_BUF_ADDR: u64 = 0x100;
const WRITE_BUF_ADDR: u64 = 0x1100;
const READ_BUF_LEN: u32 = 0x1000; // 4096 bytes for FUSE request
const WRITE_BUF_LEN: u32 = 0x1000; // 4096 bytes for FUSE response
const MEM_SIZE: usize = 0x4000;

fuzz_target!(|data: &[u8]| {
    // Set up guest memory.
    let mem = match GuestMemoryMmap::from_ranges(&[(GuestAddress(0x0), MEM_SIZE)]) {
        Ok(m) => m,
        Err(_) => return,
    };

    // Write fuzz data as the FUSE request into the readable buffer.
    let write_len = data.len().min(READ_BUF_LEN as usize);
    if write_len > 0 {
        let _ = mem.write_slice(&data[..write_len], GuestAddress(READ_BUF_ADDR));
    }

    // Build the descriptor table:
    //   Descriptor 0 (readable): addr=READ_BUF_ADDR, len=READ_BUF_LEN, flags=NEXT(1), next=1
    //   Descriptor 1 (writable): addr=WRITE_BUF_ADDR, len=WRITE_BUF_LEN, flags=WRITE(2), next=0
    //
    // Descriptor layout: addr(u64) + len(u32) + flags(u16) + next(u16) = 16 bytes
    let desc0: [u8; 16] = {
        let mut d = [0u8; 16];
        d[0..8].copy_from_slice(&READ_BUF_ADDR.to_le_bytes());
        d[8..12].copy_from_slice(&READ_BUF_LEN.to_le_bytes());
        d[12..14].copy_from_slice(&1u16.to_le_bytes()); // flags = VIRTQ_DESC_F_NEXT
        d[14..16].copy_from_slice(&1u16.to_le_bytes()); // next = 1
        d
    };
    let desc1: [u8; 16] = {
        let mut d = [0u8; 16];
        d[0..8].copy_from_slice(&WRITE_BUF_ADDR.to_le_bytes());
        d[8..12].copy_from_slice(&WRITE_BUF_LEN.to_le_bytes());
        d[12..14].copy_from_slice(&2u16.to_le_bytes()); // flags = VIRTQ_DESC_F_WRITE
        d[14..16].copy_from_slice(&0u16.to_le_bytes()); // next = 0
        d
    };
    let _ = mem.write_slice(&desc0, GuestAddress(DESC_TABLE_ADDR));
    let _ = mem.write_slice(&desc1, GuestAddress(DESC_TABLE_ADDR + 16));

    // Construct the descriptor chain starting at index 0.
    let chain = match DescriptorChain::checked_new(
        &mem,
        GuestAddress(DESC_TABLE_ADDR),
        16, // queue_size
        0,  // index
    ) {
        Some(c) => c,
        None => return,
    };

    // Build Reader and Writer over the chain.
    let reader = match Reader::new(&mem, chain.clone()) {
        Ok(r) => r,
        Err(_) => return,
    };
    let writer = match Writer::new(&mem, chain) {
        Ok(w) => w,
        Err(_) => return,
    };

    // Run the FUSE server message dispatcher.
    let server = Server::new(Box::new(NullFs));
    let exit_code = Arc::new(AtomicI32::new(0));
    // shm_region is None — DAX is not exercised.
    let _ = server.handle_message(reader, writer, &None, &exit_code);
});
```

**Verify imports:** The following items must be accessible:
- `devices::virtio::fs::filesystem::FileSystem` — confirmed as `pub` in filesystem.rs
- `devices::virtio::fs::server::Server` — check visibility in `src/devices/src/virtio/fs/mod.rs`.
  Per the CLAUDE.md for the fs device: `server` module is "crate-internal". If `Server` is not
  `pub` outside the `devices` crate, the fuzz target cannot access it directly.

**If `Server` is not pub outside `devices`:** Add a thin wrapper in `src/devices/src/lib.rs` or
`src/devices/src/virtio/fs/mod.rs` that re-exports `Server` behind a `test_utils` or `fuzz` feature
flag, or move the `pub(crate)` visibility to `pub`. The simplest fix is to change `server.rs` from
`mod server` to `pub mod server` in `src/devices/src/virtio/fs/mod.rs` (confirm current visibility
first):

```bash
# Check current visibility of server module
grep "server" src/devices/src/virtio/fs/mod.rs
```

If `mod server;` is not `pub mod server;`, change it. The `Server` struct is already `pub` in
`server.rs` (confirmed from the codebase investigation).

**Seed corpus:**

Create seeds from valid FUSE request headers. A FUSE `InHeader` is:
`len(u32) + opcode(u32) + unique(u64) + nodeid(u64) + uid(u32) + gid(u32) + pid(u32) + padding(u32)`
= 40 bytes.

```bash
mkdir -p fuzz/corpus/fuzz_fuse_parsing

# FUSE INIT request (opcode=26, FUSE_INIT)
# len=40 (header only), opcode=26, unique=1, nodeid=1, uid=0, gid=0, pid=0, padding=0
printf '\x28\x00\x00\x00\x1a\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00' \
       '\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00' \
       '\x00\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_fuse_parsing/seed_init

# FUSE LOOKUP request (opcode=1)
# len=48 (header + 8 bytes for filename ".\0"), opcode=1, unique=2, nodeid=1
printf '\x30\x00\x00\x00\x01\x00\x00\x00\x02\x00\x00\x00\x00\x00\x00\x00' \
       '\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00' \
       '\x00\x00\x00\x00\x00\x00\x00\x00\x2e\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_fuse_parsing/seed_lookup

# Unknown opcode (0xDEAD) — exercises the default error path
printf '\x28\x00\x00\x00\xad\xde\x00\x00\x03\x00\x00\x00\x00\x00\x00\x00' \
       '\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00' \
       '\x00\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_fuse_parsing/seed_unknown_opcode
```

**Verification:**

First, verify `Server` is accessible:

```bash
grep -n "mod server\|pub mod server" src/devices/src/virtio/fs/mod.rs
```

If `mod server;` (without `pub`), change to `pub(crate) mod server;` or `pub mod server;` and
re-export `Server` from the devices crate root.

Then:

```bash
cargo +nightly fuzz build fuzz_fuse_parsing
```

Expected: Compiles without errors.

```bash
cargo +nightly fuzz run fuzz_fuse_parsing -- -max_total_time=10
```

Expected: Runs 10 seconds, no crashes. The majority of inputs will fail at
`r.read_obj::<InHeader>()` (not enough bytes) — this is correct behavior.
<!-- END_TASK_5 -->

---

<!-- START_TASK_6 -->
## Task 6: `fuzz_vhost_user_msg.rs` — vhost-user message struct fuzzing

**Verifies:** testing-upgrade.AC2.2, testing-upgrade.AC2.10

**Files:**
- Create: `fuzz/fuzz_targets/fuzz_vhost_user_msg.rs`
- Create: `fuzz/corpus/fuzz_vhost_user_msg/`

**Background:**

The vhost-user message header `VhostUserMsgHeader<R>` is defined in
`vendor/vhost/src/vhost_user/message.rs` with visibility `pub(super)` — it is not accessible
from outside the `vhost` crate. The actual message parsing in production goes through Unix
socket `recvmsg` syscalls in `Endpoint::recv_message`, which cannot be fuzzed in-process
without significant scaffolding.

Instead, this fuzz target tests the message header deserialization via `ByteValued`, which is
the same mechanism used by `recvmsg` under the hood. `ByteValued` is `unsafe impl`'d for
`VhostUserMsgHeader`, meaning any 12-byte sequence is a valid header. The fuzz target
exercises `get_code()`, `get_version()`, `get_size()`, and `is_valid()` on arbitrary byte
sequences cast to the header struct.

Since `VhostUserMsgHeader` is `pub(super)`, this fuzz target accesses it by duplicating the
struct definition locally. This is safe: `ByteValued` is a marker trait and the struct is
`#[repr(C, packed)]` with only POD fields. The local definition will match the on-wire format
exactly.

**Implementation:**

Create `fuzz/fuzz_targets/fuzz_vhost_user_msg.rs`:

```rust
#![no_main]

use libfuzzer_sys::fuzz_target;
use vm_memory::ByteValued;

/// Local replica of `VhostUserMsgHeader` from vendor/vhost/src/vhost_user/message.rs.
///
/// The original is `pub(super)` and cannot be accessed outside the vhost crate.
/// This replica has the same on-wire layout:
///   request(u32) + flags(u32) + size(u32) = 12 bytes total.
///
/// This exercises:
/// 1. `ByteValued` zero-copy deserialization of arbitrary bytes as a header
/// 2. Flag field parsing: version bits [1:0], REPLY bit [2], NEED_REPLY bit [3]
/// 3. Request type validation (whether the u32 maps to a known FrontendReq)
/// 4. Size field interpretation
#[derive(Copy, Clone, Default, Debug)]
#[repr(C, packed)]
struct VhostUserMsgHeaderReplica {
    request: u32,
    flags: u32,
    size: u32,
}

// SAFETY: VhostUserMsgHeaderReplica is #[repr(C, packed)] with only POD fields.
unsafe impl ByteValued for VhostUserMsgHeaderReplica {}

// Bit masks from VhostUserHeaderFlag (message.rs).
const VERSION_MASK: u32 = 0x3;
const REPLY_FLAG: u32 = 0x4;
const NEED_REPLY_FLAG: u32 = 0x8;
const ALL_FLAGS: u32 = 0xc;
const RESERVED_BITS: u32 = !0xf;

// Known FrontendReq variants (from message.rs enum definition).
const KNOWN_REQUEST_TYPES: &[u32] = &[
    1,  // GET_FEATURES
    2,  // SET_FEATURES
    3,  // SET_OWNER
    4,  // RESET_OWNER
    5,  // SET_MEM_TABLE
    8,  // SET_VRING_NUM
    9,  // SET_VRING_ADDR
    10, // SET_VRING_BASE
    11, // GET_VRING_BASE
    12, // SET_VRING_KICK
    13, // SET_VRING_CALL
    14, // SET_VRING_ERR
    15, // GET_PROTOCOL_FEATURES
    16, // SET_PROTOCOL_FEATURES
];

fuzz_target!(|data: &[u8]| {
    // VhostUserMsgHeaderReplica is 12 bytes. Pad with zeros if input is too short.
    let mut buf = [0u8; std::mem::size_of::<VhostUserMsgHeaderReplica>()];
    let copy_len = data.len().min(buf.len());
    buf[..copy_len].copy_from_slice(&data[..copy_len]);

    // Interpret arbitrary bytes as a message header.
    // SAFETY: Any bit pattern is valid for a ByteValued type.
    let header = VhostUserMsgHeaderReplica {
        request: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
        flags: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        size: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
    };

    // Exercise flag parsing — mirrors VhostUserMsgHeader::get_version(), is_reply(), etc.
    let _version = header.flags & VERSION_MASK;
    let _is_reply = (header.flags & REPLY_FLAG) != 0;
    let _needs_reply = (header.flags & NEED_REPLY_FLAG) != 0;
    let _has_reserved = (header.flags & RESERVED_BITS) != 0;

    // Exercise request type validation.
    let _is_known = KNOWN_REQUEST_TYPES.contains(&header.request);

    // Exercise size field interpretation.
    // In production: size must be <= MAX_MSG_SIZE (4096). Check the boundary.
    const MAX_MSG_SIZE: u32 = 0x1000;
    let _size_valid = header.size <= MAX_MSG_SIZE;
    let _size_overflow = header.size.checked_add(12); // header + body overflow check
});
```

**Seed corpus:**

```bash
mkdir -p fuzz/corpus/fuzz_vhost_user_msg

# GET_FEATURES request: request=1, flags=0x1 (version 1), size=0
printf '\x01\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_vhost_user_msg/seed_get_features

# SET_FEATURES request: request=2, flags=0x9 (NEED_REPLY|version1), size=8
printf '\x02\x00\x00\x00\x09\x00\x00\x00\x08\x00\x00\x00' \
    > fuzz/corpus/fuzz_vhost_user_msg/seed_set_features

# Reply with REPLY flag: request=1, flags=0x5 (REPLY|version1), size=8
printf '\x01\x00\x00\x00\x05\x00\x00\x00\x08\x00\x00\x00' \
    > fuzz/corpus/fuzz_vhost_user_msg/seed_reply

# Unknown request type (0xDEADBEEF):
printf '\xef\xbe\xad\xde\x01\x00\x00\x00\x00\x00\x00\x00' \
    > fuzz/corpus/fuzz_vhost_user_msg/seed_unknown

# Max size: request=1, flags=0x1, size=4096
printf '\x01\x00\x00\x00\x01\x00\x00\x00\x00\x10\x00\x00' \
    > fuzz/corpus/fuzz_vhost_user_msg/seed_max_size
```

**Verification:**

```bash
cargo +nightly fuzz build fuzz_vhost_user_msg
```

Expected: Compiles without errors. No access to `vhost` crate internals is needed since
the struct is replicated locally.

```bash
cargo +nightly fuzz run fuzz_vhost_user_msg -- -max_total_time=10
```

Expected: Runs 10 seconds, no crashes.
<!-- END_TASK_6 -->

---

<!-- START_TASK_7 -->
## Task 7: Update justfile fuzz targets

**Verifies:** testing-upgrade.AC2.2 (all fuzzing targets testable)

**Files:**
- Modify: `justfile` at project root (replace stub fuzz targets from Phase 1)

**Implementation:**

Replace the stub `fuzz`, `fuzz-all`, `fuzz-list`, and `fuzz-corpus` targets in the justfile
with working implementations.

Locate the stub targets in `justfile` (added in Phase 1):

```just
fuzz target:
    @echo "fuzz: set up in Phase 4 (Fuzzing)"
    @exit 1

fuzz-all duration="60":
    @echo "fuzz-all: set up in Phase 4 (Fuzzing)"
    @exit 1

fuzz-list:
    @echo "fuzz-list: set up in Phase 4 (Fuzzing)"
    @exit 1

fuzz-corpus target:
    @echo "fuzz-corpus: set up in Phase 4 (Fuzzing)"
    @exit 1
```

Replace them with:

```just
# Run a single fuzz target for a given duration.
# Usage: just fuzz fuzz_snapshot_deser
#        just fuzz fuzz_fuse_parsing 120
fuzz target duration="60":
    cargo +nightly fuzz run --manifest-path fuzz/Cargo.toml {{target}} -- -max_total_time={{duration}}

# Run all fuzz targets sequentially, each for the given duration.
# Usage: just fuzz-all
#        just fuzz-all 300
fuzz-all duration="60":
    for target in $(just fuzz-list); do \
        echo "--- Fuzzing $target for {{duration}}s ---"; \
        just fuzz $target {{duration}}; \
    done

# List all available fuzz targets.
fuzz-list:
    @cargo +nightly fuzz list --manifest-path fuzz/Cargo.toml 2>/dev/null \
        || grep '^name = ' fuzz/Cargo.toml | grep -v 'libkrun-fuzz' | sed 's/name = "\(.*\)"/\1/'

# Show corpus statistics for a fuzz target.
# Usage: just fuzz-corpus fuzz_snapshot_deser
fuzz-corpus target:
    @if [ -d "fuzz/corpus/{{target}}" ]; then \
        echo "Corpus for {{target}}:"; \
        ls -lh fuzz/corpus/{{target}}/; \
        echo "Total: $(ls fuzz/corpus/{{target}}/ | wc -l) files"; \
    else \
        echo "No corpus directory yet: fuzz/corpus/{{target}}/"; \
        echo "Run 'just fuzz {{target}}' to start generating one."; \
    fi
```

Also update the `safety` compound target to include `fuzz-all`:

Locate the `safety` target (added in Phase 1):

```just
# Compound target: safety checks (extended in later phases)
# Phase 1: check only
# Later phases add: asan miri fuzz-all kani
safety: check
```

Replace with:

```just
# Compound target: safety checks.
# Phase 1: check
# Phase 4: + fuzz-all (60s per target)
# Later phases add: asan miri kani
safety: check fuzz-all
```

**Verification:**

```bash
just fuzz-list
```

Expected output (one target per line):
```
fuzz_block_request
fuzz_descriptor_chain
fuzz_fuse_parsing
fuzz_snapshot_deser
fuzz_vhost_user_msg
```

```bash
just fuzz-corpus fuzz_snapshot_deser
```

Expected: Prints corpus file listing or "No corpus directory yet" message (no error exit).

```bash
just fuzz fuzz_block_request 5
```

Expected: Runs `fuzz_block_request` for 5 seconds, no crashes.

**Commit:** `feat: add cargo-fuzz infrastructure and 5 fuzz targets (Phase 4)`
<!-- END_TASK_7 -->

---

<!-- START_TASK_8 -->
## Task 8: Run full 60-second fuzz session on all targets

**Verifies:** testing-upgrade.AC2.10 (no crashes in initial 60-second runs)

**Files:** None (verification only)

**Implementation:**

```bash
# Build all fuzz targets first.
cargo +nightly fuzz build --manifest-path fuzz/Cargo.toml
```

Expected: All 5 targets compile.

```bash
# Run each target for 60 seconds.
just fuzz fuzz_snapshot_deser 60
just fuzz fuzz_block_request 60
just fuzz fuzz_descriptor_chain 60
just fuzz fuzz_fuse_parsing 60
just fuzz fuzz_vhost_user_msg 60
```

Expected for each: libFuzzer output ending with `SUMMARY: libFuzzer: no crashes`. The
per-second execution count will vary; `fuzz_block_request` and `fuzz_vhost_user_msg` will be
fastest (pure struct manipulation), while `fuzz_descriptor_chain` and `fuzz_fuse_parsing` will
be slower (memory allocation per iteration).

If any target crashes, investigate:

1. Identify the crash input: `ls fuzz/artifacts/<target>/crash-*`
2. Reproduce: `cargo +nightly fuzz run --manifest-path fuzz/Cargo.toml <target> fuzz/artifacts/<target>/crash-<hash>`
3. Analyze: `cargo +nightly fuzz fmt --manifest-path fuzz/Cargo.toml <target> fuzz/artifacts/<target>/crash-<hash>`

A crash in `fuzz_descriptor_chain` or `fuzz_fuse_parsing` is a real bug that must be fixed
before this phase is marked done. A crash in `fuzz_snapshot_deser`, `fuzz_block_request`, or
`fuzz_vhost_user_msg` would indicate a bug in bincode or the local struct definition.

**Commit:** `test(fuzz): add seed corpus files for all fuzz targets`
<!-- END_TASK_8 -->

---

## Verification

After completing all tasks, run the following sequence to verify the phase is complete:

```bash
# 1. All fuzz targets build.
cargo +nightly fuzz build --manifest-path fuzz/Cargo.toml

# 2. fuzz-list returns 5 targets.
just fuzz-list

# 3. Individual fuzz run (quick smoke test).
just fuzz fuzz_snapshot_deser 10
just fuzz fuzz_block_request 10
just fuzz fuzz_descriptor_chain 10
just fuzz fuzz_fuse_parsing 10
just fuzz fuzz_vhost_user_msg 10

# 4. Full 60-second run of all targets.
just fuzz-all 60

# 5. Corpus utilities work.
just fuzz-corpus fuzz_snapshot_deser

# 6. Existing tests still pass (no regressions from visibility changes).
cargo test -p devices --features net,snapshot,blk,vhost-user
cargo test -p vmm --features snapshot
```

---

## Design Discrepancy Notes

- **`VhostUserMsgHeader` is `pub(super)`:** The design specifies testing vhost-user message
  parsing via `VhostUserMsgHeader`. The actual type is `pub(super)` in
  `vendor/vhost/src/vhost_user/message.rs` and inaccessible from outside the crate. The fuzz
  target in Task 6 replicates the struct locally as `VhostUserMsgHeaderReplica`. This tests
  the same on-wire parsing logic without requiring changes to the vendored crate's visibility.
  If future work requires direct access, add `pub(crate)` visibility to the type in the vendor
  crate (the vendor crate is a patched local copy).

- **`Server` visibility:** Per `src/devices/src/virtio/fs/CLAUDE.md`, `server` is listed as
  "crate-internal". Verify the actual `mod server` declaration in
  `src/devices/src/virtio/fs/mod.rs` before running `fuzz_fuse_parsing`. If `Server` is not
  accessible from outside `devices`, change `mod server;` to `pub(crate) mod server;` in that
  file (no API breakage — it was already crate-internal, just making it visible to the fuzz
  workspace which depends on `devices` directly).

- **`create_descriptor_chain` is `#[cfg(test)]`:** The design references
  `create_descriptor_chain` from `descriptor_utils.rs`. This utility is gated behind
  `#[cfg(test)]` and is not available to the fuzz workspace. Task 4 replicates the descriptor
  table layout inline using direct `mem.write_slice` calls. This is equivalent and more
  transparent about what the fuzz target is doing.

- **`RequestHeader` is not re-exported from `devices`:** The `RequestHeader` struct is defined
  in `src/devices/src/virtio/block/worker.rs` which is a `mod worker` (non-`pub`) within
  `src/devices/src/virtio/block/mod.rs`. The fuzz target in Task 3 replicates the struct
  locally. An alternative is to move `RequestHeader` to `block/mod.rs` with `pub` visibility
  as part of Phase 2 (pure logic extraction), which would make it directly accessible.
  For Phase 4, the local replica approach avoids requiring Phase 2 as a prerequisite.

- **`bincode` dependency in fuzz/Cargo.toml:** The `fuzz_snapshot_deser` target uses
  `bincode::deserialize`. The `vmm` crate re-exports `bincode` as an optional dependency
  (behind `feature = "snapshot"`). The fuzz workspace must add `bincode` as a direct
  dependency since it calls `bincode::deserialize` directly:

  Add to `fuzz/Cargo.toml`:
  ```toml
  bincode = "1.3"
  vm-memory = { version = "0.18", features = ["backend-mmap"] }
  ```

  The `vm-memory` dependency is needed by `fuzz_descriptor_chain` for `GuestAddress`,
  `GuestMemoryMmap`, and `Bytes` imports. Since `devices` and `vmm` both depend on
  `vm-memory 0.18`, Cargo will unify to the same version.
