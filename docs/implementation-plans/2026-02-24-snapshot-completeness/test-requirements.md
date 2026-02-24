# Snapshot Completeness — Test Requirements

Generated from: `docs/design-plans/2026-02-24-snapshot-completeness.md`

## Automated Tests

| AC ID | Description | Test Type | Test File | Phase |
|-------|-------------|-----------|-----------|-------|
| snapshot-completeness.AC1.1 | 16550 Serial registers round-trip | Unit | `src/devices/src/legacy/serial_16550.rs` (test module) | 2 |
| snapshot-completeness.AC1.2 | 16550 Serial in_buffer FIFO preserved | Unit | `src/devices/src/legacy/serial_16550.rs` (test module) | 2 |
| snapshot-completeness.AC1.3 | i8042 registers and buffer round-trip | Unit | `src/devices/src/legacy/i8042.rs` (test module) | 2 |
| snapshot-completeness.AC1.4 | CMOS index + 128 data bytes round-trip | Unit | `src/devices/src/legacy/x86_64/cmos.rs` (test module) | 2 |
| snapshot-completeness.AC1.5 | PL031 RTC registers round-trip | Unit | `src/devices/src/legacy/rtc_pl031.rs` (test module) | 2 |
| snapshot-completeness.AC1.6 | PortIO device states in VmSnapshot | Unit | `src/vmm/src/device_manager/legacy.rs` (test module) | 3 |
| snapshot-completeness.AC1.7 | Corrupted state returns SnapshotError | Unit | `src/devices/src/legacy/serial_16550.rs` (test module) | 2 |
| snapshot-completeness.AC1.8 | Missing device entry leaves defaults | Unit | `src/vmm/src/device_manager/legacy.rs` (test module) | 3 |
| snapshot-completeness.AC2.1 | Used ring pages marked dirty | Unit | `src/vmm/src/device_manager/kvm/mmio.rs` (test module) | 4 |
| snapshot-completeness.AC2.3 | Inactive queues not marked | Unit | `src/vmm/src/device_manager/kvm/mmio.rs` (test module) | 4 |
| snapshot-completeness.AC5.1 | vmstate under 10MB loads normally | Unit | `src/vmm/src/snapshot.rs` (test module, existing round-trip tests) | 6 |
| snapshot-completeness.AC5.2 | vmstate over 10MB returns error | Unit | `src/vmm/src/snapshot.rs` (test module) | 6 |
| snapshot-completeness.AC5.3 | Incremental under 10MB loads normally | Unit | `src/vmm/src/snapshot.rs` (test module, existing round-trip tests) | 6 |
| snapshot-completeness.AC5.4 | Incremental over 10MB returns error | Unit | `src/vmm/src/snapshot.rs` (test module) | 6 |
| snapshot-completeness.AC6.1 | Serial scratch register survives snapshot/restore | E2E | `tests/test_cases/src/test_snapshot_serial.rs` | 7 |
| snapshot-completeness.AC6.2 | Block device data survives snapshot/restore | E2E | `tests/test_cases/src/test_snapshot_block.rs` | 7 |
| snapshot-completeness.AC6.3 | Incremental snapshot preserves guest state | E2E | `tests/test_cases/src/test_snapshot_incremental_state.rs` | 7 |
| snapshot-completeness.AC6.4 | Network connectivity after snapshot/restore | E2E | `tests/test_cases/src/test_snapshot_net.rs` | 7 |

## Human Verification

| AC ID | Description | Justification | Verification Approach |
|-------|-------------|---------------|----------------------|
| snapshot-completeness.AC2.2 | Incremental snapshot includes used ring pages in dirty set | Unit test verifies page range collection logic, but confirming pages appear in a serialized incremental snapshot requires live KVM dirty tracking with an active virtio device. The E2E test for AC6.3 (incremental snapshot preserves guest state) provides strong indirect coverage. | Code review: verify `get_virtio_used_ring_ranges()` result is merged into `dirty_pages` Vec in `create_incremental_snapshot()`. AC6.3 E2E test exercises the full incremental path end-to-end. |
| snapshot-completeness.AC3.1 | kvmclock_ctrl() called after each vCPU restore | Single KVM ioctl with no userspace-observable side effect — the effect (no stolen-time misaccounting) is only visible through guest `/proc/stat` under specific timing conditions. | Code review: verify `self.fd.kvmclock_ctrl()` call at end of `restore_state()` in `src/vmm/src/linux/vstate.rs`. The call is unconditional (not behind a feature flag or condition). |
| snapshot-completeness.AC3.2 | kvmclock_ctrl() failure logs warning, does not fail restore | Error path requires an older kernel that doesn't support the ioctl — not reproducible in CI. | Code review: verify the `if let Err(e)` pattern with `warn!()` log, and that no `?` operator or `return Err(...)` follows the call. |
| snapshot-completeness.AC4.2 | tsc_khz is None when KVM_GET_TSC_KHZ unsupported | Requires a host without TSC frequency reporting (effectively a CPU without invariant TSC), which is unavailable in modern x86_64 CI environments. | Code review: verify `match self.fd.get_tsc_khz()` has `Err(_) => None` arm with `warn!()` log. Verify `#[serde(default)]` on the `tsc_khz` field ensures old snapshots deserialize as `None`. |

## Coverage Summary

- Total acceptance criteria: 22
- Automated: 18
- Human verification: 4
- Coverage: 100%
