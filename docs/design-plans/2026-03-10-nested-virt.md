# Nested Virtualization Support Design

## Summary

Nested virtualization support adds the ability for a VM started by libkrun (the L1 guest) to itself act as a hypervisor and run further VMs (L2 guests). This is accomplished through a minimal, CPUID-only approach: when a caller invokes `Builder::enable_nested_virt()`, the VMM sets the appropriate CPUID feature bits during vCPU configuration so the L1 guest kernel detects hardware virtualization support and loads KVM modules. KVM on the host then handles all the complex mechanics — VMX/SVM MSR exposure, VMCS management, and L2-to-host exit routing — without any additional work from libkrun.

The implementation threads a single `nested_enabled` boolean from the public `Builder` API through the internal configuration pipeline (`VmResources` → `VcpuConfig` → `VmSpec`) into the per-vendor CPUID transformer, where Intel VMX and AMD SVM/NPT bits are conditionally set. A companion `libkrunfw-nested` kernel variant (built with `CONFIG_KVM=y`) is added to the Nix flake to provide a guest kernel that includes `/dev/kvm` support. An end-to-end integration test closes the loop by running a three-level chain (host → L1 → L2) and verifying that an exit code propagates correctly up through all levels.

## Definition of Done
1. A `Builder::enable_nested_virt()` method that configures the VM to expose nested virtualization capabilities (VMX on Intel, SVM on AMD) to the guest, disabled by default
2. Correct CPUID bit exposure and MSR passthrough so that a guest can load KVM modules and use `/dev/kvm`
3. A `libkrunfw-nested` kernel variant in `flake.nix` with `CONFIG_KVM=y`, `CONFIG_KVM_AMD=y`, `CONFIG_KVM_INTEL=y`, used by the nested integration test
4. An integration test that boots an L1 VM with nested virt enabled, the L1 guest uses libkrun (shared via virtiofs) to boot an L2 VM, the L2 runs guest code and reports a result, and the full chain (host → L1 → L2) verifies correctness

## Acceptance Criteria

### nested-virt.AC1: CPUID bits exposed correctly
- **nested-virt.AC1.1 Success:** On Intel hosts, CPUID leaf 0x1 ECX bit 5 (VMX) is set when nested_enabled=true
- **nested-virt.AC1.2 Success:** On AMD hosts, CPUID leaf 0x80000001 ECX bit 2 (SVM) is set when nested_enabled=true
- **nested-virt.AC1.3 Success:** On AMD hosts, CPUID leaf 0x8000000A EDX bit 0 (NPT) is set when nested_enabled=true
- **nested-virt.AC1.4 Failure:** VMX/SVM/NPT bits are NOT set when nested_enabled=false (default)

### nested-virt.AC2: Builder API
- **nested-virt.AC2.1 Success:** `Builder::enable_nested_virt()` sets nested_enabled on VmResources
- **nested-virt.AC2.2 Success:** nested_enabled defaults to false (disabled by default)

### nested-virt.AC3: Guest kernel with KVM
- **nested-virt.AC3.1 Success:** libkrunfw-nested kernel variant builds and includes KVM module support

### nested-virt.AC4: End-to-end nested boot
- **nested-virt.AC4.1 Success:** L1 guest with nested virt enabled can access /dev/kvm
- **nested-virt.AC4.2 Success:** L1 guest runs libkrun to boot L2 VM, L2 executes guest code and exits with expected code
- **nested-virt.AC4.3 Success:** Full verification chain passes: L2 exits 42 → L1 asserts and exits 0 → host asserts L1 exits 0
- **nested-virt.AC4.4 Edge:** Test skips cleanly on hosts without nested virt support (no error, prints skip message)

## Glossary

