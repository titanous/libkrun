# Snapshot Completeness Design

## Summary

libkrun's snapshot/restore system captures complete VM state — vCPU registers, RAM, and device registers — so a running VM can be paused, saved to disk, and resumed later or on another host. This is foundational for live migration: transferring a running VM between physical machines without stopping it.

The current implementation covers virtio devices and aarch64 legacy devices, but silently omits x86_64 and riscv64 legacy hardware devices (16550 UART serial port, i8042 keyboard controller, CMOS, and PL031 RTC). It also has several correctness gaps: virtio queue memory written by the VMM in userspace is not reflected in KVM's dirty page tracking (causing incremental snapshots to miss those pages), the guest clock is not notified of the time discontinuity introduced by a restore, and deserialization has no size bound against corrupted files. This design closes all of those gaps by implementing the `Snapshottable` trait on each missing device, wiring PortIO devices into the snapshot save/restore flow, marking virtio queue pages dirty after save, calling `kvmclock_ctrl()` after x86_64 vCPU restore, and adding a 10MB deserialization limit. The implementation follows established patterns already used by aarch64 devices and introduces no new abstractions.

## Definition of Done

Complete the snapshot/restore system so that no device state is silently lost during save/restore cycles, and the snapshot lifecycle correctly handles in-flight I/O, guest time consistency, and data integrity. This is foundational work for eventual live migration support on AWS (Linux/KVM, same-CPU constraint).

Specifically:

1. All legacy devices implement `Snapshottable` — 16550 Serial (x86_64, riscv64), i8042 (x86_64), CMOS (x86_64), PL031 RTC (aarch64). No device state is silently skipped during save/restore.
2. Virtio queue used ring and notification memory pages are marked dirty in the KVM dirty bitmap after snapshot save, so incremental snapshots capture userspace writes that KVM's dirty log misses.
3. `kvmclock_ctrl()` is called after x86_64 vCPU restore to prevent stolen-time reporting anomalies in the guest.
4. TSC frequency is saved in x86_64 `VcpuState` for diagnostics and future cross-host validation.
5. vmstate deserialization has a size limit (10MB) to prevent OOM on corrupted snapshot files.
6. Integration tests verify device state survives snapshot/restore from the guest perspective.

## Acceptance Criteria

### snapshot-completeness.AC1: Legacy device state survives snapshot/restore
- **snapshot-completeness.AC1.1 Success:** 16550 Serial save/restore round-trip preserves all register values (interrupt_enable, line_control, modem_control, scratch, baud_divisor)
- **snapshot-completeness.AC1.2 Success:** 16550 Serial save/restore preserves in_buffer FIFO contents (non-empty buffer)
- **snapshot-completeness.AC1.3 Success:** i8042 save/restore preserves status, control, output port, command, and buffer contents
- **snapshot-completeness.AC1.4 Success:** CMOS save/restore preserves index register and all 128 data bytes
- **snapshot-completeness.AC1.5 Success:** PL031 RTC save/restore preserves tick_offset, load, match_value, imsc, ris
- **snapshot-completeness.AC1.6 Success:** PortIO device states appear in VmSnapshot.device_states after full snapshot
- **snapshot-completeness.AC1.7 Failure:** Restoring a snapshot with corrupted device state bytes returns SnapshotError, does not panic
- **snapshot-completeness.AC1.8 Edge:** Restoring a snapshot missing a device entry (e.g. old snapshot without CMOS) leaves that device at construction defaults

### snapshot-completeness.AC2: Virtio queue memory marked dirty
- **snapshot-completeness.AC2.1 Success:** After snapshot save, used ring pages for all active virtio queues are marked dirty in KVM dirty bitmap
- **snapshot-completeness.AC2.2 Success:** Incremental snapshot after virtio I/O includes used ring pages in dirty page set
- **snapshot-completeness.AC2.3 Edge:** Inactive/unactivated queues are not marked (no crash on queues with zero addresses)

### snapshot-completeness.AC3: kvmclock_ctrl on x86_64 restore
- **snapshot-completeness.AC3.1 Success:** kvmclock_ctrl() is called after each vCPU restore on x86_64
- **snapshot-completeness.AC3.2 Failure:** kvmclock_ctrl() failure logs warning but does not fail the restore

