# Test Requirements: vhost-user-fs-dax

## AC1: VhostUserDevice generic wrapper works

| ID | Description | Phase | Test Type | Test/Verification |
|---|---|---|---|---|
| AC1.1 | VhostUserDevice connects to a vhost-user daemon over a Unix socket and completes feature negotiation | Phase 2 | Integration test | Phase 8 `vhost-user-fs-dax-read` test (end-to-end daemon connection); unit-level construction path exercised by `test_new_fails_with_unavailable_socket` (negative path) |
| AC1.2 | VhostUserDevice shares memfd-backed guest memory with daemon via SET_MEM_TABLE | Phase 2 | Integration test | Phase 8 `vhost-user-fs-dax-read` test (daemon receives guest memory, processes FUSE requests proving SET_MEM_TABLE succeeded) |
| AC1.3 | VhostUserDevice returns error when daemon socket is unavailable | Phase 2 | Unit test | `test_new_fails_with_unavailable_socket` in `src/devices/src/virtio/vhost_user/fs.rs` — calls `VhostUserFs::new("tag", "/tmp/nonexistent-socket-path-12345", None)`, asserts error is returned |
| AC1.4 | Existing tests pass without regression after PR #527 integration | Phase 1 | Unit test (regression) | `cargo test -p devices --features net,snapshot` and `cargo test -p vmm --features snapshot` pass without failures |

## AC2: VhostUserFs device exposes correct virtio-fs identity

| ID | Description | Phase | Test Type | Test/Verification |
|---|---|---|---|---|
| AC2.1 | device_type() returns 26 (VIRTIO_ID_FS) | Phase 2 | Unit test | `test_device_type_is_fs` in `src/devices/src/virtio/vhost_user/fs.rs` — constructs via `new_for_test()`, asserts `device_type() == 26` |
| AC2.2 | Config space contains filesystem tag and num_request_queues from daemon (via VHOST_USER_GET_CONFIG) | Phase 2 | Unit test | `test_read_config_tag` and `test_read_config_num_queues` in `src/devices/src/virtio/vhost_user/fs.rs` — constructs with known config, reads back via `read_config()` at correct offsets |
| AC2.3 | Queue layout has HPQ (queue 0) + N request queues, each with 1024 descriptors | Phase 3 | Unit test | `test_queue_config_hpq_plus_request_queues` in `src/devices/src/virtio/vhost_user/fs.rs` — constructs with `num_request_queues=3`, verifies 4 queue entries each with `max_size=1024` |
| AC2.4 | shm_region() returns VirtioShmRegion with SHM region ID 0 when DAX configured | Phase 3 | Unit test | `test_shm_region_some_with_dax` in `src/devices/src/virtio/vhost_user/fs.rs` — constructs with `dax_window_mib=Some(32)`, calls `set_shm_region()`, asserts `shm_region()` returns the region |
| AC2.5 | shm_region() returns None when dax_window_mib is None | Phase 3 | Unit test | `test_shm_region_none_without_dax` in `src/devices/src/virtio/vhost_user/fs.rs` — constructs with `dax_window_mib=None`, asserts `shm_region()` is None |
| AC2.6 | DAX window memfd shared with daemon as additional region via ADD_MEM_REGION (CONFIGURE_MEM_SLOTS) | Phase 3 | Integration test | Phase 8 `vhost-user-fs-dax-read` test — daemon writes 0xBB to DAX window via SETUPMAPPING handler, which only works if ADD_MEM_REGION successfully shared the memfd; also verified by `test_queue_config_hpq_plus_request_queues` |

## AC3: Builder API configures the device

| ID | Description | Phase | Test Type | Test/Verification |
|---|---|---|---|---|
| AC3.1 | `add_virtiofs_vhost_user(tag, socket_path, Some(32))` results in a bootable VM with the device visible to the guest kernel | Phase 4 | Integration test | Phase 8 `vhost-user-fs-dax-read` test — guest mounts the virtiofs filesystem proving the device was visible and functional |
| AC3.2 | `add_virtiofs_vhost_user(tag, socket_path, None)` configures device without DAX window | Phase 4 | Integration test | Phase 8 `vhost-user-fs-dax-read` test variant without DAX (or manual verification: configure VM with `None`, confirm no SHM capability region is advertised) |
| AC3.3 | Tag longer than 36 bytes is rejected | Phase 4 | Unit test | Unit test in `src/libkrun/src/lib.rs` — calls `add_virtiofs_vhost_user` with 37-byte tag, asserts `Err(StartError::TagTooLong(37))` is returned |
| AC3.4 | Coexists with existing direct FUSE virtio-fs device (both can be configured on same VM) | Phase 4 | Unit test | Unit test in `src/libkrun/src/lib.rs` — constructs Builder, calls both `add_virtiofs("fs1", "/shared")` and `add_virtiofs_vhost_user("vhostfs", "/tmp/sock", Some(32))`, verifies both configs are stored in VmResources without conflict |

## AC4: Snapshot/restore preserves device and daemon state

