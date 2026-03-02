# Vhost-User-VSock Design

## Summary

Vhost-user-vsock adds a new virtio-vsock device backend to libkrun where all vsock packet processing is offloaded to an external proxy process instead of being handled inside the VMM. The proxy connects to libkrun over a Unix socket provisioned by the orchestrator, and the two sides communicate using the vhost-user protocol: libkrun shares guest memory and sets up vring notifications, while the proxy reads and writes virtqueue buffers directly, parsing vsock packets and handling TSI routing without any VMM involvement. The guest sees a standard virtio-vsock device and requires no kernel changes.

The implementation follows the existing vhost-user-fs pattern in the codebase. A generic `VhostUserDevice` layer handles Unix socket connection, feature negotiation, memory region sharing, and snapshot state transfer via the DEVICE_STATE protocol. A new vsock-specific wrapper, `VhostUserVsock`, supplies the vsock device type, three-queue layout (RX, TX, Event), and config space. A new `from_stream()` constructor on `VhostUserDevice` supports connections via a pre-provisioned file descriptor rather than a socket path, which allows the orchestrator to establish the channel before handing the fd to libkrun — keeping the proxy isolated to its own network namespace with no host filesystem access required.

## Definition of Done
- libkrun supports vhost-user-vsock as a vsock backend option, following the existing vhost-user-fs pattern
- The vhost-user master side runs in libkrun; an external proxy binary acts as the vhost-user backend and handles TSI directly over virtio-vsock shared memory
- Orchestrator provisions the Unix socket between libkrun and proxy — proxy needs no other host access beyond its netns and enclave fd
- `krun_add_vsock_port` API is preserved for now
- Guest-side protocol is unchanged — no kernel patches needed in libkrunfw
- Integration test: a separate test proxy binary performs TSI connect + send/recv over vhost-user-vsock, proving the end-to-end path works

## Acceptance Criteria

### vhost-user-vsock.AC1: VhostUserVsock device activates and connects to backend
- **vhost-user-vsock.AC1.1 Success:** VhostUserVsock connects to backend via socket path, negotiates features, exposes correct guest_cid in config space
- **vhost-user-vsock.AC1.2 Success:** VhostUserVsock connects via pre-provisioned fd (OwnedFd), same feature negotiation and config behavior
- **vhost-user-vsock.AC1.3 Success:** Device reports VIRTIO_ID_VSOCK (19) as device type and configures 3 queues (RX, TX, Event)
- **vhost-user-vsock.AC1.4 Failure:** Connection to non-existent socket path returns error
- **vhost-user-vsock.AC1.5 Failure:** Backend that doesn't support required protocol features returns error during negotiation

### vhost-user-vsock.AC2: API enforces mutual exclusivity with userspace vsock
- **vhost-user-vsock.AC2.1 Success:** add_vsock_vhost_user() configures VM with vhost-user-vsock when no userspace vsock is configured
- **vhost-user-vsock.AC2.2 Success:** add_vsock_vhost_user_fd() configures VM with pre-provisioned fd
- **vhost-user-vsock.AC2.3 Failure:** Calling both krun_add_vsock() and add_vsock_vhost_user() returns error
- **vhost-user-vsock.AC2.4 Failure:** Calling both add_vsock_vhost_user() and krun_add_vsock() returns error (either order)

### vhost-user-vsock.AC3: Guest can communicate with backend over vsock
- **vhost-user-vsock.AC3.1 Success:** Guest connects to backend via AF_VSOCK, sends data, receives response
- **vhost-user-vsock.AC3.2 Success:** Multiple concurrent vsock connections work simultaneously

### vhost-user-vsock.AC4: Snapshot and restore preserves state
- **vhost-user-vsock.AC4.1 Success:** VhostUserVsock state is saved (vring bases + backend state blob via DEVICE_STATE)
- **vhost-user-vsock.AC4.2 Success:** Restored VM reconnects to fresh backend via new provisioned fd, resumes vsock communication
- **vhost-user-vsock.AC4.3 Success:** Backend internal state (counter) survives snapshot/restore cycle — post-restore value continues from pre-snapshot
- **vhost-user-vsock.AC4.4 Failure:** Restore fails cleanly if backend is unavailable at restore time