### snapshot-completeness.AC4: TSC frequency saved
- **snapshot-completeness.AC4.1 Success:** x86_64 VcpuState includes tsc_khz after save
- **snapshot-completeness.AC4.2 Edge:** tsc_khz is None when KVM_GET_TSC_KHZ is not supported (no error)

### snapshot-completeness.AC5: Deserialization size limit
- **snapshot-completeness.AC5.1 Success:** vmstate file under 10MB loads normally
- **snapshot-completeness.AC5.2 Failure:** vmstate file over 10MB returns SnapshotError without allocating unbounded memory
- **snapshot-completeness.AC5.3 Success:** Incremental snapshot file under 10MB loads normally
- **snapshot-completeness.AC5.4 Failure:** Incremental snapshot file over 10MB returns SnapshotError

### snapshot-completeness.AC6: Integration tests
- **snapshot-completeness.AC6.1 Success:** Guest reads serial scratch register value that was written before snapshot
- **snapshot-completeness.AC6.2 Success:** Guest reads block device data that was written before snapshot
- **snapshot-completeness.AC6.3 Success:** Incremental snapshot/restore preserves guest state after workload
- **snapshot-completeness.AC6.4 Success:** Guest network connectivity works after snapshot/restore

## Glossary

- **KVM**: Kernel-based Virtual Machine — the Linux kernel subsystem that exposes hardware virtualization to userspace.
- **vCPU**: A virtual CPU — a software-emulated processor core with register state that must be saved and restored.
- **legacy device**: A hardware device emulated for compatibility with historical PC or ARM platform conventions (serial ports, keyboard controllers, CMOS, RTC) — as opposed to virtio devices which use a modern paravirtualized interface.
- **virtio**: A standardized interface for paravirtualized devices that uses shared memory queues instead of emulated hardware registers.
- **used ring**: The memory region where the VMM writes completed I/O back to the guest. Written in userspace, outside KVM's dirty page tracking.
- **dirty page tracking**: KVM records which guest memory pages have been written since the last snapshot. KVM tracks guest writes but not VMM userspace writes.
- **incremental snapshot**: A snapshot storing only RAM pages that changed since the previous snapshot, plus full device/vCPU state.
- **Snapshottable**: Rust trait in `src/devices/src/snapshot.rs` that devices implement for snapshot save/restore via `save_state()` / `restore_state()`.
- **BusDevice**: Rust trait for any device on the VM's bus. Provides opt-in `as_snapshottable()` method.
- **MMIO / PortIO**: Two bus address spaces for device communication. MMIO shares the physical address space; PortIO uses x86 I/O ports (`IN`/`OUT` instructions).
- **PortIODeviceManager**: Manager for PortIO-attached x86_64 devices. Currently lacks snapshot methods; this design adds them.
- **16550 UART**: Serial port controller emulated for x86_64 and riscv64 guests. Two identical copies exist; this design deduplicates them.
- **i8042**: Legacy PC keyboard/mouse controller chip emulated for x86_64 guests.
- **CMOS**: Legacy PC chip holding memory size configuration, accessed via I/O ports 0x70/0x71.
- **PL031 RTC**: ARM PrimeCell Real-Time Clock emulated for aarch64 guests.
- **kvmclock**: Paravirtualized clock for Linux guests on KVM x86_64. `kvmclock_ctrl()` notifies the guest of time discontinuity after restore.
- **TSC**: Time Stamp Counter — x86 hardware counter. `tsc_khz` is its frequency; saved for diagnostics and future cross-host validation.
- **stolen time**: Time the guest's vCPU was not scheduled. Misaccounting snapshot gaps as stolen time produces incorrect CPU utilization metrics.
- **bincode**: Rust binary serialization format used to encode device state structs into opaque `Vec<u8>` in snapshots.
- **live migration**: Moving a running VM between physical hosts with minimal downtime using repeated incremental snapshots.

## Architecture

The snapshot system currently saves state for virtio devices (via `MmioTransport`) and aarch64 legacy devices (PL011, GPIO), but silently skips x86_64 legacy devices (serial, i8042, CMOS) and the aarch64 PL031 RTC. This design closes those gaps and addresses several other snapshot correctness issues discovered by comparing against Firecracker's implementation.

### Snapshot Pipeline

The save flow is unchanged in structure. The restore flow adds `kvmclock_ctrl()`:

