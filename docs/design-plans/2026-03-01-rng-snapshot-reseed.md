# RNG Snapshot Reseed Design

## Summary

When a VM is created by restoring the same snapshot multiple times — a common pattern for scaling identical workloads — every clone starts with the same kernel random number generator state. Unless the guest receives fresh entropy before it begins executing, reads from `/dev/urandom` or any CSPRNG-derived source will return identical bytes across all clones, undermining any cryptographic operation that happens early in the guest's lifetime.

This document describes a minimal, host-driven fix: a new `on_restore_complete()` hook on the `VirtioDevice` trait that the virtio-rng device overrides to drain any pending guest entropy requests and fill them with bytes from the host OS's RNG immediately after restore, before vCPUs resume. No guest driver changes, new virtio feature bits, or ACPI mechanisms are required. The implementation adds one call site in `MmioTransport::complete_restore()`, one trait method override in the `Rng` device, and a `Snapshottable` impl so that the device's negotiated feature flags survive the snapshot/restore cycle. An integration test validates the fix by restoring the same snapshot into two VMs and asserting their `/dev/urandom` outputs differ.

## Definition of Done

When a libkrun VM is restored from snapshot (cloned), the virtio-rng device proactively serves fresh host entropy through the existing virtqueue immediately after restore completes, causing the guest kernel CSPRNG to reseed and diverge from any other VM restored from the same snapshot.

**Success criteria:**
- After restore, the rng device processes pending virtqueue requests before the guest resumes
- Fresh entropy comes from the host OS (not cached/snapshotted values)
- The rng device has a proper `Snapshottable` impl so queue state survives restore correctly
- Integration test: restore the same snapshot twice, read from a randomness source in each VM, verify the outputs differ
- No new virtio feature bits, no ACPI, no guest-side changes required

**Out of scope:**
- VMGENID / ACPI-based approach
- New virtio feature bit negotiation
- Guest driver changes

## Acceptance Criteria

### rng-snapshot-reseed.AC1: Fresh entropy is delivered on restore
- **rng-snapshot-reseed.AC1.1 Success:** After snapshot restore, the rng device serves all pending virtqueue entropy requests with fresh host bytes before vCPUs resume
- **rng-snapshot-reseed.AC1.2 Success:** Guest reads valid random bytes after restore with no stall or empty response

### rng-snapshot-reseed.AC2: RNG state diverges across clones
- **rng-snapshot-reseed.AC2.1 Success:** Two VMs restored sequentially from the same snapshot produce different 32-byte strings read from `/dev/urandom`
- **rng-snapshot-reseed.AC2.2 Failure (baseline):** Without `on_restore_complete()`, both restores produce identical bytes — integration test must demonstrate this failure before the fix is applied

### rng-snapshot-reseed.AC3: Rng device snapshots correctly
- **rng-snapshot-reseed.AC3.1 Success:** `acked_features` are preserved across a snapshot/restore cycle
- **rng-snapshot-reseed.AC3.2 Success:** Rng device activates and serves entropy normally in a fresh (non-restore) VM startup

### rng-snapshot-reseed.AC4: No regression to other devices
- **rng-snapshot-reseed.AC4.1 Success:** `on_restore_complete()` default no-op causes no behavior change in any existing `VirtioDevice` implementor

## Glossary

- **virtio**: A standardized interface for paravirtual I/O devices. Guest drivers communicate with host-side device backends through a shared-memory protocol rather than emulating real hardware registers.
- **virtio-rng**: A virtio device type (device ID 4) that exposes the host's random number generator to the guest. The guest driver submits buffers to a virtqueue; the host fills them with entropy and returns them.
- **virtqueue**: The shared-memory ring buffer used by virtio devices. It has three parts: a descriptor table (buffer addresses and lengths), an available ring (guest-to-host notifications of new requests), and a used ring (host-to-guest responses with filled buffers).
- **available ring / used ring**: Halves of a virtqueue. The guest places buffer descriptors into the available ring; the host moves them to the used ring after processing, triggering an interrupt.
- **`VirtioDevice` trait**: The Rust trait all virtio device implementations satisfy. It defines lifecycle methods (`activate`, `reset`), feature negotiation, and queue configuration. `MmioTransport` drives devices through this interface.
- **`MmioTransport`**: The MMIO transport layer that mediates between the KVM hypervisor's MMIO exits and a `VirtioDevice`. It handles queue setup, feature negotiation, and the restore sequence.
- **`complete_restore()`**: The method on `MmioTransport` called during snapshot restore to re-activate a device with its recovered queue and memory state, then signal the guest before vCPUs resume.
- **`Snapshottable`**: A trait (behind the `snapshot` feature flag) that devices implement to serialize and deserialize their private state (e.g., negotiated features) alongside the transport-level queue state.
- **`acked_features`**: The set of virtio feature bits that both the host device and guest driver have agreed to use, established during feature negotiation. Must be preserved across snapshot/restore so the device behaves consistently after resume.
- **CSPRNG**: Cryptographically Secure Pseudo-Random Number Generator. The kernel's random number generator used to service `/dev/urandom` and `getrandom()`. It is seeded from entropy sources; virtio-rng is one such source via `add_hwgenerator_randomness()`.
- **`OsRngBackend`**: The production `RngBackend` implementation that draws bytes from the host OS's RNG. This is the source of fresh entropy injected on restore.
- **`process_req()`**: The `Rng` device method that pops all descriptors from the available ring, fills each buffer with bytes from the backend, and appends entries to the used ring.
- **`signal_used_queue()`**: Raises the interrupt line that notifies the guest driver that the host has finished processing one or more virtqueue entries.
- **`post_restore_kick()`**: A method called after restore that writes to each queue's eventfd, waking background worker threads to process any pending queue activity.
- **snapshot/restore**: The mechanism by which a VM's full state (vCPU registers, guest memory, device state) is saved to disk and later reloaded — potentially into multiple independent VM instances (cloning).
- **VMGENID**: A Windows/ACPI-defined mechanism that exposes a unique ID to the guest, changed on each restore, so the guest OS can detect cloning and reseed its RNG. Explicitly out of scope for this design.
- **RED/GREEN phases**: A TDD structure where Phase 1 (RED) writes a failing test confirming the bug exists, and Phase 2 (GREEN) implements the fix that makes it pass.