### vhost-user-vsock.AC5: Backward compatibility
- **vhost-user-vsock.AC5.1 Success:** Existing userspace vsock (krun_add_vsock) continues to work unchanged
- **vhost-user-vsock.AC5.2 Success:** krun_add_vsock_port API is preserved and functional with userspace vsock
- **vhost-user-vsock.AC5.3 Success:** Guest kernel (libkrunfw) requires no changes — same TSI patches work with both backends

## Glossary

- **vhost-user**: A protocol that allows a VMM to delegate virtio device data-plane processing to an external backend process. The VMM retains control-plane responsibility (memory mapping, feature negotiation, vring setup) while the backend accesses guest memory and virtqueues directly via shared memory.
- **virtio-vsock**: A virtio device that provides host-guest socket communication using `AF_VSOCK` addresses. Uses three virtqueues: RX (host-to-guest), TX (guest-to-host), and Event.
- **TSI (Transparent Socket Impersonation)**: A libkrunfw kernel patch that intercepts `AF_INET`/`AF_INET6` socket calls inside the guest and transparently redirects them over `AF_VSOCK`, allowing guest apps to make network connections without a virtual NIC.
- **vhost-user master / backend**: The two sides of the vhost-user protocol. The master (libkrun) initiates the connection and owns control; the backend (external proxy) owns the data plane and processes virtqueue buffers.
- **DEVICE_STATE protocol**: An extension to the vhost-user protocol (patched in `vendor/vhost/`) that allows the VMM to request the backend to serialize and transfer its internal state for snapshot/restore.
- **guest_cid**: The Context Identifier assigned to the VM for vsock addressing. Embedded in the vsock device's config space and used by the guest to identify itself.
- **vring_call / irqfd**: File descriptors for interrupt signaling. The backend writes to `vring_call` to notify the guest of new data; KVM translates this into a virtual interrupt via `irqfd`.
- **vring base**: The current head index of a virtqueue, saved during snapshot to allow replay from the correct position on restore.
- **VIRTIO_VSOCK_F_DGRAM**: A virtio-vsock feature bit that advertises datagram socket support. Required for TSI's use of `SOCK_DGRAM` control messages on ports 1024-1031.
- **VhostUserBackendMut**: A trait from the `vhost-user-backend` crate (rust-vmm) that a backend process implements to handle virtqueue processing callbacks.
- **libkrunfw**: The custom Linux kernel image used as the guest OS in libkrun VMs. Contains the TSI patches.
- **orchestrator**: The external system (e.g., holodeck) that provisions resources for a VM before startup, including the Unix socket connecting libkrun to the vsock proxy.

## Architecture

Vhost-user-vsock adds a new virtio-vsock device backed by the vhost-user protocol, following the same generic-specific pattern used by vhost-user-fs.

**Generic layer** — `VhostUserDevice` in `src/devices/src/virtio/vhost_user/device.rs` (already exists). Handles Unix socket connection, feature negotiation, memory region sharing, vring setup, interrupt monitoring, and DEVICE_STATE save/load.

**Specific layer** — new `VhostUserVsock` in `src/devices/src/virtio/vhost_user/vsock.rs`. Wraps `VhostUserDevice` with:
- Device type `VIRTIO_ID_VSOCK` (19)
- 3 queues: RX, TX, Event (per virtio-vsock spec)
- Config space: `virtio_vsock_config { guest_cid: u64 }` fetched from backend via `get_config()`
- Snapshot state: `VhostUserVsockState` with guest_cid, features, vring bases, backend state blob
- No DAX window (unlike fs)

**Connection modes** — `VhostUserDevice` currently only supports connecting via socket path. A new `from_stream()` constructor accepts a pre-connected `UnixStream` (from an fd provisioned by the orchestrator). Both paths converge after connection establishment.