**Save:** pause vCPUs → quiesce workers → save device states (MMIO + PortIO) → save vCPU/VM/GIC state → mark virtio queue memory dirty → dump memory → resume workers → resume vCPUs.

**Restore:** load vmstate → restore device states (MMIO + PortIO) → `complete_restore()` → resume workers → `kvmclock_ctrl()` (new, x86_64) → resume vCPUs.

### PortIO Device Snapshot Participation

x86_64 legacy devices are managed by `PortIODeviceManager` (`src/vmm/src/device_manager/legacy.rs`), which holds devices as named fields (`stdio_serial`, `i8042`, `cmos`). Unlike `MMIODeviceManager`, it has no snapshot methods today.

Add `save_all_device_states()` and `restore_all_device_states()` to `PortIODeviceManager`, mirroring the MMIO pattern. These iterate the three device fields, call `as_snapshottable()` / `as_snapshottable_mut()`, and collect/restore `Vec<(String, Vec<u8>)>`. The snapshot orchestration in `src/vmm/src/lib.rs` merges PortIO states into the same `device_states` vec in `VmSnapshot`. No format change needed.

### 16550 Serial Deduplication

`src/devices/src/legacy/x86_64/serial.rs` and `src/devices/src/legacy/riscv64/serial.rs` are identical copies. Extract the shared implementation to `src/devices/src/legacy/serial_16550.rs`. The architecture-specific modules re-export or thin-wrap it. The `Snapshottable` implementation lives in the shared module.

### Device State Structs

Each legacy device gets a state struct following the PL011/GPIO pattern — `#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]`, serialized with `bincode`:

**Serial16550State** (shared module):
- `interrupt_enable: u8`, `interrupt_identification: u8`, `line_control: u8`, `line_status: u8`, `modem_control: u8`, `modem_status: u8`, `scratch: u8`, `baud_divisor: u16`, `in_buffer: Vec<u8>`

**I8042State** (`src/devices/src/legacy/i8042.rs`):
- `status: u8`, `control: u8`, `outp: u8`, `cmd: u8`, `buf: Vec<u8>`, `bhead: usize`, `btail: usize`

**CmosState** (`src/devices/src/legacy/x86_64/cmos.rs`):
- `index: u8`, `data: Vec<u8>` (128-byte register array)

**RtcState** (`src/devices/src/legacy/rtc_pl031.rs`):
- `tick_offset: i64`, `previous_now_elapsed_nanos: u64`, `load: u32`, `match_value: u32`, `imsc: u32`, `ris: u32`