| ID | Description | Phase | Test Type | Test/Verification |
|---|---|---|---|---|
| AC4.1 | SET_DEVICE_STATE_FD (msg 42) transfers daemon state to VMM via pipe | Phase 5 | Integration test | Phase 8 `vhost-user-fs-dax-snapshot` test — full snapshot cycle exercises the pipe-based transfer; unit-level negative path covered by `test_save_device_state_not_supported` in `src/devices/src/virtio/vhost_user/device.rs` |
| AC4.2 | CHECK_DEVICE_STATE (msg 43) confirms daemon finished state transfer | Phase 5 | Integration test | Phase 8 `vhost-user-fs-dax-snapshot` test — snapshot succeeds only if CHECK_DEVICE_STATE returns success; unit-level negative path covered by `test_load_device_state_not_supported` in `src/devices/src/virtio/vhost_user/device.rs` |
| AC4.3 | Snapshot captures vring bases (GET_VRING_BASE), device config, and daemon state blob | Phase 6 | Unit test | `test_snapshot_state_roundtrip` in `src/devices/src/virtio/vhost_user/fs.rs` — constructs VhostUserFsState with test data, serializes with bincode, deserializes, verifies all fields match |
| AC4.4 | Restore reconnects to daemon, re-shares memory + DAX window, restores vring bases and daemon state | Phase 6 | Integration test | Phase 8 `vhost-user-fs-dax-snapshot` test — daemon is restarted between snapshot and restore; post-restore DAX read succeeds proving full restore path ran correctly |
| AC4.5 | DAX window contents survive snapshot/restore (via daemon state restore + cache re-population) | Phase 6 | Integration test | Phase 8 `vhost-user-fs-dax-snapshot` test — post-restore `fs::read("/mnt/testfs/hello.txt")` returns 0xBB, proving daemon state was restored and the kernel re-faulted DAX pages correctly |
| AC4.6 | Restore fails gracefully when daemon is not running at socket path | Phase 6 | Unit test | `test_restore_backend_state_stores_pending` and `test_activate_restore_fails_when_daemon_unavailable` in `src/devices/src/virtio/vhost_user/fs.rs` — constructs with `pending_restore_state` pointing to non-existent socket, calls `activate()`, asserts `ActivateError` is returned |

## AC5: Test daemon serves synthetic filesystem with DAX

| ID | Description | Phase | Test Type | Test/Verification |
|---|---|---|---|---|
| AC5.1 | Daemon accepts vhost-user connection and negotiates FUSE_INIT with MAP_ALIGNMENT and HAS_INODE_DAX | Phase 7 | Integration test | Phase 8 `vhost-user-fs-dax-read` test — guest mount succeeds and DAX content is readable, which requires successful FUSE_INIT negotiation including HAS_INODE_DAX |
| AC5.2 | LOOKUP/GETATTR responses set FUSE_ATTR_DAX on files | Phase 7 | Integration test | Phase 8 `vhost-user-fs-dax-read` test — guest reads 0xBB (DAX pattern) rather than 0xAA (FUSE_READ pattern), proving kernel issued SETUPMAPPING which only happens when FUSE_ATTR_DAX was set |
| AC5.3 | SETUPMAPPING writes known byte pattern to DAX window at requested offset | Phase 7 | Integration test | Phase 8 `vhost-user-fs-dax-read` test — guest reads 0xBB from `hello.txt`, which is the pattern written by the daemon's SETUPMAPPING handler |
| AC5.4 | FUSE_READ returns different content than DAX path (allows guest to distinguish) | Phase 7 | Integration test | Phase 8 `vhost-user-fs-dax-read` test — diagnostic output distinguishes 0xAA (FUSE_READ fallback) from 0xBB (DAX path); 0xBB proves DAX was active rather than FUSE_READ |
| AC5.5 | DEVICE_STATE save/load round-trips the in-memory file table | Phase 7 | Integration test | Phase 8 `vhost-user-fs-dax-snapshot` test — daemon is killed and restarted with fresh state, then restore loads saved state; post-restore file reads return correct content proving the file table was round-tripped |
| AC5.6 | Daemon observes guest writes to DAX window (file content updated in synthetic filesystem) | Phase 7 | Integration test | Phase 8 `vhost-user-fs-dax-write` test — guest writes 0xCC via DAX, reads back 0xCC, which works only if the daemon's `sync_dax_writes()` path correctly observes guest writes |

## AC6: End-to-end integration tests pass

| ID | Description | Phase | Test Type | Test/Verification |
|---|---|---|---|---|
| AC6.1 | Guest mounts virtiofs, reads file, receives DAX-specific byte pattern (proving DAX active, not FUSE_READ fallback) | Phase 8 | Integration test | `vhost-user-fs-dax-read` in `tests/test_cases/src/test_vhost_user_fs.rs` — guest asserts `data.iter().all(|&b| b == 0xBB)` after mounting with `dax=inode` |
| AC6.2 | Guest writes to a DAX-mapped file, reads back via DAX, and verifies the written content persists | Phase 8 | Integration test | `vhost-user-fs-dax-write` in `tests/test_cases/src/test_vhost_user_fs.rs` — guest writes 0xCC, reads back, asserts all bytes are 0xCC |
| AC6.3 | Snapshot/restore test: mount + DAX read, snapshot, daemon restart, restore, DAX content survives | Phase 8 | Integration test | `vhost-user-fs-dax-snapshot` in `tests/test_cases/src/test_vhost_user_fs.rs` — host snapshots VM after guest signals READY, restarts daemon, restores; guest verifies 0xBB pattern post-restore |
| AC6.4 | Tests pass with `make test FEATURE_FLAGS="--features embedded_init,vhost-user"` | Phase 8 | Build/CI verification | `make test FEATURE_FLAGS="--features embedded_init,vhost-user"` — all three vhost-user-fs tests (`dax-read`, `dax-write`, `dax-snapshot`) pass alongside existing tests |
| AC6.5 | Tests use `#[host]`/`#[guest]` proc macro framework consistent with existing test patterns | Phase 8 | Code review | Inspect `tests/test_cases/src/test_vhost_user_fs.rs` — `#[host]` and `#[guest]` attributes on `impl Test` blocks for all three test structs match the pattern in `test_snapshot_restore.rs` and `test_custom_block_backend.rs` |
