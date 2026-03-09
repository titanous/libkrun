# Safe Snapshot Serialization Design

## Summary

libkrun uses `bincode` 1.x to serialize virtual machine device state when creating snapshots — freeze-frames of a running VM that can be restored later. The problem is that bincode 1.x is unmaintained and its deserialization is unsafe: a crafted snapshot can embed a Vec or String length prefix claiming to hold gigabytes of data, causing the allocator to attempt an enormous allocation (and likely OOM the host process) before any validation occurs. This is a guest-to-host attack surface when snapshot files are treated as untrusted input.

This design replaces every bincode 1.x call site — across device state structs, VMM orchestration, and vhost-user backend state — with `bincode-next` (the maintained community fork, v3.x) behind a thin `snapshot_serde` module. The module exposes a single `deserialize` entry point that applies a per-device byte limit via bincode-next's `with_limit()` guard, which rejects oversized allocation requests before they reach the allocator. State structs drop their serde derives in favor of bincode-next's native `Encode`/`Decode`, removing the serde dependency from the snapshot path. The migration is a clean break: existing snapshots are intentionally invalidated because the wire format changes (varint-encoded lengths replace fixed 8-byte prefixes), which is acceptable since integration tests always create fresh snapshots.

## Definition of Done
- Replace all bincode serialize/deserialize calls with a new maintained backend
- Provide a safe wrapper API that enforces byte-level deserialization limits (no OOM from crafted snapshots)
- Migrate all 19 call sites across Snapshottable impls and backend_state methods
- Preserve existing field-level validation where it exists
- Clean break: no backwards compatibility with bincode-era snapshots

## Acceptance Criteria

### safe-snapshot-serde.AC1: All bincode call sites migrated to bincode-next
- **safe-snapshot-serde.AC1.1 Success:** Legacy device Snapshottable impls (serial 16550, i8042, CMOS, PL011, RTC, GPIO, GICv3) serialize/deserialize via `snapshot_serde` module
- **safe-snapshot-serde.AC1.2 Success:** Legacy device snapshot round-trip tests pass with new backend
- **safe-snapshot-serde.AC1.3 Success:** Virtio device snapshot paths (MMIO transport, balloon, vhost-user vsock, vhost-user fs) use `snapshot_serde`
- **safe-snapshot-serde.AC1.4 Success:** Vhost-user `save_backend_state`/`restore_backend_state` methods use `snapshot_serde`
- **safe-snapshot-serde.AC1.5 Success:** VMM-level snapshot structs (VmSnapshot, IncrementalSnapshot, SnapshotHeader, vCPU state, IC state) serialize/deserialize via `snapshot_serde`

### safe-snapshot-serde.AC2: Safe wrapper enforces byte-level deserialization limits
- **safe-snapshot-serde.AC2.1 Success:** `snapshot_serde::deserialize` with valid data under limit succeeds
- **safe-snapshot-serde.AC2.2 Failure:** `snapshot_serde::deserialize` with payload exceeding `max_bytes` returns `SnapshotError::Deserialize` without allocating the claimed size
- **safe-snapshot-serde.AC2.3 Failure:** Crafted payload with Vec length prefix claiming 1GB+ is rejected before allocation

### safe-snapshot-serde.AC3: Existing field-level validation preserved
- **safe-snapshot-serde.AC3.1 Success:** CMOS `data.len() == 128`, i8042 `buf.len() == 16`, serial 16550 `in_buffer.len() <= 64` checks remain and reject invalid sizes
- **safe-snapshot-serde.AC3.2 Success:** PL011 gains `read_fifo` length validation (bounded by FIFO size)

### safe-snapshot-serde.AC4: State struct derives use bincode-next native Encode/Decode
- **safe-snapshot-serde.AC4.1 Success:** All `*State` structs use `#[cfg_attr(feature = "snapshot", derive(bincode_next::Encode, bincode_next::Decode))]` instead of serde derives

### safe-snapshot-serde.AC5: bincode 1.x fully removed
- **safe-snapshot-serde.AC5.1 Success:** No `bincode` (1.x) in `Cargo.lock` after migration
- **safe-snapshot-serde.AC5.2 Success:** `fuzz/Cargo.toml` uses bincode-next for snapshot-related fuzz targets