- **Nested virtualization**: The ability for a guest VM to itself act as a hypervisor and run further guest VMs using hardware-assisted virtualization. Requires explicit host support and CPUID bit exposure by the VMM.
- **L0 / L1 / L2**: Conventional labels for virtualization nesting levels. L0 is the bare-metal host running KVM; L1 is the first-level guest VM (which acts as a hypervisor); L2 is a VM launched by L1.
- **VMX (Virtual Machine Extensions)**: Intel's hardware virtualization instruction set. Guests detect VMX support via CPUID leaf 0x1 ECX bit 5.
- **SVM (Secure Virtual Machine)**: AMD's hardware virtualization instruction set. Guests detect SVM support via CPUID leaf 0x80000001 ECX bit 2.
- **NPT (Nested Page Tables)**: AMD's hardware two-dimensional page table mechanism. Guests detect NPT support via CPUID leaf 0x8000000A EDX bit 0.
- **CPUID**: x86 instruction that returns CPU feature information indexed by a leaf number. VMMs intercept CPUID and can add or mask bits before returning them to the guest.
- **CPUID transformer pipeline**: libkrun's internal mechanism (`filter_cpuid()` in `src/cpuid/src/transformer/`) that applies per-vendor, per-leaf transformation functions to raw CPUID values before loading them into vCPUs via `KVM_SET_CPUID2`.
- **VmSpec**: Internal struct carrying per-vCPU configuration into the CPUID transformer. `nested_enabled` is being added here.
- **VcpuConfig**: Internal struct holding per-vCPU configuration derived from `VmResources`. Bridge between VMM configuration and KVM vCPU setup.
- **VmResources**: Central configuration store for a libkrun VM, populated from `Builder` calls before the VM is built.
- **MSR (Model-Specific Register)**: x86 registers accessed via `RDMSR`/`WRMSR`. VMX and SVM capability MSRs are automatically exposed by KVM once the relevant CPUID bits are set.
- **VMCS (Virtual Machine Control Structure)**: Intel data structure storing VMX guest/host state. KVM manages VMCS shadowing transparently when nested is enabled.
- **virtiofs**: A virtio device that exposes a host directory as a filesystem to the guest. Used in the integration test to share libraries into the L1 guest.
- **libkrunfw**: Companion shared library bundling the guest kernel loaded by libkrun. `libkrunfw-nested` is a new variant built with KVM modules enabled.
- **`KVM_SET_CPUID2`**: Linux KVM ioctl that installs a caller-supplied CPUID table into a vCPU.
- **`#[host]` / `#[guest]` proc macros**: libkrun integration test macros that split a test into host-side setup code and guest-side code that runs inside the VM.
- **`KRUN_NESTING_LEVEL`**: Environment variable used in the nested integration test to let a single binary detect whether it is running as L0, L1, or L2.

## Architecture

Nested virtualization allows a guest VM (L1) to run its own VMs (L2) using hardware-assisted virtualization. KVM handles the complex emulation of VMX/SVM instructions — the VMM's job is to expose the right CPUID bits so the guest kernel detects and enables its hypervisor support.

**Approach:** Minimal CPUID-only. When `nested_enabled` is true on the Builder, set the appropriate CPUID bits during vCPU configuration. KVM automatically exposes VMX/SVM capability MSRs to the guest when these CPUID bits are present — no explicit MSR whitelisting needed.

**Config flow:**

```
Builder::enable_nested_virt()
  → self.config.vmr.nested_enabled = true    (already exists)
  → VmResources.nested_enabled               (already exists)
  → VcpuConfig.nested_enabled                (NEW field)
  → VmSpec.nested_enabled                    (NEW field)
  → filter_cpuid() transformer pipeline      (NEW logic)
  → KVM_SET_CPUID2                           (existing)
```

**CPUID modifications (Intel):**
- Leaf 0x1, ECX bit 5 (VMX) — set to true when `vm_spec.nested_enabled`
- Added in `src/cpuid/src/transformer/intel.rs` `update_feature_info_entry()`