Note: The RTC uses `Instant`-based time. `previous_now` cannot be serialized directly (it's opaque). Instead, save the elapsed duration since `previous_now` was set, and reconstruct `previous_now` on restore as `Instant::now() - elapsed`.

Each device overrides `as_snapshottable()` / `as_snapshottable_mut()` on `BusDevice` to return `Some(self)`.

### Virtio Queue Dirty Marking

After `save_all_device_states()`, a new method `mark_virtio_queue_memory_dirty()` on `MMIODeviceManager` iterates all activated virtio devices, gets their queue descriptors, and for each queue marks the used ring pages as dirty. The used ring starts at `used_ring` and spans `6 + 8 * queue_size` bytes. These are userspace writes (from `add_used()` and `set_notification()` in `src/devices/src/virtio/queue.rs`) that KVM's dirty log doesn't track.

This only matters for incremental snapshots (full snapshots dump all memory), but calling it unconditionally is simpler and harmless.

The dirty marking uses the KVM `SET_USER_MEMORY_REGION` dirty logging API — the same mechanism already used by `enable_dirty_tracking()` in `src/vmm/src/lib.rs`. The specific pages are identified by converting the guest physical addresses of each used ring to page-aligned ranges and marking them in the KVM dirty bitmap.

### kvmclock_ctrl on Restore

After restoring x86_64 vCPU state, call `vcpu_fd.kvmclock_ctrl()`. This KVM ioctl informs the guest that a time discontinuity occurred, preventing the kernel from accounting the snapshot gap as stolen time. Tolerate `EINVAL` (older kernels) with a warning log.

Location: end of vCPU restore path in `src/vmm/src/linux/vstate.rs`.

### TSC Frequency in VcpuState

Add `tsc_khz: Option<u32>` to the x86_64 `VcpuState` struct in `src/vmm/src/linux/vstate.rs`. On save, populate via `KVM_GET_TSC_KHZ`. On restore, store in snapshot for diagnostics but do not call `KVM_SET_TSC_KHZ` (same-CPU constraint means frequency matches). The `Option` wrapper handles kernels/CPUs that don't support TSC frequency reporting.

### Deserialization Size Limit

In `load_vmstate()` (`src/vmm/src/snapshot.rs`), replace `file.read_to_end(&mut data)` with a bounded read of up to 10MB. If the file exceeds this, return `SnapshotError::Deserialize("vmstate exceeds 10MB size limit")`. This prevents OOM from corrupted or malicious snapshot files.

## Existing Patterns

This design follows established patterns from the codebase:

**Snapshottable trait** (`src/devices/src/snapshot.rs:111-120`): `snapshot_id() -> &str`, `save_state() -> Vec<u8>`, `restore_state(&[u8])`. Already implemented by PL011 serial and GPIO on aarch64. New implementations follow the same `bincode::serialize`/`deserialize` pattern.

**BusDevice opt-in** (`src/devices/src/bus.rs:35-41`): `as_snapshottable()` returns `Option<&dyn Snapshottable>`, defaulting to `None`. Devices override to return `Some(self)`. Same pattern used by `MmioTransport`.

**Feature gating**: `#[cfg(feature = "snapshot")]` guards implementations, `#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]` on state structs. Matches PL011's `SerialState` and GPIO's `GpioState`.

**State struct convention**: Extract serializable fields into a separate `*State` struct. Non-serializable fields (EventFd, trait objects) are excluded and reconstructed on restore. See `src/devices/src/legacy/aarch64/serial.rs:98-116`.

**Device manager iteration**: `MMIODeviceManager::save_all_device_states()` iterates `id_to_dev_info`, locks each device, calls `as_snapshottable()`. PortIODeviceManager's new methods follow the same pattern but iterate named fields instead of a HashMap.

**No new patterns introduced.** The only structural change is deduplicating the 16550 serial into a shared module, which reduces code rather than adding new abstractions.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: 16550 Serial Deduplication

**Goal:** Extract shared 16550 UART implementation so Snapshottable can be added once.

**Components:**
- New shared module `src/devices/src/legacy/serial_16550.rs` — contains `Serial` struct, `BusDevice` impl, all register logic
- `src/devices/src/legacy/x86_64/serial.rs` — re-exports from shared module
- `src/devices/src/legacy/riscv64/serial.rs` — re-exports from shared module
- `src/devices/src/legacy/mod.rs` — updated module declarations

**Dependencies:** None

**Done when:** `cargo build` succeeds on all targets, existing serial tests pass, x86_64 and riscv64 serial modules use the shared implementation
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: Legacy Device Snapshottable Implementations

**Goal:** All legacy devices implement `Snapshottable` with state structs and bincode serialization.

**Components:**
- `Serial16550State` + `Snapshottable` impl in `src/devices/src/legacy/serial_16550.rs`
- `I8042State` + `Snapshottable` impl in `src/devices/src/legacy/i8042.rs`
- `CmosState` + `Snapshottable` impl in `src/devices/src/legacy/x86_64/cmos.rs`
- `RtcState` + `Snapshottable` impl in `src/devices/src/legacy/rtc_pl031.rs`
- `BusDevice::as_snapshottable()` overrides on each device
- Unit tests for save/restore round-trip on each device

**Dependencies:** Phase 1 (serial deduplication)

**Done when:** Each device can serialize its state, deserialize it, and the restored device produces identical register reads. Tests cover: round-trip serialization, state fidelity after restore, edge cases (full FIFO buffer for serial, full i8042 buffer). Covers `snapshot-completeness.AC1.*`.
<!-- END_PHASE_2 -->

<!-- START_PHASE_3 -->
### Phase 3: PortIO Device Manager Snapshot Support

**Goal:** x86_64 PortIO devices participate in the snapshot save/restore flow.

**Components:**
- `save_all_device_states()` on `PortIODeviceManager` in `src/vmm/src/device_manager/legacy.rs`
- `restore_all_device_states()` on `PortIODeviceManager`
- Snapshot orchestration changes in `src/vmm/src/lib.rs` — merge PortIO states into `VmSnapshot.device_states`, restore them alongside MMIO states
- Unit tests verifying PortIO states appear in snapshot output and restore correctly

**Dependencies:** Phase 2 (devices implement Snapshottable)

**Done when:** A full snapshot includes PortIO device states. A restore populates PortIO devices from the snapshot. No device is silently skipped (verified by checking `device_states` vec contains entries for all registered PortIO devices). Covers `snapshot-completeness.AC1.*`.
<!-- END_PHASE_3 -->

<!-- START_PHASE_4 -->
### Phase 4: Virtio Queue Dirty Marking

**Goal:** Used ring pages are marked dirty after snapshot save for incremental snapshot correctness.

**Components:**
- `mark_virtio_queue_memory_dirty()` on `MMIODeviceManager` in `src/vmm/src/device_manager/kvm/mmio.rs`
- Call site in `src/vmm/src/lib.rs` after `save_all_device_states()` in both full and incremental snapshot paths
- Helper to calculate page-aligned ranges from queue used ring addresses

**Dependencies:** None (independent of Phases 1-3, but ordered here for logical flow)

**Done when:** After a snapshot save, the used ring pages for all active virtio queues are marked in the KVM dirty bitmap. Unit test verifies the correct pages are marked for a known queue configuration. Covers `snapshot-completeness.AC2.*`.
<!-- END_PHASE_4 -->

<!-- START_PHASE_5 -->
### Phase 5: vCPU Restore Fixes

**Goal:** kvmclock_ctrl and TSC frequency for correct guest time behavior after restore.

**Components:**
- `kvmclock_ctrl()` call at end of x86_64 vCPU restore in `src/vmm/src/linux/vstate.rs`
- `tsc_khz: Option<u32>` field added to x86_64 `VcpuState` in `src/vmm/src/linux/vstate.rs`
- `KVM_GET_TSC_KHZ` call in save path, stored in VcpuState

**Dependencies:** None (independent of other phases)

**Done when:** After x86_64 vCPU restore, `kvmclock_ctrl()` is called (with warning on failure). TSC frequency is present in saved VcpuState. Covers `snapshot-completeness.AC3.*`, `snapshot-completeness.AC4.*`.
<!-- END_PHASE_5 -->

<!-- START_PHASE_6 -->
### Phase 6: Deserialization Size Limit

**Goal:** Prevent OOM from corrupted vmstate files.

**Components:**
- Bounded read in `load_vmstate()` in `src/vmm/src/snapshot.rs`
- `load_incremental_snapshot()` gets same treatment
- Unit test with oversized input

**Dependencies:** None

**Done when:** Loading a vmstate file >10MB returns an error. Loading a valid file <10MB succeeds. Covers `snapshot-completeness.AC5.*`.
<!-- END_PHASE_6 -->

<!-- START_PHASE_7 -->
### Phase 7: Integration Tests

**Goal:** End-to-end verification that device state survives snapshot/restore from the guest perspective.

**Components:**
- Integration test in `tests/` using `#[host]`/`#[guest]` proc macros
- Test scenarios:
  - Serial: write scratch register value before snapshot, read back after restore, verify match
  - Block: write data to virtio-blk before snapshot, read back after restore, verify match
  - Incremental: full snapshot → guest workload → incremental snapshot → restore → verify state
- Test infrastructure may need snapshot/restore helpers in the host-side test harness

**Dependencies:** Phases 1-6 (all snapshot fixes in place)

**Done when:** Integration tests pass exercising snapshot/restore with device state verification from inside the guest. Covers `snapshot-completeness.AC6.*`.
<!-- END_PHASE_7 -->

## Additional Considerations

**RTC time serialization:** `Instant` is opaque and cannot be serialized. The `RtcState` saves the elapsed nanoseconds since `previous_now` was captured. On restore, `previous_now` is reconstructed as `Instant::now()` and `tick_offset` is adjusted to account for the wall-clock gap. This preserves the guest's perception of time continuity.

**CMOS is read-only from guest perspective:** The guest writes to the index register (port 0x70) but the data registers (port 0x71) are read-only in this implementation. The CMOS state is initialized at VM build time with memory size information. Snapshot/restore preserves the `index` and `data` array, which is correct — the guest may have set the index register to a specific value.

**Snapshot format version stays at 1.** Adding new device states to `device_states: Vec<(String, Vec<u8>)>` is additive — old snapshots without those entries will simply not restore those devices (they keep construction defaults). This is acceptable for pre-release.