## Glossary
- **bincode**: Compact binary serialization crate for Rust (version 1.x). Uses fixed 8-byte length prefixes for variable-length types, unmaintained, no runtime allocation limits.
- **bincode-next**: Community-maintained fork of bincode, v3.x. Adds `with_limit()` to reject deserializations exceeding a byte budget before allocating.
- **`Snapshottable` trait**: Interface in `src/devices/src/snapshot.rs` that snapshotable devices implement. Provides `save_state()` and `restore_state()`.
- **`*State` struct**: Plain data struct capturing serializable device fields (e.g., `Serial16550State`, `BalloonState`).
- **`snapshot_serde` module**: New thin wrapper (`src/devices/src/snapshot_serde.rs`) centralizing bincode-next configuration with mandatory byte limit parameter.
- **`with_limit()`**: bincode-next API wrapping a deserializer config with a cumulative byte budget. Returns error before allocating if limit would be exceeded.
- **`save_backend_state` / `restore_backend_state`**: Vhost-user device methods for serializing backend state outside the `Snapshottable` trait.
- **`VmSnapshot` / `IncrementalSnapshot`**: Top-level VMM structs aggregating all device state plus vCPU and memory metadata into a snapshot artifact.
- **`VMSTATE_MAX_SIZE`**: Existing 10 MB limit on serialized VMM state file, retained as defense-in-depth.
- **`MAX_SNAPSHOT_BYTES`**: Per-device constant (2-4x realistic size) passed as `max_bytes` to `snapshot_serde::deserialize`.
- **`Encode` / `Decode`**: bincode-next's native derive traits, replacing serde's `Serialize`/`Deserialize`.
- **varint encoding**: Variable-length integer encoding for length prefixes. Small numbers use fewer bytes than fixed-width. Makes the wire format incompatible with bincode 1.x.
- **OOM**: Out Of Memory. A crafted Vec length prefix in bincode 1.x can trigger this by claiming an arbitrarily large allocation.
- **vhost-user**: Protocol allowing virtio device backends to run in a separate process. Used for virtio-fs and vsock backends.
- **legacy devices**: Non-virtio hardware interfaces: serial 16550, i8042, CMOS, PL011, RTC PL031, GPIO, GICv3.
- **MMIO transport**: Virtio memory-mapped I/O transport layer wrapping queue and feature negotiation state.

## Architecture

Replace all `bincode` serialization with `bincode-next` (the maintained community fork, crate `bincode-next` v3.x) behind a thin `snapshot_serde` module. The module centralizes configuration and enforces byte-level deserialization limits via bincode-next's `with_limit()`, which validates allocation requests *before* touching the allocator.

### Module: `src/devices/src/snapshot_serde.rs`

```rust
pub fn serialize<T: Encode>(state: &T) -> Result<Vec<u8>, SnapshotError>;
pub fn deserialize<T: Decode>(data: &[u8], max_bytes: usize) -> Result<T, SnapshotError>;
```

`serialize` uses a shared bincode-next config (little-endian, variable int encoding). `deserialize` applies a runtime byte limit — if the deserializer's cumulative allocation exceeds `max_bytes`, it returns an error before allocating. This eliminates the OOM vector from crafted Vec/String length prefixes.

### Per-Device Byte Limits

Each device defines a `MAX_SNAPSHOT_BYTES` constant next to its state struct, sized 2-4x the realistic maximum:

| Device | Realistic Size | Limit |
|--------|---------------|-------|
| Serial 16550 | ~20 bytes | 128 |
| i8042 | ~25 bytes | 128 |
| CMOS | ~140 bytes | 512 |
| PL011 | ~80 bytes | 512 |
| RTC PL031 | ~32 bytes | 128 |
| GPIO | ~32 bytes | 128 |
| GICv3 | variable | 4096 |
| MMIO Transport | ~200-300 bytes | 4096 |
| Balloon | ~24 bytes | 128 |
| VhostUser Vsock | ~200 bytes | 8192 |
| VhostUser FS | ~500 bytes | 8192 |

VMM-level structs (`VmSnapshot`, `IncrementalSnapshot`) retain the existing 10MB `VMSTATE_MAX_SIZE` limit.

### Derive Strategy

State structs switch from serde derives to bincode-next native `Encode`/`Decode`, feature-gated:

```rust
#[cfg_attr(feature = "snapshot", derive(bincode_next::Encode, bincode_next::Decode))]
```

Native derives chosen over serde compat for: fewer dependencies, tighter integration with `with_limit()`, no serde middleman.

### Field-Level Validation

Post-deserialization domain validation is preserved where it exists:
- CMOS: `data.len() == 128`
- i8042: `buf.len() == 16`
- Serial 16550: `in_buffer.len() <= 64`

These enforce domain invariants, not allocation safety. The byte limit handles allocation; field checks handle correctness.

### VMM-Level Integration

`src/vmm/src/snapshot.rs` functions `write_vmstate`/`read_vmstate` and their incremental counterparts use `snapshot_serde` with `VMSTATE_MAX_SIZE` as the limit. The existing file-size pre-check remains as defense in depth (fails before the read).

`src/vmm/src/lib.rs` contains the most call sites (~19 production + test): vCPU state, interrupt controller, VM state, and the top-level snapshot orchestration. All migrate to `snapshot_serde`.

`src/vmm/src/snapshot_store.rs` has additional serialization for merged snapshots and excluded page indices.

`src/vmm/src/linux/vstate.rs` serializes per-vCPU register state (arch-specific: `VcpuState` on x86_64, `Aarch64VcpuState` on aarch64).

## Existing Patterns

Investigation found a consistent pattern across all `Snapshottable` implementations:

1. Define a `*State` struct with `#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]`
2. `save_state()` copies fields into the state struct, calls `bincode::serialize`
3. `restore_state()` calls `bincode::deserialize`, optionally validates, then copies fields back

This pattern is preserved — only the serialization backend changes. The `Snapshottable` trait itself (`src/devices/src/snapshot.rs`) is unchanged.

Three devices (CMOS, i8042, serial 16550) validate after deserialization. The remaining devices with unbounded fields (PL011, MMIO transport, vhost-user vsock/fs) do not. This design adds byte-limit protection for all devices structurally, and adds field-level validation to PL011's `read_fifo` (bounded by its FIFO size, same pattern as serial 16550).

Vhost-user devices use `save_backend_state()`/`restore_backend_state()` instead of the `Snapshottable` trait, but follow the same serialize/deserialize pattern. Both paths migrate to `snapshot_serde`.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: Add `snapshot_serde` Module and bincode-next Dependency

**Goal:** Create the centralized serialization module and add bincode-next to the dependency tree.

**Components:**
- `src/devices/src/snapshot_serde.rs` — new module with `serialize`/`deserialize` functions
- `src/devices/Cargo.toml` — add `bincode-next` optional dependency behind `snapshot` feature
- `src/devices/src/lib.rs` — declare `snapshot_serde` module

**Dependencies:** None (first phase)

**Done when:** Module compiles, unit tests verify serialize/deserialize round-trip with byte limits, and a test confirms oversized payloads are rejected before allocation. Covers safe-snapshot-serde.AC2.1, AC2.2, AC2.3.
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: Migrate Legacy Devices

**Goal:** Switch all legacy device `Snapshottable` impls from bincode to `snapshot_serde`.

**Components:**
- `src/devices/src/legacy/serial_16550.rs` — Serial16550State: switch derives, use `snapshot_serde`, keep `in_buffer` validation
- `src/devices/src/legacy/i8042.rs` — I8042State: switch derives, use `snapshot_serde`, keep `buf` validation
- `src/devices/src/legacy/x86_64/cmos.rs` — CmosState: switch derives, use `snapshot_serde`, keep `data` validation
- `src/devices/src/legacy/aarch64/serial.rs` — SerialState (PL011): switch derives, use `snapshot_serde`, add `read_fifo` length validation
- `src/devices/src/legacy/rtc_pl031.rs` — RtcState: switch derives, use `snapshot_serde`
- `src/devices/src/legacy/aarch64/gpio.rs` — GpioState: switch derives, use `snapshot_serde`
- `src/devices/src/legacy/gicv3.rs` — GicV3SnapshotState: switch derives, use `snapshot_serde`
- `src/devices/src/legacy/kvmgicv3.rs` — GicV3State: switch derives, use `snapshot_serde`

**Dependencies:** Phase 1

