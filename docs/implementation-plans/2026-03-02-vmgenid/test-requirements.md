# VMGENID Test Requirements

Generated from acceptance criteria in docs/design-plans/2026-03-02-vmgenid.md

## Automated Tests

| AC ID | Criterion | Test Type | Test File | Phase |
|-------|-----------|-----------|-----------|-------|
| vmgenid.AC1.1 | Fresh random 128-bit GUID is generated and written to guest page at offset 40 on VM creation | Unit | `src/devices/src/vmgenid/mod.rs` (`#[cfg(test)]` module) | 2 |
| vmgenid.AC1.2 | `update_guid()` produces a different GUID each invocation and writes it to the correct guest physical address | Unit | `src/devices/src/vmgenid/mod.rs` (`#[cfg(test)]` module) | 2 |
| vmgenid.AC4.4 | Two VMs restored from the same snapshot produce different 32-byte `/dev/urandom` output and different GUIDs | Integration (e2e) | `tests/test_cases/src/test_snapshot_rng_reseed.rs` | 6 |
| vmgenid.AC5.2 | GUID page address does not appear in E820 memory map entries | Unit | `src/arch/src/x86_64/acpi.rs` or `src/arch/src/x86_64/mod.rs` (`#[cfg(test)]` module) | 1 |
| vmgenid.AC6.1 | `on_restore_complete()` method removed from `VirtioDevice` trait | Unit (grep assertion) | Verified via `grep -r "on_restore_complete" src/` returning no matches; can be a CI script check | 6 |
| vmgenid.AC6.2 | No snapshot-specific additions remain in the Rng device from the rng worktree | Unit (grep assertion) | Verified via `grep -r "on_restore_complete\|restore_rng\|reseed" src/devices/src/virtio/rng/` returning no matches; can be a CI script check | 6 |

### Test Details

#### vmgenid.AC1.1 — Unit test: GUID generation and initial write

- Create a `GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)])`.
- Construct `Vmgenid::new(guid_page_addr, guid_offset, irq, &mem)`.
- Read back 16 bytes from `GuestAddress(guid_page_addr + guid_offset)`.
- Assert bytes match `vmgenid.guid()`.
- Assert GUID is not all zeros.

#### vmgenid.AC1.2 — Unit test: GUID update produces different value

- Construct `Vmgenid` as above.
- Store initial GUID via `vmgenid.guid()`.
- Call `update_guid(&mem)` and capture `(old, new)` return.
- Assert `old == initial_guid`.
- Assert `old != new`.
- Read back 16 bytes from guest memory and assert they match `new`.
- Call `update_guid()` a second time and assert it produces yet another different GUID.

#### vmgenid.AC4.4 — Integration test: snapshot restore entropy divergence

- Guest connects via vsock, signals "READY", waits for "READ" command, responds with 32 bytes from `/dev/urandom`.
- Host takes snapshot with guest idle, restores twice from the same snapshot.
- After each restore, host sends "READ" and collects 32-byte entropy samples.
- Assert the two entropy samples differ (proving CSPRNG was reseeded by VMGENID).
- Test file: ported from `rng-snapshot-reseed` branch.
- Feature gate: `snapshot`.

#### vmgenid.AC5.2 — Code-level verification: E820 exclusion

- The E820 setup in `configure_system()` only maps 0 to `EBDA_START` (0x9FC00) and `HIMEM_START` (0x100000) onwards.
- The GUID page at 0xC0000 and ACPI tables at 0xE0000 fall in the gap (0x9FC00 to 0xFFFFF) that is NOT in E820.
- This can be verified by a unit test that calls E820 setup and asserts no entry covers the GUID page address, or by static code review (the addresses are constants and the E820 entries are constants).

#### vmgenid.AC6.1 and vmgenid.AC6.2 — Absence verification

- These are verified by confirming the relevant code does not exist on the base branch.
- Can be automated as a CI grep check: `grep -r "on_restore_complete" src/` must return no matches.
- Phase 6 documents that these ACs are pre-satisfied on the base branch (the `on_restore_complete` method was never merged).

## Human Verification

