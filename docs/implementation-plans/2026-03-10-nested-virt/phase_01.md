# Nested Virtualization Implementation Plan

**Goal:** Thread `nested_enabled` through VcpuConfig and VmSpec to the CPUID transformer pipeline, and set the correct CPUID bits for Intel VMX and AMD SVM/NPT.

**Architecture:** A boolean `nested_enabled` flag flows from `VmResources` → `VcpuConfig` → `VmSpec` → per-vendor CPUID transformer functions. On Intel, leaf 0x1 ECX bit 5 (VMX) is set. On AMD, leaf 0x80000001 ECX bit 2 (SVM) and leaf 0x8000000A EDX bit 0 (NPT) are set. KVM automatically exposes capability MSRs when these CPUID bits are present.

**Tech Stack:** Rust, KVM, x86 CPUID

**Scope:** 4 phases from original design (phases 1-4)

**Codebase verified:** 2026-03-10

---

## Acceptance Criteria Coverage

This phase implements and tests:

### nested-virt.AC1: CPUID bits exposed correctly
- **nested-virt.AC1.1 Success:** On Intel hosts, CPUID leaf 0x1 ECX bit 5 (VMX) is set when nested_enabled=true
- **nested-virt.AC1.2 Success:** On AMD hosts, CPUID leaf 0x80000001 ECX bit 2 (SVM) is set when nested_enabled=true
- **nested-virt.AC1.3 Success:** On AMD hosts, CPUID leaf 0x8000000A EDX bit 0 (NPT) is set when nested_enabled=true
- **nested-virt.AC1.4 Failure:** VMX/SVM/NPT bits are NOT set when nested_enabled=false (default)

---

<!-- START_SUBCOMPONENT_A (tasks 1-4) -->

<!-- START_TASK_1 -->
### Task 1: Add `nested_enabled` to VmSpec and VcpuConfig

**Verifies:** None (infrastructure plumbing)

**Files:**
- Modify: `src/cpuid/src/transformer/mod.rs:15-47` (VmSpec struct + new())
- Modify: `src/vmm/src/linux/vstate.rs:988-996` (VcpuConfig struct)
- Modify: `src/vmm/src/resources.rs:267-275` (VmResources::vcpu_config())
- Modify: `src/vmm/src/linux/vstate.rs:1271` (VmSpec::new() call site)

**Implementation:**

**1. Add `nested_enabled` field to `VmSpec` (`src/cpuid/src/transformer/mod.rs`):**

In the `VmSpec` struct (line 15), add after `ht_enabled`:
```rust
/// Whether nested virtualization is enabled.
nested_enabled: bool,
```

Update `VmSpec::new()` (line 31) to accept and store the new parameter:
```rust
pub fn new(cpu_id: u8, cpu_count: u8, ht_enabled: bool, nested_enabled: bool) -> Result<VmSpec, Error> {
    let cpu_vendor_id = get_vendor_id().map_err(Error::InternalError)?;

    Ok(VmSpec {
        cpu_vendor_id,
        cpu_id,
        cpu_count,
        ht_enabled,
        nested_enabled,
        brand_string: BrandString::from_vendor_id(&cpu_vendor_id),
    })
}
```

Add an accessor method after `cpu_vendor_id()`:
```rust
/// Returns whether nested virtualization is enabled
pub fn nested_enabled(&self) -> bool {
    self.nested_enabled
}
```

**2. Add `nested_enabled` field to `VcpuConfig` (`src/vmm/src/linux/vstate.rs:988-996`):**

```rust
pub struct VcpuConfig {
    pub vcpu_count: u8,
    pub ht_enabled: bool,
    pub cpu_template: Option<CpuFeaturesTemplate>,
    pub nested_enabled: bool,
}
```

**3. Pass `nested_enabled` through `VmResources::vcpu_config()` (`src/vmm/src/resources.rs:267-275`):**

```rust
pub fn vcpu_config(&self) -> VcpuConfig {
    VcpuConfig {
        vcpu_count: self.vm_config().vcpu_count.unwrap(),
        ht_enabled: self.vm_config().ht_enabled.unwrap(),
        cpu_template: self.vm_config().cpu_template,
        nested_enabled: self.nested_enabled,
    }
}
```

**4. Update `configure_x86_64()` call to `VmSpec::new()` (`src/vmm/src/linux/vstate.rs:1271`):**

```rust
let cpuid_vm_spec = VmSpec::new(self.id, vcpu_config.vcpu_count, vcpu_config.ht_enabled, vcpu_config.nested_enabled)
    .map_err(Error::CpuId)?;
```

**5. Fix all existing `VmSpec::new()` calls** in test code (they currently pass 3 args, need 4):

All existing `VmSpec::new(id, count, ht)` calls must become `VmSpec::new(id, count, ht, false)`. These appear in:
- `src/cpuid/src/transformer/mod.rs` tests (line 120)
- `src/cpuid/src/transformer/intel.rs` tests (lines 222, 241, 269, 301)
- `src/cpuid/src/transformer/amd.rs` tests (lines 162, 187, 213, 233, 266, 309)
- `src/cpuid/src/transformer/common.rs` tests (lines 154, 179)