**Done when:** All legacy device snapshot tests pass with new backend. Existing field-level validation preserved. PL011 gains `read_fifo` length check. Covers safe-snapshot-serde.AC1.1, AC1.2, AC3.1, AC4.1.
<!-- END_PHASE_2 -->

<!-- START_PHASE_3 -->
### Phase 3: Migrate Virtio Devices

**Goal:** Switch MMIO transport, balloon, and vhost-user device serialization to `snapshot_serde`.

**Components:**
- `src/devices/src/virtio/mmio.rs` — MmioTransportState: switch derives, use `snapshot_serde`
- `src/devices/src/virtio/balloon/device.rs` — BalloonState: switch derives, use `snapshot_serde`
- `src/devices/src/virtio/vhost_user/vsock.rs` — VhostUserVsockState: switch derives, use `snapshot_serde`
- `src/devices/src/virtio/vhost_user/fs.rs` — VhostUserFsState: switch derives, use `snapshot_serde`

**Dependencies:** Phase 1

**Done when:** All virtio device snapshot tests pass. Vhost-user `save/restore_backend_state` methods use `snapshot_serde`. Covers safe-snapshot-serde.AC1.3, AC1.4.
<!-- END_PHASE_3 -->

<!-- START_PHASE_4 -->
### Phase 4: Migrate VMM-Level Serialization

**Goal:** Switch top-level snapshot structs and orchestration code from bincode to `snapshot_serde`.

**Components:**
- `src/vmm/Cargo.toml` — add `bincode-next` dependency (or re-export from devices crate)
- `src/vmm/src/snapshot.rs` — VmSnapshot, IncrementalSnapshot, SnapshotHeader: switch derives, use `snapshot_serde` in `write_vmstate`/`read_vmstate` and incremental variants
- `src/vmm/src/lib.rs` — migrate all ~19 production bincode calls (vCPU state, IC snapshot, VM state, snapshot orchestration)
- `src/vmm/src/snapshot_store.rs` — migrate merged snapshot serialization and excluded page index ser/de
- `src/vmm/src/builder.rs` — migrate vmstate deserialization in restore path
- `src/vmm/src/linux/vstate.rs` — migrate per-vCPU state serialization (arch-specific)

**Dependencies:** Phases 2, 3 (device-level migration proven first)

**Done when:** All VMM snapshot tests pass. Full snapshot/restore integration tests pass. Covers safe-snapshot-serde.AC1.5, AC3.2.
<!-- END_PHASE_4 -->

<!-- START_PHASE_5 -->
### Phase 5: Remove bincode 1.x and Update Fuzz/Test Workspaces

**Goal:** Remove the old bincode dependency entirely and update ancillary workspaces.

**Components:**
- `src/devices/Cargo.toml` — remove `bincode` dependency
- `src/vmm/Cargo.toml` — remove `bincode` dependency
- `Cargo.toml` (root) — remove any bincode references
- `fuzz/Cargo.toml` — replace `bincode` with `bincode-next`, update fuzz targets that deserialize snapshot data
- `Cargo.lock` — regenerated without bincode

**Dependencies:** Phase 4

**Done when:** `cargo build --all-features` succeeds with no bincode 1.x in dependency tree. Fuzz targets compile and run. `just check` passes. Covers safe-snapshot-serde.AC5.1, AC5.2.
<!-- END_PHASE_5 -->

## Additional Considerations

**Wire format change:** bincode 1.x uses fixed 8-byte length prefixes for Vec/String; bincode-next uses varint encoding. All existing snapshots become invalid. This is the accepted clean break. Integration tests create fresh snapshots each run, so no test fixtures need updating.

**bincode-next rc status:** v3.0.0-rc.5 is pre-release. The API surface we use (encode/decode with config + limit) has been stable across rc versions. Pin the exact version in Cargo.toml. If the API changes before 3.0 stable, the migration is isolated to `snapshot_serde.rs`.

**aarch64 devices:** PL011 (`src/devices/src/legacy/aarch64/serial.rs`), GPIO (`src/devices/src/legacy/aarch64/gpio.rs`), RTC PL031, and GICv3 are aarch64-only. The migration is the same pattern but these files are `#[cfg(target_arch)]`-gated. Testing on x86_64 covers the x86 devices; aarch64 CI covers the rest.