| AC ID | Criterion | Justification | Verification Approach |
|-------|-----------|---------------|----------------------|
| vmgenid.AC2.1 | Kernel finds RSDP in EBDA scan range, parses XSDT -> FADT, enters HW-reduced ACPI mode | Requires booting an x86_64 VM and observing kernel ACPI initialization output. Cannot be unit tested because it depends on the Linux kernel's ACPI subsystem parsing tables from guest memory at runtime. | Boot x86_64 VM. Run `dmesg \| grep -i acpi` and verify ACPI initialization messages appear. Run `dmesg \| grep -i "HW-reduced\|Hardware-reduced"` and verify HW-reduced ACPI mode is active. Verify no ACPI errors about missing GPE blocks. |
| vmgenid.AC2.2 | vmgenid driver binds to `\_SB.VGEN` device via CID `"VMGENCTR"` | Requires the Linux vmgenid kernel driver to successfully match the ACPI device and bind to it at runtime. This depends on kernel driver probe logic, ACPI namespace traversal, and AML evaluation that cannot be simulated in a unit test. | Boot x86_64 VM. Run `dmesg \| grep vmgenid` and verify the driver binds (e.g., `vmgenid: driver loaded`). Alternatively, check `/sys/bus/acpi/devices/` for a `VMGENCTR` entry. |
| vmgenid.AC2.3 | vmgenid driver evaluates ADDR method and reads initial GUID from guest memory | Requires the kernel driver to evaluate AML bytecode (the ADDR method) and read from the physical address it returns. This is an end-to-end kernel/hypervisor interaction that cannot be unit tested. | Boot x86_64 VM. Run `dmesg \| grep vmgenid` and verify no error messages about ADDR evaluation failure. The driver binding (AC2.2) implicitly confirms AC2.3 succeeds, since the driver reads the GUID during probe. |
| vmgenid.AC3.1 | FDT contains vmgenid node with `compatible = "microsoft,vmgenid"`, `reg`, and `interrupts` properties | While the FDT generation code can be reviewed, verifying the actual FDT blob contents as parsed by the kernel requires either a binary FDT dump tool or a running aarch64 VM. An FDT unit test could validate the node structure if the FDT writer supports serialization to bytes for inspection, but the existing test infrastructure does not include FDT parsing. | Boot aarch64 VM. Run `ls /proc/device-tree/vmgenid@*/` and verify `compatible`, `reg`, and `interrupts` files exist. Or run `dtc -I fs /proc/device-tree/ 2>/dev/null \| grep -A5 vmgenid` to dump the node. |
| vmgenid.AC3.2 | vmgenid driver binds via device tree compatible match | Requires the Linux vmgenid driver to match on `compatible = "microsoft,vmgenid"` in the FDT at runtime. This is kernel driver probe behavior that cannot be unit tested. | Boot aarch64 VM. Run `dmesg \| grep vmgenid` and verify the driver binds. Check `/sys/bus/platform/drivers/vmgenid/` for a bound device. |
| vmgenid.AC3.3 | vmgenid driver reads initial GUID from `reg` address and registers IRQ handler | Requires the kernel driver to read from the physical address specified in the FDT `reg` property and register an IRQ handler for the SPI in `interrupts`. End-to-end kernel interaction. | Boot aarch64 VM. Run `dmesg \| grep vmgenid` and verify no probe errors. Check `/proc/interrupts` for the vmgenid IRQ line. Driver binding (AC3.2) implicitly confirms the GUID read and IRQ registration succeeded. |
| vmgenid.AC4.1 | After restore, VMM writes a new GUID to the guest page before vCPUs resume | The ordering guarantee (GUID written before vCPU resume) is enforced by code placement in `restore_device_and_vcpu_states()` but cannot be directly observed in a test without instrumentation. The integration test (AC4.4) validates the outcome but not the precise ordering. | Code review: verify `vmgenid.update_guid()` call is placed after device restore and before `restore_vcpu_states()` in `src/vmm/src/lib.rs`. The integration test (AC4.4) indirectly validates this -- if the GUID were written after vCPU resume, the driver might miss the change. |
| vmgenid.AC4.2 | VMM injects the platform-specific interrupt (GED on x86_64, SPI on aarch64) after writing the GUID | The interrupt injection mechanism (EventFd -> KVM irqfd -> GED/SPI) is internal to KVM and cannot be directly observed from a test. The integration test (AC4.4) validates the end-to-end outcome. | Code review: verify `vmgenid.signal_interrupt()` is called immediately after `update_guid()` in the restore flow. The integration test (AC4.4) indirectly validates this -- without the interrupt, the guest driver would not detect the GUID change and CSPRNG would not be reseeded. |
| vmgenid.AC4.3 | Guest kernel logs `"crng reseeded due to virtual machine fork"` after restore | Requires reading guest dmesg after a snapshot restore, which the current integration test framework does not capture (it uses vsock for data exchange, not dmesg scraping). | Boot VM, take snapshot, restore. On the restored VM, run `dmesg \| grep "crng reseeded due to virtual machine fork"` and verify the message appears. This can potentially be automated in the integration test by having the guest-side code read `/dev/kmsg` after restore and send the result over vsock, but the current test design validates entropy divergence (AC4.4) instead. |
| vmgenid.AC5.1 | x86_64 FADT has `HW_REDUCED_ACPI` flag set (bit 20) and revision 6 | The FADT flag and revision are set by the `acpi_tables` crate's `FADTBuilder`. A unit test could serialize the FADT and check the bytes, but the `acpi_tables` crate's output format would need to be parsed. More practically verified by kernel boot behavior (AC2.1). | Code review: verify `FADTBuilder` is constructed with `.flag(Flags::HwReducedAcpi)` and the crate produces revision 6. Runtime: boot x86_64 VM and verify `dmesg \| grep "HW-reduced"` shows HW-reduced ACPI mode. Alternatively, run `cat /sys/firmware/acpi/tables/FACP \| xxd` in the guest and check byte 112 (flags field) has bit 20 set and byte 8 (revision) is 6. |

## Test Commands

```bash
# Unit tests for vmgenid device model (AC1.1, AC1.2)
cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo test -p devices -- vmgenid

# Full integration test suite including snapshot-rng-reseed (AC4.4)
cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && make test FEATURE_FLAGS="--features embedded_init,snapshot"

# Cleanup verification (AC6.1, AC6.2) — expect no output
grep -r "on_restore_complete" /home/titanous/vm-platform/libkrun/.worktrees/vmgenid/src/
grep -r "on_restore_complete\|restore_rng\|reseed" /home/titanous/vm-platform/libkrun/.worktrees/vmgenid/src/devices/src/virtio/rng/

# Build verification with snapshot feature (Phase 5 restore flow)
cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo build --features snapshot

# Build verification with uffd feature (Phase 5 UFFD restore path)
cd /home/titanous/vm-platform/libkrun/.worktrees/vmgenid && cargo build --features uffd
```