**Verification:**

Run: `cargo test -p cpuid -p vmm`
Expected: All existing tests compile and pass (no behavior change, all new args are `false`)

**Commit:** `refactor: thread nested_enabled through VcpuConfig and VmSpec`

<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add VMX constant and set VMX bit in Intel transformer

**Verifies:** nested-virt.AC1.1, nested-virt.AC1.4 (Intel side)

**Files:**
- Modify: `src/cpuid/src/cpu_leaf.rs:30-57` (add VMX_BITINDEX constant to leaf_0x1::ecx)
- Modify: `src/cpuid/src/transformer/intel.rs:12-30` (update_feature_info_entry)

**Implementation:**

**1. Add VMX constant to `cpu_leaf.rs`** in the `leaf_0x1::ecx` module (after line 36, where the comment already says "5 = VMX"):

```rust
// VMX = Virtual Machine Extensions (nested virtualization)
pub const VMX_BITINDEX: u32 = 5;
```

**2. Set VMX bit in `update_feature_info_entry()` (`src/cpuid/src/transformer/intel.rs`):**

Add after the TSC_DEADLINE_TIMER line (line 27):
```rust
entry.ecx.write_bit(ecx::VMX_BITINDEX, vm_spec.nested_enabled());
```

**Testing:**

Tests must verify:
- nested-virt.AC1.1: CPUID leaf 0x1 ECX bit 5 (VMX) is set when `nested_enabled=true`
- nested-virt.AC1.4 (Intel): VMX bit is NOT set when `nested_enabled=false`

Add to `src/cpuid/src/transformer/intel.rs` in `mod tests`:

```rust
#[test]
fn test_nested_virt_vmx_enabled() {
    use crate::cpu_leaf::leaf_0x1::*;

    let vm_spec = VmSpec::new(0, 1, false, true).expect("Error creating vm_spec");
    let mut entry = kvm_cpuid_entry2 {
        function: leaf_0x1::LEAF_NUM,
        index: 0,
        flags: 0,
        eax: 0,
        ebx: 0,
        ecx: 0,
        edx: 0,
        padding: [0, 0, 0],
    };

    assert!(update_feature_info_entry(&mut entry, &vm_spec).is_ok());
    assert!(entry.ecx.read_bit(ecx::VMX_BITINDEX));
}

#[test]
fn test_nested_virt_vmx_disabled() {
    use crate::cpu_leaf::leaf_0x1::*;

    let vm_spec = VmSpec::new(0, 1, false, false).expect("Error creating vm_spec");
    let mut entry = kvm_cpuid_entry2 {
        function: leaf_0x1::LEAF_NUM,
        index: 0,
        flags: 0,
        eax: 0,
        ebx: 0,
        ecx: 0,
        edx: 0,
        padding: [0, 0, 0],
    };

    assert!(update_feature_info_entry(&mut entry, &vm_spec).is_ok());
    assert!(!entry.ecx.read_bit(ecx::VMX_BITINDEX));
}
```

**Verification:**

Run: `cargo test -p cpuid`
Expected: All tests pass, including the two new nested virt tests

**Commit:** `feat: expose VMX CPUID bit for nested virtualization on Intel`

<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Add SVM/NPT constants and set bits in AMD transformer

**Verifies:** nested-virt.AC1.2, nested-virt.AC1.3, nested-virt.AC1.4 (AMD side)

**Files:**
- Modify: `src/cpuid/src/cpu_leaf.rs:267-279` (add SVM_BITINDEX to leaf_0x80000001::ecx)
- Modify: `src/cpuid/src/cpu_leaf.rs` (add new leaf_0x8000000A module)
- Modify: `src/cpuid/src/transformer/amd.rs:52-62` (update_extended_feature_info_entry — add SVM bit)
- Modify: `src/cpuid/src/transformer/amd.rs:129-151` (add new handler + dispatch entry for leaf 0x8000000A)

**Implementation:**

**1. Add SVM constant to `leaf_0x80000001::ecx` in `cpu_leaf.rs` (around line 272):**

```rust
pub const SVM_BITINDEX: u32 = 2; // Secure Virtual Machine
```

**2. Add new `leaf_0x8000000a` module to `cpu_leaf.rs`** (after `leaf_0x80000008` module, before `leaf_0x8000001d`):

```rust
// SVM Features Leaf
pub mod leaf_0x8000000a {
    pub const LEAF_NUM: u32 = 0x8000_000a;

    pub mod edx {
        pub const NPT_BITINDEX: u32 = 0; // Nested Page Tables
    }
}
```

**3. Update `update_extended_feature_info_entry()` in `amd.rs` (lines 52-62)** to set SVM bit:

```rust
pub fn update_extended_feature_info_entry(
    entry: &mut kvm_cpuid_entry2,
    vm_spec: &VmSpec,
) -> Result<(), Error> {
    use crate::cpu_leaf::leaf_0x80000001::*;

    // set the Topology Extension bit since we use the Extended Cache Topology leaf
    entry.ecx.write_bit(ecx::TOPOEXT_INDEX, true);

    // set the SVM bit when nested virtualization is enabled
    entry.ecx.write_bit(ecx::SVM_BITINDEX, vm_spec.nested_enabled());

    Ok(())
}
```