**Data flow**:
```
Guest app → AF_TSI intercept → virtio-vsock TX queue → (shared memory) →
  Backend reads TX virtqueue → parses vsock packet → handles TSI/routes traffic

Backend writes RX virtqueue → (shared memory) → irqfd →
  Guest receives interrupt → reads RX queue → app gets response
```

The VMM never sees vsock packet contents. It shares guest memory with the backend, sets up vring notifications, and monitors the vring_call eventfd for guest interrupts. All packet processing happens in the backend process.

**Coexistence with existing vsock** — the userspace virtio-vsock device (`src/devices/src/virtio/vsock/`) is unchanged. A VM is configured with either the userspace vsock (via `krun_add_vsock()`) or vhost-user-vsock (via `add_vsock_vhost_user()`). Both cannot be used simultaneously — the API enforces mutual exclusivity.

**Guest-side transparency** — the guest sees a standard virtio-vsock device regardless of backend. The existing TSI kernel patches in libkrunfw work unchanged because the virtio device interface is identical; only the host-side processing moves from in-VMM to external process.

## Existing Patterns

This design follows the vhost-user-fs pattern established in `src/devices/src/virtio/vhost_user/`:

- **Device wrapper**: `VhostUserVsock` wraps `VhostUserDevice` via composition (same as `VhostUserFs`)
- **Config struct**: `VhostUserVsockConfig` in `src/vmm/src/vmm_config/vhost_user_vsock.rs` (mirrors `VhostUserFsConfig`)
- **Resource storage**: `VmResources.vhost_user_vsock` field (mirrors `VmResources.vhost_user_fs`)
- **Builder attachment**: `attach_vhost_user_vsock_device()` in `src/vmm/src/builder.rs` (mirrors `attach_vhost_user_fs_device()`)
- **Public API**: `Builder::add_vsock_vhost_user()` in `src/libkrun/src/lib.rs` (mirrors `add_virtiofs_vhost_user()`)
- **Snapshot**: `VhostUserVsockState` serialized with bincode, quiescence via `get_vring_base()`, daemon state via DEVICE_STATE protocol (same as `VhostUserFsState`)
- **Feature gating**: Behind existing `vhost-user` feature flag, no new flag needed

**Divergence**: `VhostUserDevice::from_stream()` is new — the existing code only supports socket path connection. This is a backward-compatible addition to the generic wrapper, also usable by `VhostUserFs` in the future.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: VhostUserDevice from_stream Constructor

**Goal:** Enable VhostUserDevice to accept a pre-connected UnixStream, supporting fd-provisioned connections.

**Components:**
- `VhostUserDevice::from_stream()` in `src/devices/src/virtio/vhost_user/device.rs` — new constructor that accepts `UnixStream` instead of socket path, shares feature negotiation logic with `new()`

**Dependencies:** None

**Done when:** `VhostUserDevice` can be constructed from either a socket path or an existing `UnixStream`, with identical feature negotiation behavior.
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: VhostUserVsock Device

**Goal:** Implement the vsock-specific vhost-user device wrapper.

**Components:**
- `VhostUserVsock` in `src/devices/src/virtio/vhost_user/vsock.rs` — device wrapper with vsock config space, queue layout, VirtioDevice trait implementation
- Module registration in `src/devices/src/virtio/vhost_user/mod.rs`
- `VhostUserVsockConfig` in `src/vmm/src/vmm_config/vhost_user_vsock.rs`

**Dependencies:** Phase 1 (from_stream constructor)

**Done when:** `VhostUserVsock` can be constructed, connects to a vhost-user backend, negotiates vsock features, and exposes correct config space (guest_cid). Tests verify construction and config space reading with a mock backend.
<!-- END_PHASE_2 -->

<!-- START_PHASE_3 -->
### Phase 3: Builder and API Integration

**Goal:** Wire VhostUserVsock into VM construction and expose via the Rust Builder API.

**Components:**
- `VmResources.vhost_user_vsock` field in `src/vmm/src/resources.rs`
- `attach_vhost_user_vsock_device()` in `src/vmm/src/builder.rs`
- `Builder::add_vsock_vhost_user()` and `Builder::add_vsock_vhost_user_fd()` in `src/libkrun/src/lib.rs`
- Mutual exclusivity enforcement: error if both `krun_add_vsock()` and `add_vsock_vhost_user()` are called