**CPUID modifications (AMD):**
- Leaf 0x80000001, ECX bit 2 (SVM) — set to true when `vm_spec.nested_enabled`
- Leaf 0x8000000A, EDX bit 0 (NPT) — set to true when `vm_spec.nested_enabled`
- New transformer entries in `src/cpuid/src/transformer/amd.rs`

**What KVM handles automatically:**
- VMX capability MSRs (0x480–0x48B) — exposed when guest CPUID has VMX
- VMCS shadow management — transparent to the VMM
- L2 → L0 exit handling — fully managed by host KVM
- CR4.VMXE / EFER.SVME — guest sets these itself after detecting VMX/SVM via CPUID

**Integration test (L0 → L1 → L2):**

The test uses a single binary that checks `KRUN_NESTING_LEVEL` to determine behavior:
- Level 0 (host): builds L1 VM with `enable_nested_virt()`, shares libraries via virtiofs
- Level 1 (L1 guest): loads libkrun from virtiofs, builds minimal L2 VM, runs it
- Level 2 (L2 guest): runs trivial workload, exits with magic exit code 42

Verification: L2 exits 42 → L1 asserts and exits 0 → host asserts L1 exits 0.

## Existing Patterns

**Builder API pattern:** `enable_nested_virt()` follows the same pattern as `enable_balloon()` in `src/libkrun/src/lib.rs` — a method that sets a bool on `self.config.vmr` and returns `&mut Self`.

**VmResources → VcpuConfig flow:** `nested_enabled` already exists on `VmResources` (defaults to `false`) and propagates to `Vmm.nested_enabled` and `SnapshotHeader.nested_enabled`. Adding it to `VcpuConfig` follows the existing pattern of `vcpu_count`, `ht_enabled`, and `cpu_template`.

**CPUID transformer pipeline:** The per-vendor transformer dispatch in `intel.rs` and `amd.rs` (`entry_transformer_fn()`) returns `Option<EntryTransformerFn>` for each leaf. Adding new leaf handlers follows the existing pattern (e.g., `leaf_0x1::LEAF_NUM => Some(update_feature_info_entry)`).

**VmSpec:** Currently carries `cpu_id`, `cpu_count`, `ht_enabled`, `brand_string`. Adding `nested_enabled: bool` follows this pattern.

**Snapshot header:** `nested_enabled` is already validated on restore via `check_nested_enabled()`. No changes needed — the existing validation ensures snapshot restore rejects mismatched nested state.

**Integration test pattern:** `#[host]`/`#[guest]` proc macros with `Test` trait (`start_vm` on host, `in_guest` on guest). The nested test extends this by having the guest code itself use the libkrun Builder API.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: CPUID Nested Virt Plumbing

**Goal:** Thread `nested_enabled` through VcpuConfig and VmSpec to the CPUID transformer pipeline, and set the correct CPUID bits for Intel VMX and AMD SVM.

**Components:**
- `VcpuConfig` in `src/vmm/src/linux/vstate.rs` — add `nested_enabled: bool` field
- `VmResources::vcpu_config()` in `src/vmm/src/resources.rs` — populate `nested_enabled` from `self.nested_enabled`
- `VmSpec` in `src/cpuid/src/transformer/mod.rs` — add `nested_enabled: bool` field
- `VmSpec::new()` — accept and store `nested_enabled`
- `cpu_leaf.rs` in `src/cpuid/src/cpu_leaf.rs` — add `VMX_BITINDEX` constant (leaf 0x1 ECX bit 5)
- `src/cpuid/src/transformer/intel.rs` `update_feature_info_entry()` — set VMX bit when `vm_spec.nested_enabled`
- `src/cpuid/src/transformer/amd.rs` — add transformer for leaf 0x80000001 (SVM bit 2) and leaf 0x8000000A (NPT bit 0)
- `src/cpuid/src/transformer/amd.rs` `entry_transformer_fn()` — register new leaf handlers
- `configure_x86_64()` in `src/vmm/src/linux/vstate.rs` — pass `nested_enabled` when constructing VmSpec