Note: the function signature changes from `_vm_spec: &VmSpec` to `vm_spec: &VmSpec` (remove underscore since it's now used).

**4. Add new `update_svm_features_entry()` handler in `amd.rs`** (after `update_extended_feature_info_entry`):

```rust
pub fn update_svm_features_entry(
    entry: &mut kvm_cpuid_entry2,
    vm_spec: &VmSpec,
) -> Result<(), Error> {
    use crate::cpu_leaf::leaf_0x8000000a::*;

    // set the NPT (Nested Page Tables) bit when nested virtualization is enabled
    entry.edx.write_bit(edx::NPT_BITINDEX, vm_spec.nested_enabled());

    Ok(())
}
```

**5. Register the new handler in `entry_transformer_fn()`** in `AmdCpuidTransformer` (around line 138):

Add after the `leaf_0x80000008` line:
```rust
leaf_0x8000000a::LEAF_NUM => Some(amd::update_svm_features_entry),
```

**Testing:**

Tests must verify:
- nested-virt.AC1.2: leaf 0x80000001 ECX bit 2 (SVM) set when `nested_enabled=true`
- nested-virt.AC1.3: leaf 0x8000000A EDX bit 0 (NPT) set when `nested_enabled=true`
- nested-virt.AC1.4 (AMD): SVM and NPT bits NOT set when `nested_enabled=false`

Add to `src/cpuid/src/transformer/amd.rs` in `mod tests`:

```rust
#[test]
fn test_nested_virt_svm_enabled() {
    use crate::cpu_leaf::leaf_0x80000001::*;

    let vm_spec = VmSpec::new(0, 1, false, true).expect("Error creating vm_spec");
    let mut entry = kvm_cpuid_entry2 {
        function: LEAF_NUM,
        index: 0,
        flags: 0,
        eax: 0,
        ebx: 0,
        ecx: 0,
        edx: 0,
        padding: [0, 0, 0],
    };

    assert!(update_extended_feature_info_entry(&mut entry, &vm_spec).is_ok());
    assert!(entry.ecx.read_bit(ecx::SVM_BITINDEX));
    assert!(entry.ecx.read_bit(ecx::TOPOEXT_INDEX));
}

#[test]
fn test_nested_virt_svm_disabled() {
    use crate::cpu_leaf::leaf_0x80000001::*;

    let vm_spec = VmSpec::new(0, 1, false, false).expect("Error creating vm_spec");
    let mut entry = kvm_cpuid_entry2 {
        function: LEAF_NUM,
        index: 0,
        flags: 0,
        eax: 0,
        ebx: 0,
        ecx: 0,
        edx: 0,
        padding: [0, 0, 0],
    };

    assert!(update_extended_feature_info_entry(&mut entry, &vm_spec).is_ok());
    assert!(!entry.ecx.read_bit(ecx::SVM_BITINDEX));
    assert!(entry.ecx.read_bit(ecx::TOPOEXT_INDEX));
}

#[test]
fn test_nested_virt_npt_enabled() {
    use crate::cpu_leaf::leaf_0x8000000a::*;

    let vm_spec = VmSpec::new(0, 1, false, true).expect("Error creating vm_spec");
    let mut entry = kvm_cpuid_entry2 {
        function: LEAF_NUM,
        index: 0,
        flags: 0,
        eax: 0,
        ebx: 0,
        ecx: 0,
        edx: 0,
        padding: [0, 0, 0],
    };

    assert!(update_svm_features_entry(&mut entry, &vm_spec).is_ok());
    assert!(entry.edx.read_bit(edx::NPT_BITINDEX));
}

#[test]
fn test_nested_virt_npt_disabled() {
    use crate::cpu_leaf::leaf_0x8000000a::*;

    let vm_spec = VmSpec::new(0, 1, false, false).expect("Error creating vm_spec");
    let mut entry = kvm_cpuid_entry2 {
        function: LEAF_NUM,
        index: 0,
        flags: 0,
        eax: 0,
        ebx: 0,
        ecx: 0,
        edx: 0,
        padding: [0, 0, 0],
    };

    assert!(update_svm_features_entry(&mut entry, &vm_spec).is_ok());
    assert!(!entry.edx.read_bit(edx::NPT_BITINDEX));
}
```

**Verification:**

Run: `cargo test -p cpuid`
Expected: All tests pass, including the four new AMD nested virt tests

**Commit:** `feat: expose SVM and NPT CPUID bits for nested virtualization on AMD`

<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Run full test suite

**Verifies:** nested-virt.AC1.1, nested-virt.AC1.2, nested-virt.AC1.3, nested-virt.AC1.4

**Step 1: Run all unit tests**

Run: `just test`
Expected: All tests pass

**Step 2: Run clippy**

Run: `just check`
Expected: No warnings or errors

**Commit:** None (verification only)

<!-- END_TASK_4 -->

<!-- END_SUBCOMPONENT_A -->
