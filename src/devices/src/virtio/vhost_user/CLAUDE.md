# Vhost-User Devices

Last verified: 2026-03-01

## Purpose
Vhost-user frontend support for libkrun. Delegates virtio device I/O to external daemon processes over the vhost-user protocol, enabling device isolation and flexibility. Provides VhostUserFs (virtio-fs with optional DAX window) and VhostUserVsock (vsock via vhost-user backend).

## Contracts
- **Exposes**: `VhostUserDevice` (generic wrapper), `VhostUserFs` (filesystem-specific), `VhostUserVsock` (vsock-specific)
- **Guarantees**:
  - `VhostUserDevice::new` connects to daemon, negotiates features/protocol features, auto-detects queue count via MQ protocol when `num_queues=0`
  - `VhostUserDevice::from_stream` accepts a pre-connected `UnixStream` (fd-provisioned by orchestrator); same negotiation as `new`
  - When MQ protocol feature is negotiated with explicit queue count (`num_queues > 0`), `get_queue_num` is still called to update the frontend's internal `max_queue_num` (without this, queue index > 0 is rejected)
  - `VHOST_USER_F_PROTOCOL_FEATURES` (bit 30) is stripped from guest-visible features (backend-only)
  - `save_device_state` / `load_device_state` use DEVICE_STATE protocol (pipe-based transfer); return `Err(InvalidInput)` if DEVICE_STATE not negotiated
  - `add_mem_region` requires CONFIGURE_MEM_SLOTS protocol feature; returns `Err(InvalidInput)` otherwise
  - `VhostUserFs::new` validates tag <= 36 bytes; fetches config from daemon via `get_config`; creates DAX memfd if `dax_window_mib` is Some
  - VhostUserFs queue layout: 1 HPQ + `num_request_queues` request queues, each size 1024
  - `shm_region()` returns `Some` only when DAX is configured and `set_shm_region` was called
  - `VhostUserVsock::new(socket_path)` connects to daemon, fetches `guest_cid` via `get_config` (8-byte LE u64)
  - `VhostUserVsock::from_stream(stream)` accepts pre-connected `UnixStream`; `socket_path` is None (restore not supported for fd-based connections)
  - VhostUserVsock queue layout: 3 queues (RX, TX, Event), each size 256; device type 19
  - VhostUserVsock config space: 8-byte LE `guest_cid`; `write_config` is a no-op (read-only)
  - Snapshot (both Fs and Vsock): `save_backend_state` stops vrings via `get_vring_base`, saves daemon state via DEVICE_STATE, serializes state struct with bincode
  - Restore (both Fs and Vsock): `restore_backend_state` stores pending state; `activate()` detects it and runs `activate_restore` (reconnect, re-negotiate, load daemon state)
  - Restore fails with `ActivateError::BadActivate` if daemon is unavailable at restore time
- **Expects**: Running vhost-user daemon at socket path (or pre-connected stream); guest memory with file backing (memfd) for `set_mem_table`

## Dependencies
- **Uses**: `vhost` crate (vendored, patched 0.15.0 with DEVICE_STATE protocol), `vm-memory`, `nix` (pipe), `bincode` (snapshot serialization)
- **Used by**: `vmm::builder` (attaches to MMIO bus), `libkrun` (configures via Builder API)
- **Boundary**: `vhost-user` feature flag gates this entire module; `snapshot` feature gates save/restore

## Key Decisions
- Generic `VhostUserDevice` handles protocol negotiation and vring setup; device-specific wrappers add config, queue layout, and snapshot logic
- Single interrupt monitor thread per device (all vrings share one `vring_call_event`)
- DAX window is a separate memfd, NOT part of `GuestMemoryMmap`; registered as additional KVM memory slot by vmm builder
- `reconnect_for_restore` re-negotiates with daemon using saved features intersected with current daemon capabilities (handles daemon restart)
- `VhostUserFsState` and `VhostUserVsockState` snapshot structs store vring bases, daemon state blob, features; Vsock also stores `socket_path` for reconnection
- `VhostUserVsock::from_stream` sets `socket_path = None`; restore for fd-based connections is not yet supported (orchestrator must provide new connection)

## Invariants
- `activate_vhost_user` translates virtio queue GPAs to VMM VAs before passing to daemon
- Only file-backed (memfd) memory regions are shared with the daemon via `set_mem_table`
- `get_vring_base` is the quiesce mechanism (stops daemon from processing that vring)
- `pending_restore_state` is consumed (taken) by `activate()` -- only used once
- VhostUserVsock `activate()` copies queue state to device-local `queues` vec (MmioTransport reads `device.queues()`)

## Key Files
- `device.rs` - `VhostUserDevice`: generic vhost-user wrapper, VirtioDevice impl, protocol negotiation, `from_stream`, DEVICE_STATE save/load
- `fs.rs` - `VhostUserFs`: filesystem device (type 26), DAX memfd, config space, snapshot/restore, queue layout
- `vsock.rs` - `VhostUserVsock`: vsock device (type 19), guest_cid config, 3-queue layout, snapshot/restore via `VhostUserVsockState`
- `mod.rs` - Module declarations and re-exports