**Dependencies:** None

**Done when:** Unit tests verify that CPUID leaf 0x1 ECX bit 5 is set on Intel (and unset when disabled), leaf 0x80000001 ECX bit 2 and leaf 0x8000000A EDX bit 0 are set on AMD (and unset when disabled). Covers `nested-virt.AC1.1`, `nested-virt.AC1.2`, `nested-virt.AC1.3`, `nested-virt.AC1.4`.
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: Builder API

**Goal:** Expose `enable_nested_virt()` on the public Builder API.

**Components:**
- `Builder::enable_nested_virt()` in `src/libkrun/src/lib.rs` — sets `self.config.vmr.nested_enabled = true`, returns `&mut Self`

**Dependencies:** Phase 1 (CPUID plumbing must exist for the flag to have effect)

**Done when:** Unit test verifies `enable_nested_virt()` sets the flag on VmResources. Covers `nested-virt.AC2.1`, `nested-virt.AC2.2`.
<!-- END_PHASE_2 -->

<!-- START_PHASE_3 -->
### Phase 3: libkrunfw-nested Kernel Variant

**Goal:** Create a libkrunfw kernel variant with KVM guest support compiled in.

**Components:**
- `flake.nix` — new `libkrunfw-nested` derivation based on `libkrunfw-vmgenid`, adding `CONFIG_KVM=y`, `CONFIG_KVM_INTEL=y`, `CONFIG_KVM_AMD=y` to the kernel config
- `flake.nix` shellHook — symlink `libkrunfw-nested` into test prefix for integration tests

**Dependencies:** None (independent of Phases 1-2)

**Done when:** `nix build .#libkrunfw-nested` succeeds and the resulting kernel supports `/dev/kvm` in a guest VM. Covers `nested-virt.AC3.1`.
<!-- END_PHASE_3 -->

<!-- START_PHASE_4 -->
### Phase 4: Integration Test

**Goal:** End-to-end test proving nested virtualization works: host → L1 → L2.

**Components:**
- New test case in `tests/test_cases/src/` — `test_nested_virt.rs`
- Test registration in `tests/test_cases/src/lib.rs`
- Host side: builds L1 VM with `enable_nested_virt()`, shares `test-prefix/lib64/` via virtiofs (contains libkrun.so + libkrunfw-nested)
- L1 guest side: sets `LD_LIBRARY_PATH`, uses libkrun Builder API to build and run L2 VM with the nested libkrunfw
- L2 guest side: runs trivial workload, exits with code 42
- L1 guest side: asserts L2 exit code is 42, exits with code 0
- Host side: asserts L1 exit code is 0
- Test gating: skip if host `/sys/module/kvm_*/parameters/nested` is not `1` or `Y`
- Timeout: 120s (nested virt has significant overhead)

**Dependencies:** Phase 1 (CPUID bits), Phase 2 (Builder API), Phase 3 (nested kernel)

**Done when:** `just integration nested-virt` passes on a host with nested virt support, and skips cleanly on hosts without it. Covers `nested-virt.AC4.1`, `nested-virt.AC4.2`, `nested-virt.AC4.3`, `nested-virt.AC4.4`.
<!-- END_PHASE_4 -->

## Additional Considerations

**Test gating:** The nested virt integration test must check host support before attempting to run. Running on a host without `nested=1` would produce confusing KVM errors rather than a clear skip message.

**L1 guest library loading:** The L1 guest needs `LD_LIBRARY_PATH` set to find libkrun.so and libkrunfw.so from the virtiofs mount. The init binary sets up the environment before executing the guest test binary, so the library path must be configured via the virtiofs directory structure or an env var passed through.

**L2 kernel:** The L2 VM also needs a libkrunfw kernel. The same `libkrunfw-nested` shared via virtiofs serves double duty — it's used by the L1 guest's libkrun to boot L2. The standard (non-nested) libkrunfw would also work for L2 since L2 doesn't need KVM support itself.
