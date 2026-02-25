# Vhost-User Devices

Last verified: 2026-02-25

## Purpose
Vhost-user frontend support for libkrun. Delegates virtio device I/O to external daemon processes over the vhost-user protocol, enabling device isolation and flexibility. Currently provides VhostUserFs (virtio-fs with optional DAX window).

## Contracts
- **Exposes**: `VhostUserDevice` (generic wrapper), `VhostUserFs` (filesystem-specific)
- **Guarantees**:
  - `VhostUserDevice::new` connects to daemon, negotiates features/protocol features, auto-detects queue count via MQ protocol when `num_queues=0`
  - `VHOST_USER_F_PROTOCOL_FEATURES` (bit 30) is stripped from guest-visible features (backend-only)
  - `save_device_state` / `load_device_state` use DEVICE_STATE protocol (pipe-based transfer); return `Err(InvalidInput)` if DEVICE_STATE not negotiated
  - `add_mem_region` requires CONFIGURE_MEM_SLOTS protocol feature; returns `Err(InvalidInput)` otherwise
  - `VhostUserFs::new` validates tag <= 36 bytes; fetches config from daemon via `get_config`; creates DAX memfd if `dax_window_mib` is Some
  - Queue layout: 1 HPQ + `num_request_queues` request queues, each size 1024
  - `shm_region()` returns `Some` only when DAX is configured and `set_shm_region` was called
  - Snapshot: `save_backend_state` stops vrings via `get_vring_base`, saves daemon state via DEVICE_STATE, serializes `VhostUserFsState` with bincode
  - Restore: `restore_backend_state` stores pending state; `activate()` detects it and runs `activate_restore` (reconnect, re-negotiate, load daemon state)
  - Restore fails with `ActivateError::BadActivate` if daemon is unavailable at restore time
- **Expects**: Running vhost-user daemon at socket path; guest memory with file backing (memfd) for `set_mem_table`

## Dependencies
- **Uses**: `vhost` crate (vendored, patched 0.14.0 with DEVICE_STATE protocol), `vm-memory`, `nix` (pipe), `bincode` (snapshot serialization)
- **Used by**: `vmm::builder` (attaches to MMIO bus), `libkrun` (configures via Builder API)
- **Boundary**: `vhost-user` feature flag gates this entire module; `snapshot` feature gates save/restore

## Key Decisions
- Generic `VhostUserDevice` handles protocol negotiation and vring setup; `VhostUserFs` adds FS-specific config, DAX, and snapshot logic
- Single interrupt monitor thread per device (all vrings share one `vring_call_event`)
- DAX window is a separate memfd, NOT part of `GuestMemoryMmap`; registered as additional KVM memory slot by vmm builder
- `reconnect_for_restore` re-negotiates with daemon using saved features intersected with current daemon capabilities (handles daemon restart)
- `VhostUserFsState` snapshot struct stores vring bases, daemon state blob, config, features

## Invariants
- `activate_vhost_user` translates virtio queue GPAs to VMM VAs before passing to daemon
- Only file-backed (memfd) memory regions are shared with the daemon via `set_mem_table`
- `get_vring_base` is the quiesce mechanism (stops daemon from processing that vring)
- `pending_restore_state` is consumed (taken) by `activate()` -- only used once

## Key Files
- `device.rs` - `VhostUserDevice`: generic vhost-user wrapper, VirtioDevice impl, protocol negotiation, DEVICE_STATE save/load
- `fs.rs` - `VhostUserFs`: filesystem device (type 26), DAX memfd, config space, snapshot/restore, queue layout
- `mod.rs` - Module declarations and re-exports
