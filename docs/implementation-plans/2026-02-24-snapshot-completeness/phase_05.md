# Snapshot Completeness Implementation Plan — Phase 5

**Goal:** kvmclock_ctrl and TSC frequency for correct guest time behavior after restore.

**Architecture:** Add `tsc_khz: Option<u32>` to the x86_64 `VcpuState` struct. Save TSC frequency via `KVM_GET_TSC_KHZ` in the save path. Call `kvmclock_ctrl()` at the end of the restore path to notify the guest of time discontinuity. Both APIs are available in kvm-ioctls 0.22.0 (current pinned version).

**Tech Stack:** Rust (vmm crate, kvm-ioctls 0.22.0)

**Scope:** 7 phases from original design (this is phase 5 of 7)

**Codebase verified:** 2026-02-24

---

## Acceptance Criteria Coverage

This phase implements and tests:

### snapshot-completeness.AC3: kvmclock_ctrl on x86_64 restore
- **snapshot-completeness.AC3.1 Success:** kvmclock_ctrl() is called after each vCPU restore on x86_64
- **snapshot-completeness.AC3.2 Failure:** kvmclock_ctrl() failure logs warning but does not fail the restore

### snapshot-completeness.AC4: TSC frequency saved
- **snapshot-completeness.AC4.1 Success:** x86_64 VcpuState includes tsc_khz after save
- **snapshot-completeness.AC4.2 Edge:** tsc_khz is None when KVM_GET_TSC_KHZ is not supported (no error)

---

<!-- START_TASK_1 -->
### Task 1: Add tsc_khz to VcpuState and save it

**Verifies:** snapshot-completeness.AC4.1, snapshot-completeness.AC4.2

**Files:**
- Modify: `src/vmm/src/linux/vstate.rs:1848-1862` (VcpuState struct)
- Modify: `src/vmm/src/linux/vstate.rs:1346-1407` (save_state method)

**Implementation:**

1. Add `tsc_khz` field to `VcpuState` struct at line 1862 (before the closing brace):

```rust
#[cfg(all(target_arch = "x86_64", feature = "snapshot"))]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct VcpuState {
    cpuid: CpuId,
    msrs: Msrs,
    debug_regs: kvm_debugregs,
    lapic: kvm_lapic_state,
    mp_state: kvm_mp_state,
    regs: kvm_regs,
    sregs: kvm_sregs,
    vcpu_events: kvm_vcpu_events,
    xcrs: kvm_xcrs,
    xsave: kvm_xsave,
    #[serde(default)]
    tsc_khz: Option<u32>,
}
```

The `#[serde(default)]` attribute ensures backward compatibility — old snapshots without `tsc_khz` deserialize as `None` instead of failing (forward compat).

2. In `save_state()`, after the `get_vcpu_events` call (line 1394), get TSC frequency:

```rust
let tsc_khz = match self.fd.get_tsc_khz() {
    Ok(khz) => Some(khz),
    Err(e) => {
        warn!("Could not get TSC frequency: {e} (host may have unstable TSC)");
        None // AC4.2: gracefully handle unsupported
    }
};
```

3. Include `tsc_khz` in the `VcpuState` constructor return at line 1395.

**Verification:**

Build: `cargo build -p vmm --features snapshot`

**Commit:** `feat: save TSC frequency in x86_64 VcpuState`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add kvmclock_ctrl call to vCPU restore path

**Verifies:** snapshot-completeness.AC3.1, snapshot-completeness.AC3.2

**Files:**
- Modify: `src/vmm/src/linux/vstate.rs:1409-1460` (restore_state method)

**Implementation:**

At the end of `restore_state()`, after `set_vcpu_events` (line 1458) and before `Ok(())` (line 1459), add the kvmclock_ctrl call. Note: `restore_state()` runs inside each vCPU thread's message handler, so this correctly notifies each vCPU individually:

```rust
// Notify guest of time discontinuity after restore (AC3.1)
if let Err(e) = self.fd.kvmclock_ctrl() {
    // AC3.2: Log warning but do not fail restore
    warn!("kvmclock_ctrl failed for vCPU {}: {e} (older kernels may not support this)", self.id);
}
```

This is a warning-only call per AC3.2. EINVAL from older kernels is expected.

**Verification:**

Build: `cargo build -p vmm --features snapshot`

Run: `cargo test -p vmm --features snapshot`

Expected: All existing tests pass.

**Commit:** `feat: call kvmclock_ctrl after x86_64 vCPU restore`
<!-- END_TASK_2 -->
