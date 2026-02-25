# VhostUserFs with DAX Implementation Plan - Phase 7

**Goal:** Build a purpose-built minimal vhost-user filesystem test daemon that serves a synthetic in-memory filesystem with DAX support and DEVICE_STATE state transfer.

**Architecture:** A standalone Rust binary in the test workspace that acts as a vhost-user backend. It implements the `VhostUserBackendMut` trait from the `vhost-user-backend` crate, which handles the vhost-user protocol (connection, feature negotiation, memory regions, vring management). The daemon implements FUSE protocol message handling in `process_queue()` by reading FUSE requests from virtqueue descriptor chains. SETUPMAPPING writes a known byte pattern to the DAX window. DEVICE_STATE save/load is handled via the crate's built-in `set_device_state_fd()` and `check_device_state()` trait methods (supported since vhost-user-backend v0.14.0).

**Tech Stack:** Rust, vhost-user-backend crate (>= 0.14.0), vhost crate v0.15, virtio-queue crate, vm-memory crate, nix

**Scope:** 8 phases from original design (phase 7 of 8)

**Codebase verified:** 2026-02-24

**Reference files:**
- Test workspace: `tests/Cargo.toml` (workspace members)
- Test patterns: `tests/test_cases/src/mem_block_backend.rs` (custom backend pattern)
- vhost-user-backend crate: `VhostUserBackendMut` trait (process_queue, set_device_state_fd, check_device_state)
- vhost crate: `VhostUserProtocolFeatures::DEVICE_STATE`, `VhostTransferStateDirection`, `VhostTransferStatePhase`

**FUSE protocol constants (from research):**
- FUSE_LOOKUP=1, FUSE_FORGET=2, FUSE_GETATTR=3, FUSE_OPEN=14, FUSE_READ=15, FUSE_WRITE=16, FUSE_INIT=26, FUSE_BATCH_FORGET=42, FUSE_SETUPMAPPING=48, FUSE_REMOVEMAPPING=49
- FUSE_ATTR_DAX = 2 (bit 1 in fuse_attr.flags)
- FUSE_HAS_INODE_DAX = 0x200000000 (bit 33 in init flags)
- FUSE_MAP_ALIGNMENT = alignment negotiated in FUSE_INIT

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-fs-dax.AC5: Test daemon serves synthetic filesystem with DAX
- **vhost-user-fs-dax.AC5.1 Success:** Daemon accepts vhost-user connection and negotiates FUSE_INIT with MAP_ALIGNMENT and HAS_INODE_DAX
- **vhost-user-fs-dax.AC5.2 Success:** LOOKUP/GETATTR responses set FUSE_ATTR_DAX on files
- **vhost-user-fs-dax.AC5.3 Success:** SETUPMAPPING writes known byte pattern to DAX window at requested offset
- **vhost-user-fs-dax.AC5.4 Success:** FUSE_READ returns different content than DAX path (allows guest to distinguish)
- **vhost-user-fs-dax.AC5.5 Success:** DEVICE_STATE save/load round-trips the in-memory file table
- **vhost-user-fs-dax.AC5.6 Success:** Daemon observes guest writes to DAX window (file content updated in synthetic filesystem)

---

<!-- START_TASK_1 -->
### Task 1: Create test_daemon crate in tests workspace

**Files:**
- Create: `tests/test_daemon/Cargo.toml`
- Create: `tests/test_daemon/src/main.rs`
- Modify: `tests/Cargo.toml` (add "test_daemon" to members)

**Implementation:**

`tests/test_daemon/Cargo.toml`:
```toml
[package]
name = "test-daemon"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "test-daemon"
path = "src/main.rs"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
log = "0.4"
env_logger = "0.10"
nix = { version = "0.27", features = ["socket", "uio", "fs"] }
libc = "0.2"
vhost = { version = "0.15", features = ["vhost-user"] }
vhost-user-backend = "0.16"
virtio-queue = "0.13"
vm-memory = { version = "0.16.2", features = ["backend-mmap"] }
```