**Dependencies:** Phase 2 (VhostUserVsock device)

**Done when:** A VM can be built with vhost-user-vsock configured via either socket path or provisioned fd. API rejects conflicting vsock configurations. Tests verify API usage and mutual exclusivity.
<!-- END_PHASE_3 -->

<!-- START_PHASE_4 -->
### Phase 4: Snapshot and Restore

**Goal:** Implement save/restore for vhost-user-vsock following the vhost-user-fs pattern.

**Components:**
- `VhostUserVsockState` in `src/devices/src/virtio/vhost_user/vsock.rs` — serializable snapshot state
- `save_backend_state()` / `restore_backend_state()` / `activate_restore()` on `VhostUserVsock`
- Restore reconnection via `add_vsock_vhost_user_fd()` (orchestrator provides new fd before restore)

**Dependencies:** Phase 3 (builder integration)

**Done when:** VhostUserVsock state can be saved (vring bases + backend state blob), and a new VM can restore from that state by reconnecting to a fresh backend. Tests verify state round-trip with a test backend that implements DEVICE_STATE.
<!-- END_PHASE_4 -->

<!-- START_PHASE_5 -->
### Phase 5: Test Proxy Binary

**Goal:** Build a minimal vhost-user-vsock backend for integration testing.

**Components:**
- Test proxy binary in `tests/test_vsock_proxy/` — implements `VhostUserBackendMut` trait from the vhost-user-backend crate
- Handles virtqueue setup, reads vsock packets from TX queue, echoes data back via RX queue on a test port
- Implements DEVICE_STATE protocol for snapshot testing (saves/restores an internal counter)

**Dependencies:** Phase 2 (needs a backend to validate against)

**Done when:** The test proxy binary starts, accepts a vhost-user connection, and can echo vsock packets. Counter state survives save/load via DEVICE_STATE protocol.
<!-- END_PHASE_5 -->

<!-- START_PHASE_6 -->
### Phase 6: Integration Test

**Goal:** End-to-end test proving vhost-user-vsock works with a real VM, including snapshot/restore.

**Components:**
- Integration test in `tests/test_cases/` using `#[host]`/`#[guest]` proc macros
- Host side: starts test proxy, boots VM with `add_vsock_vhost_user()`, triggers snapshot/restore
- Guest side: connects via `AF_VSOCK`, sends data, verifies echo, verifies state survives restore

**Dependencies:** Phase 3 (builder integration), Phase 4 (snapshot), Phase 5 (test proxy)

**Done when:** Integration test passes: guest sends data over vsock to test proxy, receives echo. VM is snapshotted and restored to a new proxy instance. Guest verifies the proxy's counter continued from pre-snapshot value.
<!-- END_PHASE_6 -->

## Additional Considerations

**Single vsock device limitation:** Linux guests support only one virtio-vsock device. With vhost-user-vsock, the backend process owns the entire vsock data plane. Services that need guest vsock access (TSI proxy, guest agent, VM service API) must either run in the backend process or communicate with it. The VMM does not handle vsock traffic. VMM-bound communication (e.g., virtiofs overlay mount commands from guest) must be forwarded by the backend via a Unix socket fd to the VMM. This routing architecture is a holodeck design concern, not a libkrun concern.

**SOCK_DGRAM:** The TSI protocol uses vsock datagrams for control messages (ports 1024-1031). The `vhost-device-vsock` crate from rust-vmm does not support DGRAM, but this is irrelevant — the holodeck proxy implements its own backend with direct virtqueue access, where DGRAM/STREAM distinction is just a type field in the `virtio_vsock_hdr`. The backend must advertise `VIRTIO_VSOCK_F_DGRAM` in its feature bits for the guest to use datagrams.

**`krun_add_vsock_port`:** Preserved in the API but only functional with the userspace vsock backend. When using vhost-user-vsock, port routing is the backend's responsibility. The API does not error — the config is stored but unused.