## Architecture

When a VM is cloned by restoring the same snapshot twice, both instances start with identical kernel RNG state. The Linux virtio-rng driver feeds into the kernel entropy pool via `add_hwgenerator_randomness()`, so injecting fresh host entropy through the virtio-rng virtqueue causes the guest CSPRNG to reseed and diverge.

The restore sequence in `MmioTransport::complete_restore()` already activates devices, calls `post_restore_kick()`, and signals the used queue interrupt — all before vCPUs resume. This is the right place to inject entropy: synchronously, after activation, before the guest wakes.

Three components change:

**`VirtioDevice` trait** (`src/devices/src/virtio/device.rs`): new default no-op method called after activation during restore.

```rust
fn on_restore_complete(&mut self) {}
```

**`MmioTransport::complete_restore()`** (`src/devices/src/virtio/mmio.rs`): one call inserted after `device.activate()`:

```rust
device.activate(mem, interrupt, queues)?;
device.on_restore_complete();   // new
self.post_restore_kick();
self.interrupt.signal_used_queue();
```

**`Rng` device** (`src/devices/src/virtio/rng/device.rs`): overrides the hook to drain the available ring with fresh host entropy and signal the used queue. Also gains a `Snapshottable` impl (behind `#[cfg(feature = "snapshot")]`) that saves and restores `acked_features`.

```rust
// VirtioDevice impl
fn on_restore_complete(&mut self) {
    if self.process_req() {
        self.device_state.signal_used_queue();
    }
}

// Snapshottable contract
struct RngState { acked_features: u64 }
```

Data flow on restore: guest memory restored → queue ring pointers restored by `MmioTransport` → `activate()` called → `on_restore_complete()` drains available ring, fills used ring with fresh `OsRngBackend` bytes → `signal_used_queue()` wakes guest → guest reads fresh entropy from used ring.

## Existing Patterns

`MmioTransport::Snapshottable` already saves and restores all virtio queue state (desc_table address, avail_ring address, used_ring address, next_avail, next_used). The `Rng` Snapshottable impl only needs to handle device-specific fields (`acked_features`), following the same pattern as cloud-hypervisor's `RngState`.

The `on_restore_complete()` hook mirrors QEMU's `virtio_rng_vm_state_change` handler, which calls `virtio_rng_process()` when the VM transitions to running after a restore.

The integration test follows the structure of `tests/test_cases/src/test_snapshot_net.rs`: host and guest sides communicate via vsock, host drives snapshot/restore sequencing, guest reports results. The `#[host]`/`#[guest]` proc macro split and `snapshot` feature flag gate are used identically.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: Failing Integration Test (RED)

**Goal:** Establish a test that detects missing reseed behavior, confirming it will catch the real bug.

**Components:**
- `tests/test_cases/src/test_snapshot_rng_reseed.rs` — new test case
  - Host: builds VM with rng device, takes snapshot, restores twice from same snapshot, collects 32 bytes from guest each time via vsock, asserts they differ
  - Guest: reads 32 bytes from `/dev/urandom`, sends via vsock, exits

**Dependencies:** None (snapshot test infrastructure already exists)

**Done when:** Test compiles, runs under `make test FEATURE_FLAGS="--features embedded_init,snapshot"`, and **fails** — both restores produce identical bytes, confirming no reseed occurs without the fix
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: Reseed Implementation (GREEN)

**Goal:** Deliver fresh entropy to the guest on every snapshot restore, making the Phase 1 test pass.

**Components:**
- `src/devices/src/virtio/device.rs` — add `fn on_restore_complete(&mut self) {}` default method to `VirtioDevice` trait
- `src/devices/src/virtio/mmio.rs` — call `device.on_restore_complete()` in `complete_restore()` after `device.activate()`
- `src/devices/src/virtio/rng/device.rs` — override `on_restore_complete()` to call `process_req()` and signal used queue; add `Snapshottable` impl saving/restoring `RngState { acked_features: u64 }` behind `#[cfg(feature = "snapshot")]`

**Dependencies:** Phase 1 (failing test exists)

**Done when:** Phase 1 integration test passes — two restores from the same snapshot produce different 32-byte random strings; covers `rng-snapshot-reseed.AC1.1`, `rng-snapshot-reseed.AC2.1`
<!-- END_PHASE_2 -->