**Note on crate versions:** `vhost-user-backend = "0.16"` includes built-in DEVICE_STATE support (added in v0.14.0 via PR #203). The `VhostUserBackendMut` trait provides `set_device_state_fd()` and `check_device_state()` methods with default implementations that return `Unsupported` — the daemon overrides these. Pin `vm-memory` to 0.16.2 per the tests workspace constraint. **Important:** Verify the `vhost-user-backend` version resolves and is compatible with `vhost = "0.15"` and `vm-memory = "0.16.2"`. Run `cargo check` in the tests workspace before proceeding. If `vhost-user-backend = "0.16"` does not resolve, check for a compatible version (the crate follows independent version numbering from the `vhost` crate).

`tests/test_daemon/src/main.rs`:
```rust
use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    socket_path: String,
    #[arg(long, default_value = "/dev/null")]
    shared_dir: String,  // Ignored, files are synthetic
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = Args::parse();
    // Phase 7 Tasks 2-5 will fill in the daemon logic
    todo!()
}
```

Add to `tests/Cargo.toml`:
```toml
members = ["runner", "guest-agent", "macros", "test_cases", "test_daemon"]
```

**Verification:**
```bash
cd tests && cargo build -p test-daemon
```

**Commit:** `feat(tests): create test-daemon crate scaffold`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Implement VhostUserBackendMut trait for FsBackend

**Files:**
- Create: `tests/test_daemon/src/backend.rs`
- Create: `tests/test_daemon/src/filesystem.rs`
- Modify: `tests/test_daemon/src/main.rs`

**Implementation:**

The `vhost-user-backend` crate handles all vhost-user protocol mechanics (connection, feature negotiation, memory sharing, vring setup). The daemon only needs to implement the `VhostUserBackendMut` trait.

`backend.rs` — FsBackend implementing VhostUserBackendMut:

```rust
use std::fs::File;
use std::sync::{Arc, Mutex};
use vhost::vhost_user::message::*;
use vhost_user_backend::{VhostUserBackendMut, VringMutex, VringT};
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};

use crate::filesystem::SyntheticFs;

const NUM_QUEUES: usize = 2;  // HPQ + 1 request queue
const QUEUE_SIZE: usize = 1024;

pub struct FsBackend {
    /// Synthetic filesystem state
    fs: SyntheticFs,
    /// DAX window pointer (set when ADD_MEM_REGION shares the memfd)
    dax_window: Option<(*mut u8, usize)>,
    /// Pending DEVICE_STATE transfer state
    device_state_result: Option<std::io::Result<()>>,
    /// Guest memory reference (set by update_memory)
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
}

// SAFETY: FsBackend is only accessed from single-threaded daemon context.
// The dax_window raw pointer is derived from an mmap that lives for
// the daemon's lifetime.
unsafe impl Send for FsBackend {}
unsafe impl Sync for FsBackend {}

impl VhostUserBackendMut for FsBackend {
    type Bitmap = ();
    type Vring = VringMutex;

    fn num_queues(&self) -> usize { NUM_QUEUES }

    fn max_queue_size(&self) -> usize { QUEUE_SIZE }

    fn set_event_idx(&mut self, _enabled: bool) {}

    fn features(&self) -> u64 {
        // Virtio features: VIRTIO_F_VERSION_1
        1u64 << 32
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
            | VhostUserProtocolFeatures::DEVICE_STATE
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        // Return VirtioFsConfig: tag="testfs" + num_request_queues=1
        let mut config = vec![0u8; 40];  // 36-byte tag + 4-byte u32
        let tag = b"testfs";
        config[..tag.len()].copy_from_slice(tag);
        config[36..40].copy_from_slice(&1u32.to_le_bytes());
        let end = std::cmp::min((offset as usize) + (size as usize), config.len());
        config[offset as usize..end].to_vec()
    }

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>)
        -> std::io::Result<()>
    {
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        evset: vmm_sys_util::epoll::EventSet,
        vrings: &[Self::Vring],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        // device_event = queue index, triggered by kick eventfd
        if device_event as usize >= vrings.len() {
            return Ok(());
        }
        let vring = &vrings[device_event as usize];
        self.process_queue(vring)?;
        Ok(())
    }

    // --- DEVICE_STATE support (built into crate since v0.14.0) ---

    fn set_device_state_fd(
        &mut self,
        direction: VhostTransferStateDirection,
        _phase: VhostTransferStatePhase,
        fd: File,
    ) -> std::io::Result<Option<File>> {
        // Handle in background, store result for check_device_state
        match direction {
            VhostTransferStateDirection::Save => {
                self.device_state_result = Some(self.save_state_to_fd(&fd));
            }
            VhostTransferStateDirection::Load => {
                self.device_state_result = Some(self.load_state_from_fd(&fd));
            }
        }
        Ok(None)  // No fd to return
    }

    fn check_device_state(&self) -> std::io::Result<()> {
        match &self.device_state_result {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(std::io::Error::new(e.kind(), e.to_string())),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "no state transfer in progress",
            )),
        }
    }
}
```

`filesystem.rs` — In-memory synthetic filesystem:

```rust
pub struct SyntheticFs {
    /// Fixed inode table
    pub inodes: HashMap<u64, Inode>,
    /// File content (for FUSE_READ, NOT for DAX)
    pub file_data: HashMap<u64, Vec<u8>>,
    /// DAX byte pattern (different from file_data)
    pub dax_pattern: u8,
    /// File content as seen through DAX (updated when guest writes to DAX window)
    pub dax_file_data: HashMap<u64, Vec<u8>>,
}

pub struct Inode {
    pub nodeid: u64,
    pub name: String,
    pub mode: u32,      // S_IFREG | 0o644
    pub size: u64,
    pub nlink: u32,
}
```

Initialize with:
- Root inode (nodeid=1, S_IFDIR)
- "hello.txt" (nodeid=2, S_IFREG, size=4096)
- file_data for nodeid=2: filled with 0xAA (FUSE_READ content)
- dax_pattern: 0xBB (DAX content, different from 0xAA to distinguish paths)

**Verification:**
```bash
cd tests && cargo build -p test-daemon
```

**Commit:** `feat(test-daemon): implement VhostUserBackendMut with synthetic filesystem`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Implement FUSE message handling in process_queue

**Verifies:** vhost-user-fs-dax.AC5.1, vhost-user-fs-dax.AC5.2, vhost-user-fs-dax.AC5.3, vhost-user-fs-dax.AC5.4, vhost-user-fs-dax.AC5.6

**Files:**
- Create: `tests/test_daemon/src/fuse.rs`
- Modify: `tests/test_daemon/src/backend.rs`

**Implementation:**

`fuse.rs` — FUSE request/response handling:

Add a `process_queue()` method on FsBackend that iterates available descriptor chains from the vring, reads FUSE requests, and writes FUSE responses:

```rust
impl FsBackend {
    pub fn process_queue(&mut self, vring: &VringMutex) -> std::io::Result<()> {
        let mut vring_lock = vring.get_mut();
        let mem = self.mem.as_ref().unwrap().memory();

        while let Some(desc_chain) = vring_lock.get_queue_mut().pop_descriptor_chain(&mem) {
            // Read FUSE request from readable descriptors
            let fuse_in_header = read_fuse_header(&desc_chain, &mem);

            // Dispatch based on opcode
            let response = match fuse_in_header.opcode {
                FUSE_INIT => self.handle_init(&desc_chain, &mem),
                FUSE_LOOKUP => self.handle_lookup(&desc_chain, &mem),
                FUSE_GETATTR => self.handle_getattr(&desc_chain, &mem),
                FUSE_OPEN => self.handle_open(&desc_chain, &mem),
                FUSE_READ => self.handle_read(&desc_chain, &mem),
                FUSE_SETUPMAPPING => self.handle_setupmapping(&desc_chain, &mem),
                FUSE_REMOVEMAPPING => self.handle_removemapping(&desc_chain, &mem),
                FUSE_FORGET | FUSE_BATCH_FORGET => { continue; }  // No response
                _ => self.handle_unknown(fuse_in_header.opcode),
            };

            // Write response to writable descriptor
            write_fuse_response(&desc_chain, &mem, &fuse_in_header, &response);

            // Add used descriptor back
            vring_lock.get_queue_mut().add_used(&mem, desc_chain.head_index(), response.len() as u32);
        }

        // Signal the guest
        vring_lock.signal_used_queue().ok();
        Ok(())
    }
}
```

FUSE handlers:

**FUSE_INIT (26):**
- Respond with `fuse_init_out`: major=7, minor=36, flags including FUSE_HAS_INODE_DAX
- AC5.1: negotiates MAP_ALIGNMENT and HAS_INODE_DAX

**FUSE_LOOKUP (1):**
- Read null-terminated filename, look up in inode table
- Respond with `fuse_entry_out`: nodeid, generation, attr with FUSE_ATTR_DAX flag set
- AC5.2: LOOKUP responses set FUSE_ATTR_DAX

**FUSE_GETATTR (3):**
- Respond with `fuse_attr_out`, set `attr.flags |= FUSE_ATTR_DAX` on regular files
- AC5.2: GETATTR responses set FUSE_ATTR_DAX

**FUSE_OPEN (14):**
- Respond with `fuse_open_out`: fh=nodeid

**FUSE_READ (15):**
- Respond with data from `file_data[fh]` (content 0xAA, not DAX pattern)
- AC5.4: READ returns different content than DAX path

**FUSE_SETUPMAPPING (48):**
- Parse `fuse_setupmapping_in`: fh, foffset, len, flags, moffset
- Write known byte pattern (0xBB) to DAX window at moffset:
```rust
fn handle_setupmapping(&mut self, ...) {
    // ...parse fh, foffset, len, flags, moffset...
    if let Some((dax_ptr, dax_size)) = &self.dax_window {
        let offset = moffset as usize;
        let write_len = len as usize;
        if offset + write_len <= *dax_size {
            unsafe {
                std::ptr::write_bytes(dax_ptr.add(offset), self.fs.dax_pattern, write_len);
            }
        }
    }
    // Respond with success
}
```
- AC5.3: SETUPMAPPING writes known byte pattern to DAX window

**FUSE_REMOVEMAPPING (49):**
- No-op, respond with success

**FUSE_FORGET (2), FUSE_BATCH_FORGET (42):**
- No-op, no response

**Guest write detection (AC5.6):**
```rust
fn sync_dax_writes(&mut self, nodeid: u64, moffset: usize, len: usize) {
    if let Some((dax_ptr, _)) = &self.dax_window {
        let mut buf = vec![0u8; len];
        unsafe {
            std::ptr::copy_nonoverlapping(dax_ptr.add(moffset), buf.as_mut_ptr(), len);
        }
        self.fs.dax_file_data.insert(nodeid, buf);
    }
}
```
Called during DEVICE_STATE save (Task 2's `save_state_to_fd`) to capture current DAX window state.

**DAX window setup:** The `vhost-user-backend` crate handles ADD_MEM_REGION internally. The DAX region appears in the `GuestMemoryAtomic<GuestMemoryMmap>` passed to `update_memory()`. To get the DAX window pointer:

1. In `update_memory()`, iterate the memory regions using `mem.memory().iter()`.
2. The DAX window is identifiable by its GPA: the SHM manager allocates DAX GPAs well above the guest RAM base (typically `>= 0x1_0000_0000`), while guest RAM regions start at lower addresses. Compare each region's `start_addr()` against the known RAM ceiling.
3. Once identified, get the host pointer via `region.get_host_address(MemoryRegionAddress(0))` and store it as `self.dax_window = Some((ptr, region.len()))`.

Alternatively, track the region count across `update_memory()` calls — ADD_MEM_REGION triggers an `update_memory()` with one additional region compared to the initial SET_MEM_TABLE call. The new region is the DAX window.

**Verification:**
```bash
cd tests && cargo build -p test-daemon
```

**Commit:** `feat(test-daemon): implement FUSE message handling with DAX support`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Implement DEVICE_STATE save/load helpers

**Verifies:** vhost-user-fs-dax.AC5.5

**Files:**
- Modify: `tests/test_daemon/src/backend.rs`

**Implementation:**

The `set_device_state_fd()` and `check_device_state()` trait methods are already wired in Task 2. This task implements the actual serialization logic:

```rust
impl FsBackend {
    fn save_state_to_fd(&mut self, fd: &File) -> std::io::Result<()> {
        use std::io::Write;

        // 1. Sync any guest DAX writes
        self.sync_all_dax_writes();

        // 2. Serialize filesystem state
        //    Simple format: number of files, then for each file:
        //    nodeid(u64) + name_len(u32) + name + data_len(u32) + data
        let mut buf = Vec::new();
        for (nodeid, inode) in &self.fs.inodes {
            buf.extend_from_slice(&nodeid.to_le_bytes());
            let name_bytes = inode.name.as_bytes();
            buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(name_bytes);
            if let Some(data) = self.fs.file_data.get(nodeid) {
                buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
                buf.extend_from_slice(data);
            } else {
                buf.extend_from_slice(&0u32.to_le_bytes());
            }
        }

        // 3. Write to fd (the pipe provided by the frontend)
        let mut file = fd.try_clone()?;
        file.write_all(&buf)?;
        Ok(())
    }

    fn load_state_from_fd(&mut self, fd: &File) -> std::io::Result<()> {
        use std::io::Read;

        // 1. Read all bytes from fd (pipe)
        let mut buf = Vec::new();
        let mut file = fd.try_clone()?;
        file.read_to_end(&mut buf)?;

        // 2. Deserialize and restore filesystem state
        self.fs = SyntheticFs::deserialize(&buf);
        Ok(())
    }
}
```

AC5.5: The save/load round-trip preserves the file table — after save+load, FUSE_READ and GETATTR return the same data as before.

**Verification:**
```bash
cd tests && cargo build -p test-daemon
```

**Commit:** `feat(test-daemon): implement DEVICE_STATE save/load serialization`
<!-- END_TASK_4 -->

<!-- START_TASK_5 -->
### Task 5: Wire up main with VhostUserDaemon and verify

**Files:**
- Modify: `tests/test_daemon/src/main.rs`

**Implementation:**

Wire together using the `vhost-user-backend` crate's `VhostUserDaemon`:

```rust
use std::sync::{Arc, Mutex, RwLock};
use vhost_user_backend::VhostUserDaemon;
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};

mod backend;
mod filesystem;
mod fuse;

use backend::FsBackend;
use filesystem::SyntheticFs;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = Args::parse();

    // 1. Create backend with synthetic filesystem
    let fs = SyntheticFs::new();
    let backend = Arc::new(RwLock::new(FsBackend::new(fs)));

    // 2. Create and start vhost-user daemon
    //    The crate handles:
    //    - Listening on Unix socket
    //    - Accepting connection from frontend
    //    - Protocol message dispatch (GET_FEATURES, SET_MEM_TABLE, etc.)
    //    - Vring setup and kick/call eventfd management
    //    - Epoll-based event loop for vring kicks
    //    - ADD_MEM_REGION for DAX window
    //    - SET_DEVICE_STATE_FD / CHECK_DEVICE_STATE dispatch
    let mut daemon = VhostUserDaemon::new(
        "test-fs-daemon".to_string(),
        backend,
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )?;

    log::info!("Starting daemon on {}", args.socket_path);
    let listener = std::os::unix::net::UnixListener::bind(&args.socket_path)?;
    daemon.start(listener)?;

    // 3. Wait for daemon to finish (blocks until frontend disconnects)
    daemon.wait()?;

    Ok(())
}
```

The `vhost-user-backend` crate manages the entire event loop:
- Accepts vhost-user connection from VMM frontend
- Dispatches protocol messages to trait methods
- When vring kick eventfd fires, calls `handle_event()` → `process_queue()`
- When frontend sends SET_DEVICE_STATE_FD, calls `set_device_state_fd()`
- When frontend sends CHECK_DEVICE_STATE, calls `check_device_state()`

**Note:** The exact `VhostUserDaemon` constructor API may differ between crate versions. During implementation, check the docs for the pinned version and adjust accordingly. The `start_with_listener()` vs `start()` API may vary — use whichever is available to bind to the specified socket path.

**Verification:**

Test that the daemon starts and listens:
```bash
cd tests && cargo build -p test-daemon
./target/debug/test-daemon --socket-path /tmp/test-vhost-fs.sock &
ls -la /tmp/test-vhost-fs.sock  # Should exist
kill %1
rm /tmp/test-vhost-fs.sock
```

**Commit:** `feat(test-daemon): wire up VhostUserDaemon main loop`
<!-- END_TASK_5 -->
