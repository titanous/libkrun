# Virtio-FS over Vhost-User with DAX Design

## Summary

libkrun currently supports virtio-fs by implementing a FUSE frontend directly inside the VMM process — the VMM handles filesystem operations itself. This design plan adds an alternative: a `VhostUserFs` device that delegates all filesystem logic to an external daemon process, communicating via the vhost-user protocol over a Unix socket. The daemon owns the filesystem implementation; the VMM acts only as a thin transport layer, forwarding virtqueue traffic and sharing memory regions. A configurable DAX shared memory window is the centerpiece: rather than reading file data through the virtqueue on every access, the daemon maps file content directly into a memory region shared with the guest, and the guest kernel accesses it as ordinary memory — eliminating per-operation VMM involvement for hot data.

The implementation is structured in eight phases, building on infrastructure from an existing PR (#527) that introduced a generic `VhostUserDevice` wrapper and memfd-backed guest memory. `VhostUserFs` adds filesystem-specific concerns — config space, device identity, queue layout, and the DAX window — on top of that generic foundation. Snapshot/restore is a first-class requirement: the design includes a custom implementation of `SET_DEVICE_STATE_FD`/`CHECK_DEVICE_STATE` (vhost-user message IDs 42/43) to transfer daemon internal state into the VMM snapshot, allowing the daemon to be restarted and re-synced on restore. A purpose-built minimal test daemon (serving a synthetic in-memory filesystem) exercises DAX and state-transfer code paths in integration tests.

## Definition of Done

A new `VhostUserFs` device implementing the `VirtioDevice` trait communicates with an external vhost-user filesystem daemon over a Unix socket using the vhost-user protocol, coexisting with the existing direct FUSE `Fs` device. The device exposes a configurable DAX shared memory window (virtio SHM region ID 0) shared with the daemon via the vhost-user memory table, enabling the guest kernel and daemon to handle DAX mappings through the FUSE protocol (`FUSE_SETUPMAPPING`/`FUSE_REMOVEMAPPING`), with per-file DAX via `FUSE_ATTR_DAX`. A Rust Builder API method (no C API) configures the device. The device supports full snapshot/restore including daemon state transfer via `VHOST_USER_SET_DEVICE_STATE_FD`/`CHECK_DEVICE_STATE`. An end-to-end integration test uses a minimal purpose-built test daemon to exercise file I/O through the vhost-user virtio-fs device with DAX active, using the existing `#[host]`/`#[guest]` test framework. The implementation builds on PR #527's infrastructure (memfd guest memory, vhost-user device wrapper), stripped of RNG device and C API additions.

Out of scope: C API, replacing existing direct FUSE virtio-fs device, non-Linux platforms, contributing DAX to virtiofsd, live migration (dirty logging).

## Acceptance Criteria

### vhost-user-fs-dax.AC1: VhostUserDevice generic wrapper works
- **vhost-user-fs-dax.AC1.1 Success:** VhostUserDevice connects to a vhost-user daemon over a Unix socket and completes feature negotiation
- **vhost-user-fs-dax.AC1.2 Success:** VhostUserDevice shares memfd-backed guest memory with daemon via SET_MEM_TABLE
- **vhost-user-fs-dax.AC1.3 Failure:** VhostUserDevice returns error when daemon socket is unavailable
- **vhost-user-fs-dax.AC1.4 Success:** Existing tests pass without regression after PR #527 integration

### vhost-user-fs-dax.AC2: VhostUserFs device exposes correct virtio-fs identity
- **vhost-user-fs-dax.AC2.1 Success:** device_type() returns 26 (VIRTIO_ID_FS)
- **vhost-user-fs-dax.AC2.2 Success:** Config space contains filesystem tag and num_request_queues from daemon (via VHOST_USER_GET_CONFIG)
- **vhost-user-fs-dax.AC2.3 Success:** Queue layout has HPQ (queue 0) + N request queues, each with 1024 descriptors
- **vhost-user-fs-dax.AC2.4 Success:** shm_region() returns VirtioShmRegion with SHM region ID 0 when DAX configured
- **vhost-user-fs-dax.AC2.5 Success:** shm_region() returns None when dax_window_mib is None
- **vhost-user-fs-dax.AC2.6 Success:** DAX window memfd shared with daemon as additional region via ADD_MEM_REGION (CONFIGURE_MEM_SLOTS)

### vhost-user-fs-dax.AC3: Builder API configures the device
- **vhost-user-fs-dax.AC3.1 Success:** `add_virtiofs_vhost_user(tag, socket_path, Some(32))` results in a bootable VM with the device visible to the guest kernel
- **vhost-user-fs-dax.AC3.2 Success:** `add_virtiofs_vhost_user(tag, socket_path, None)` configures device without DAX window
- **vhost-user-fs-dax.AC3.3 Failure:** Tag longer than 36 bytes is rejected
- **vhost-user-fs-dax.AC3.4 Success:** Coexists with existing direct FUSE virtio-fs device (both can be configured on same VM)

### vhost-user-fs-dax.AC4: Snapshot/restore preserves device and daemon state
- **vhost-user-fs-dax.AC4.1 Success:** SET_DEVICE_STATE_FD (msg 42) transfers daemon state to VMM via pipe
- **vhost-user-fs-dax.AC4.2 Success:** CHECK_DEVICE_STATE (msg 43) confirms daemon finished state transfer
- **vhost-user-fs-dax.AC4.3 Success:** Snapshot captures vring bases (GET_VRING_BASE), device config, and daemon state blob
- **vhost-user-fs-dax.AC4.4 Success:** Restore reconnects to daemon, re-shares memory + DAX window, restores vring bases and daemon state
- **vhost-user-fs-dax.AC4.5 Success:** DAX window contents survive snapshot/restore (captured as part of guest memory)
- **vhost-user-fs-dax.AC4.6 Failure:** Restore fails gracefully when daemon is not running at socket path

### vhost-user-fs-dax.AC5: Test daemon serves synthetic filesystem with DAX
- **vhost-user-fs-dax.AC5.1 Success:** Daemon accepts vhost-user connection and negotiates FUSE_INIT with MAP_ALIGNMENT and HAS_INODE_DAX
- **vhost-user-fs-dax.AC5.2 Success:** LOOKUP/GETATTR responses set FUSE_ATTR_DAX on files
- **vhost-user-fs-dax.AC5.3 Success:** SETUPMAPPING writes known byte pattern to DAX window at requested offset
- **vhost-user-fs-dax.AC5.4 Success:** FUSE_READ returns different content than DAX path (allows guest to distinguish)
- **vhost-user-fs-dax.AC5.5 Success:** DEVICE_STATE save/load round-trips the in-memory file table
- **vhost-user-fs-dax.AC5.6 Success:** Daemon observes guest writes to DAX window (file content updated in synthetic filesystem)

### vhost-user-fs-dax.AC6: End-to-end integration tests pass
- **vhost-user-fs-dax.AC6.1 Success:** Guest mounts virtiofs, reads file, receives DAX-specific byte pattern (proving DAX active, not FUSE_READ fallback)
- **vhost-user-fs-dax.AC6.2 Success:** Guest writes to a DAX-mapped file, reads back via DAX, and verifies the written content persists
- **vhost-user-fs-dax.AC6.3 Success:** Snapshot/restore test: mount + DAX read, snapshot, daemon restart, restore, DAX content survives
- **vhost-user-fs-dax.AC6.4 Success:** Tests pass with `make test FEATURE_FLAGS="--features embedded_init,vhost-user"`
- **vhost-user-fs-dax.AC6.5 Edge:** Tests use `#[host]`/`#[guest]` proc macro framework consistent with existing test patterns

## Glossary

- **virtio-fs**: A virtio device type (ID 26) that exposes a filesystem to a guest VM. The guest kernel uses a FUSE driver to communicate with the host over virtqueues.
- **vhost-user protocol**: A socket-based protocol that lets an external userspace process (a "daemon") act as the backend for a virtio device. The VMM handles the guest-visible virtio interface; the daemon does the actual work.
- **DAX (Direct Access)**: A mechanism that bypasses the page cache. In virtio-fs, a shared memory window is exposed to the guest; the daemon maps file content into it, and the guest reads/writes the data directly without FUSE read/write operations per access.
- **FUSE**: Filesystem in Userspace. A Linux kernel interface and protocol that allows a userspace process to implement a filesystem. In virtio-fs, FUSE operations travel over virtqueues.
- **FUSE_SETUPMAPPING / FUSE_REMOVEMAPPING**: FUSE protocol messages for DAX. `SETUPMAPPING` asks the daemon to map a file region into the DAX window; `REMOVEMAPPING` releases a mapping when the kernel reclaims the slot.
- **FUSE_ATTR_DAX**: A per-file attribute flag set by the daemon in `GETATTR`/`LOOKUP` responses. When set, the kernel uses DAX for that file rather than regular FUSE read/write.
- **FUSE_HAS_INODE_DAX / MAP_ALIGNMENT**: Capabilities negotiated during `FUSE_INIT`. `HAS_INODE_DAX` signals per-file DAX support; `MAP_ALIGNMENT` conveys alignment requirements for DAX window mappings.
- **virtio SHM region**: A virtio mechanism for exposing a named shared memory window. virtio-fs uses SHM region ID 0 (`VIRTIO_FS_SHMCAP_ID_CACHE`) for the DAX window.
- **memfd**: A Linux facility (`memfd_create`) that creates an anonymous file backed by memory, sharable between processes via fd passing. Backs guest RAM and the DAX window so the daemon can mmap them.
- **SET_MEM_TABLE / ADD_MEM_REGION / CONFIGURE_MEM_SLOTS**: vhost-user messages for sharing memory with a daemon. `SET_MEM_TABLE` shares guest RAM; `ADD_MEM_REGION` (enabled by `CONFIGURE_MEM_SLOTS`) adds extra regions like the DAX window.
- **SET_DEVICE_STATE_FD / CHECK_DEVICE_STATE**: vhost-user protocol messages (IDs 42/43) for snapshot/restore. The VMM sends a pipe fd to the daemon, which writes or reads its internal state through it; `CHECK_DEVICE_STATE` confirms the transfer completed.
- **GET_VRING_BASE / SET_VRING_BASE**: vhost-user messages that retrieve or restore the current position in each virtqueue ring. Required for snapshot so guest and daemon resume from the same point.
- **HPQ (High-Priority Queue)**: Queue 0 in virtio-fs, reserved for high-priority requests (typically `FUSE_FORGET`). Separate from the N request queues carrying normal filesystem operations.
- **VirtioDevice trait**: The Rust interface all virtio devices implement. Defines device identity, config space, queue layout, interrupt handling, and optional shared memory (`shm_region()`).
- **Snapshottable**: A codebase trait for VM snapshot/restore participation. Requires `pause()` (save state) and `resume()` (restore state) methods.
- **VhostUserDevice**: Generic wrapper (from PR #527) handling vhost-user frontend protocol — socket connection, feature negotiation, memory sharing, vring setup, interrupt forwarding.
- **ShmManager**: VMM component (`src/vmm/src/device_manager/shm.rs`) that allocates guest physical address ranges for shared memory regions.
- **FUSE_DAX_SZ**: Fixed 2MB chunk size the kernel uses to divide the DAX window into mapping slots. A 32MB window provides 16 simultaneous mappings.
- **rust-vmm `vhost` crate**: Community Rust library providing vhost-user protocol types and helpers. Used for the frontend; `DEVICE_STATE` messages are implemented outside it (not yet supported by the crate).
- **`#[host]` / `#[guest]` proc macros**: Test framework macros that split an integration test into a host-side runner (spawns the VM) and a guest-side workload (runs inside the VM).

## Architecture

`VhostUserFs` wraps a generic `VhostUserDevice` (from PR #527) with filesystem-specific specialization. The VMM acts as a pass-through for DAX: it allocates the DAX window and shares it with the daemon, but all DAX mapping logic flows between the guest kernel and daemon via the FUSE protocol over virtqueues.

```
Host                                    Guest VM
┌─────────────────┐  unix socket  ┌───────────────────────────┐
│ vhost-user-fs   │◄────────────►│ VhostUserFs device         │
│ daemon          │  vhost-user   │ ┌───────────────────────┐ │
│                 │  protocol     │ │ VhostUserDevice        │ │
│ Handles:        │               │ │ (generic, PR #527)     │ │
│ - FUSE ops      │  memfd share  │ │ - socket connection    │ │
│ - SETUPMAPPING  │◄────────────►│ │ - feature negotiation  │ │
│ - REMOVEMAPPING │ (RAM + DAX)   │ │ - set_mem_table        │ │
│ - ATTR_DAX      │               │ │ - vring setup          │ │
│ - DEVICE_STATE  │               │ │ - interrupt forwarding │ │
│                 │               │ └───────────────────────┘ │
│                 │               │ ┌───────────────────────┐ │
│                 │               │ │ FS specialization      │ │
│                 │               │ │ - config space (tag)   │ │
│                 │               │ │ - DAX window (SHM 0)   │ │
│                 │               │ │ - device_type = 26     │ │
│                 │               │ │ - HPQ + request queues │ │
│                 │               │ │ - Snapshottable        │ │
│                 │               │ └───────────────────────┘ │
└─────────────────┘               └───────────────────────────┘
                                           │
                                  ┌────────┴────────┐
                                  │ Linux kernel     │
                                  │ virtio-fs driver │
                                  │ + FUSE DAX       │
                                  │ (fs/fuse/dax.c)  │
                                  └─────────────────┘
```

**Memory sharing:** All guest RAM and the DAX window are backed by memfd (from PR #527). The vhost-user `SET_MEM_TABLE` shares RAM regions with the daemon. The DAX window is shared as an additional region via `ADD_MEM_REGION` (requires `CONFIGURE_MEM_SLOTS` protocol feature). The daemon mmaps both, giving it direct access to virtqueues and the DAX window.

**DAX flow:** The guest kernel discovers the DAX window as virtio SHM region ID 0 (`VIRTIO_FS_SHMCAP_ID_CACHE`). During `FUSE_INIT`, kernel and daemon negotiate `FUSE_MAP_ALIGNMENT` and `FUSE_HAS_INODE_DAX`. When a DAX-eligible file is accessed, the kernel allocates a 2MB chunk (`FUSE_DAX_SZ`) from its free pool within the window and sends `FUSE_SETUPMAPPING(file_offset, window_offset, 2MB)` over the virtqueue. The daemon receives this and writes/maps the file content into the DAX window at the specified offset. The guest then accesses the data directly through the window. Under memory pressure, the kernel reclaims chunks via `FUSE_REMOVEMAPPING`.

**Per-file DAX:** The daemon controls per-file DAX eligibility by setting `FUSE_ATTR_DAX` in `FUSE_GETATTR`/`FUSE_LOOKUP` responses. Files without this flag fall back to regular FUSE read/write through the virtqueue. The kernel supports four DAX modes: `always`, `never`, `inode` (default), and `inode` (user-controlled via mount option).

**Snapshot/restore:** `VhostUserFs` implements `Snapshottable`. On snapshot, VMM saves device config, retrieves vring state via `GET_VRING_BASE`, and transfers daemon internal state via custom `VHOST_USER_SET_DEVICE_STATE_FD`/`CHECK_DEVICE_STATE` protocol messages. The DAX window contents are captured automatically as part of guest memory. On restore, VMM reconnects to the daemon, re-shares memory and DAX window, sends the saved state blob for daemon restoration, and restores vring bases. The daemon must be running at the same socket path on restore.

**Contract:** The `VhostUserDevice` generic wrapper handles all vhost-user protocol mechanics. `VhostUserFs` adds only FS-specific concerns: config space, DAX window, device type, queue layout, and snapshot state. The `VirtioDevice` trait is the boundary between the device and MMIO transport — no changes to the transport layer.

```rust
// Builder API contract
pub fn add_virtiofs_vhost_user(
    &mut self,
    tag: &str,           // filesystem mount tag (max 36 bytes)
    socket_path: &str,   // vhost-user Unix socket path
    dax_window_mib: Option<u32>, // None = no DAX, Some(N) = N MiB window
) -> &mut Self
```

## Existing Patterns

**Device model:** Follows the existing `VirtioDevice` trait pattern used by all virtio devices in `src/devices/src/virtio/`. The existing direct FUSE `Fs` device at `src/devices/src/virtio/fs/device.rs` demonstrates the virtio-fs queue layout (HPQ + request queues), config space structure, and `shm_region()` for DAX windows.

**SHM management:** Reuses `ShmManager` at `src/vmm/src/device_manager/shm.rs` which already allocates guest address ranges for shared memory regions. `ShmManager::create_fs_region()` handles page-aligned allocation from `info.shm_start_addr`.

**Builder API:** Follows the pattern of `Builder::add_virtiofs()` at `src/libkrun/src/lib.rs:2546` — configuration struct added to `VmResources`, device created in `build_microvm()`, attached via `attach_mmio_device()`.

**Feature flags:** Cascading feature flags match the `net`/`blk` pattern: root `Cargo.toml` → `vmm/Cargo.toml` → `devices/Cargo.toml`.

**Integration tests:** Uses the `#[host]`/`#[guest]` proc macro pattern from `tests/macros/`, with test cases registered in `tests/test_cases/src/lib.rs`.

**New pattern:** The generic `VhostUserDevice` wrapper (from PR #527) is new to the codebase. It introduces vhost-user frontend protocol handling and memfd-backed guest memory. This becomes the foundation for all future vhost-user devices.

**New pattern:** Custom `VHOST_USER_SET_DEVICE_STATE_FD`/`CHECK_DEVICE_STATE` protocol messages implemented directly in libkrun, since the rust-vmm `vhost` crate (through v0.15) does not support them. This diverges from using the crate's built-in protocol handling and may be upstreamed later.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: Apply and Strip PR #527

**Goal:** Establish vhost-user infrastructure foundation.

**Components:**
- Apply `https://github.com/containers/libkrun/pull/527.patch` to the codebase
- Remove: `krun_add_vhost_user_device()` C API from `src/libkrun/src/lib.rs`, `krun_disable_implicit_rng()` C API, RNG-specific logic in `src/vmm/src/builder.rs` (attach_vhost_user_device RNG suppression), `include/libkrun.h` additions, `examples/chroot_vm.c` changes
- Keep: `VhostUserDevice` in `src/devices/src/virtio/vhost_user/device.rs`, memfd-backed memory in `src/vmm/src/builder.rs` (`create_guest_memory` and `load_payload` memfd paths), `VhostUserDeviceConfig` in `src/vmm/src/resources.rs`, `vhost-user` feature flag chain across all three `Cargo.toml` files, `VhostUserDevice` error variant in `src/vmm/src/device_manager/kvm/mmio.rs`

**Dependencies:** None (first phase)

**Done when:** `cargo build --features vhost-user` succeeds. `cargo test` (existing tests) still passes without regressions.
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: VhostUserFs Device

**Goal:** FS-specific specialization wrapping the generic `VhostUserDevice`.

**Components:**
- `VhostUserFs` struct in `src/devices/src/virtio/vhost_user/fs.rs` — wraps `VhostUserDevice`, adds FS config space, DAX window, device_type=26
- Config space: fetches `virtio_fs_config` (tag + num_request_queues) from daemon via `VHOST_USER_GET_CONFIG`, caches locally, serves via `read_config()`
- Queue layout: HPQ (queue 0, size 1024) + N request queues (size 1024), count from daemon config
- DAX window: allocates memfd of configured size, registers with `ShmManager`, exposes via `shm_region()` returning `VirtioShmRegion` with SHM region ID 0
- `VhostUserFsConfig` in `src/vmm/src/resources.rs` — tag, socket_path, dax_window_mib

**Dependencies:** Phase 1

**Done when:** `VhostUserFs` constructs from a socket path, negotiates features, reports correct device_type/config space/queue layout/shm_region. Covers vhost-user-fs-dax.AC1.1, AC1.2, AC2.1, AC2.2.
<!-- END_PHASE_2 -->

<!-- START_PHASE_3 -->
### Phase 3: DAX Window Sharing

**Goal:** Share the DAX window with the daemon so it can map file content into it.

**Components:**
- Protocol feature negotiation in `VhostUserDevice` or `VhostUserFs`: negotiate `CONFIGURE_MEM_SLOTS` with daemon
- After `set_mem_table` (RAM regions), call `add_mem_region` to share the DAX window memfd as an additional memory region
- The daemon receives the DAX window fd and mmaps it, giving it direct write access

**Dependencies:** Phase 2

**Done when:** Daemon receives the DAX window as a shared memory region and can write to it. Verifiable with a test that connects to a daemon and confirms the region is shared. Covers vhost-user-fs-dax.AC2.3.
<!-- END_PHASE_3 -->

<!-- START_PHASE_4 -->
### Phase 4: Builder API and VMM Integration

**Goal:** Wire `VhostUserFs` into the VM construction pipeline.

**Components:**
- `Builder::add_virtiofs_vhost_user(tag, socket_path, dax_window_mib)` in `src/libkrun/src/lib.rs`
- `VhostUserFsConfig` stored in `VmResources` at `src/vmm/src/resources.rs`
- `attach_vhost_user_fs_device()` in `src/vmm/src/builder.rs` — creates `VhostUserFs`, configures DAX window via `ShmManager`, attaches via `attach_mmio_device()`
- Feature-gated with `#[cfg(feature = "vhost-user")]`

**Dependencies:** Phase 3

**Done when:** A VM can be configured with `add_virtiofs_vhost_user()` and boots with the device visible to the guest kernel. Covers vhost-user-fs-dax.AC3.1, AC3.2.
<!-- END_PHASE_4 -->

<!-- START_PHASE_5 -->
### Phase 5: DEVICE_STATE Protocol

**Goal:** Implement `SET_DEVICE_STATE_FD` and `CHECK_DEVICE_STATE` vhost-user messages for daemon state transfer during snapshot/restore.

**Components:**
- Custom message types added to vhost-user frontend code in `src/devices/src/virtio/vhost_user/`
- `VHOST_USER_SET_DEVICE_STATE_FD` (message id 42): sends pipe fd to daemon with direction (SAVE/LOAD) and migration phase
- `VHOST_USER_CHECK_DEVICE_STATE` (message id 43): confirms daemon finished processing
- `DEVICE_STATE` added to protocol feature negotiation
- Helper methods on `VhostUserDevice`: `save_device_state() -> Vec<u8>` and `load_device_state(data: &[u8])`

**Dependencies:** Phase 1 (uses VhostUserDevice)

**Done when:** Frontend can send SET_DEVICE_STATE_FD, read/write state via pipe, and confirm via CHECK_DEVICE_STATE. Unit tests verify the protocol message exchange. Covers vhost-user-fs-dax.AC4.1, AC4.2.
<!-- END_PHASE_5 -->

<!-- START_PHASE_6 -->
### Phase 6: Snapshot/Restore

**Goal:** Full snapshot/restore support for `VhostUserFs`.

**Components:**
- `Snapshottable` implementation for `VhostUserFs` in `src/devices/src/virtio/vhost_user/fs.rs`
- Snapshot state struct (serde + bincode): tag, socket_path, dax_window_mib, acked_features, acked_protocol_features, vring_bases, device_state_blob
- `pause()`: call `GET_VRING_BASE` for each queue, call `save_device_state()` via DEVICE_STATE protocol
- `resume()`: reconnect to daemon, re-negotiate features, re-share memory table + DAX window, call `load_device_state()`, call `SET_VRING_BASE` for each queue, enable vrings
- Feature-gated with `#[cfg(feature = "snapshot")]` + `#[cfg(feature = "vhost-user")]`

**Dependencies:** Phases 4, 5

**Done when:** VhostUserFs can be snapshot'd and restored with all vring state and daemon state preserved. Covers vhost-user-fs-dax.AC4.3, AC4.4, AC4.5.
<!-- END_PHASE_6 -->

<!-- START_PHASE_7 -->
### Phase 7: Minimal Test Daemon

**Goal:** Purpose-built vhost-user filesystem daemon for integration testing.

**Components:**
- `tests/test_daemon/` — separate binary in test workspace, host-only
- Vhost-user backend using `vhost-user-backend` + `virtio-queue` + `vm-memory` crates
- In-memory synthetic filesystem: fixed inode table, no real host files
- FUSE operations: INIT (negotiates MAP_ALIGNMENT, HAS_INODE_DAX), LOOKUP, GETATTR (sets FUSE_ATTR_DAX on all files), OPEN, READ, WRITE, SETUPMAPPING, REMOVEMAPPING, FORGET/BATCH_FORGET (no-ops)
- SETUPMAPPING: writes known byte pattern to DAX window at requested offset (different content than FUSE_READ returns)
- Writable synthetic files: daemon detects guest writes to DAX window regions and updates in-memory file content
- DEVICE_STATE backend: serialize/deserialize in-memory file table via pipe
- CLI: accepts `--socket-path` and `--shared-dir` (ignored, files are synthetic)

**Dependencies:** Phases 3, 5 (needs to exercise DAX window + DEVICE_STATE)

**Done when:** Daemon starts, accepts vhost-user connection, serves synthetic files, handles SETUPMAPPING by writing to DAX window, supports guest writes to DAX-mapped regions, handles DEVICE_STATE save/load. Covers vhost-user-fs-dax.AC5.1 through AC5.6.
<!-- END_PHASE_7 -->

<!-- START_PHASE_8 -->
### Phase 8: Integration Tests

**Goal:** End-to-end verification of virtio-fs over vhost-user with DAX.

**Components:**
- `tests/test_cases/src/test_vhost_user_fs.rs` — test cases using `#[host]`/`#[guest]` proc macros
- **DAX read test:** Host spawns test daemon, configures VM with `add_virtiofs_vhost_user("testfs", socket, Some(32))`, starts VM. Guest mounts `virtiofs testfs /mnt`, reads file, verifies DAX-specific byte pattern (not FUSE_READ content), confirming DAX is active.
- **DAX write test:** Guest writes a known byte pattern to a DAX-mapped file, reads back via DAX, verifies the written content persists.
- **Snapshot/restore test:** Host spawns daemon, starts VM, guest mounts and reads via DAX, host triggers snapshot, kills daemon, restarts daemon, restores VM, guest verifies DAX content survived restore.
- Test registration in `tests/test_cases/src/lib.rs`

**Dependencies:** Phases 4, 6, 7

**Done when:** All tests pass with `make test FEATURE_FLAGS="--features embedded_init,vhost-user"`. Covers vhost-user-fs-dax.AC6.1 through AC6.5.
<!-- END_PHASE_8 -->

## Additional Considerations

**Daemon availability contract:** The daemon must be running and listening on the Unix socket before VM start and before restore. This is the caller's responsibility. The VMM does not manage daemon lifecycle.

**DAX window sizing:** The DAX window is divided by the kernel into 2MB chunks (`FUSE_DAX_SZ`). A 32MB window gives 16 mapping slots. Reasonable defaults: 256MB for production, 32MB for testing. `None` disables DAX entirely (all I/O through virtqueue).

**DEVICE_STATE upstream path:** The custom DEVICE_STATE implementation should be contributed to the rust-vmm `vhost` crate once validated. The protocol messages follow the QEMU vhost-user specification (messages 42 and 43, protocol feature bit 19).

**Kernel requirements:** Guest kernel must be v5.4+ for basic virtio-fs, v6.2+ for per-file DAX (`FUSE_HAS_INODE_DAX`, protocol v7.36). The `dax=inode` mount option enables per-file mode.
