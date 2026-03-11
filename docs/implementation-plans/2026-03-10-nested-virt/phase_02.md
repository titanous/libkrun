# Nested Virtualization Implementation Plan

**Goal:** Expose `enable_nested_virt()` on the public Builder API.

**Architecture:** A single method on Builder sets `nested_enabled = true` on VmResources, following the existing `enable_balloon()` pattern.

**Tech Stack:** Rust

**Scope:** 4 phases from original design (phases 1-4)

**Codebase verified:** 2026-03-10

---

## Acceptance Criteria Coverage

This phase implements and tests:

### nested-virt.AC2: Builder API
- **nested-virt.AC2.1 Success:** `Builder::enable_nested_virt()` sets nested_enabled on VmResources
- **nested-virt.AC2.2 Success:** nested_enabled defaults to false (disabled by default)

---

<!-- START_TASK_1 -->
### Task 1: Add `enable_nested_virt()` to Builder

**Verifies:** nested-virt.AC2.1, nested-virt.AC2.2

**Files:**
- Modify: `src/libkrun/src/lib.rs:972-976` (add method after `enable_balloon()`)

**Implementation:**

Add the following method to the Builder impl block, after `enable_balloon()` (around line 976):

```rust
/// Enables nested virtualization support, exposing VMX (Intel) or SVM (AMD)
/// capabilities to the guest so it can act as a hypervisor.
pub fn enable_nested_virt(&mut self) -> &mut Self {
    self.config.vmr.nested_enabled = true;
    self
}
```

Note: Unlike `enable_balloon()`, this method does NOT need `#[cfg(not(feature = "tee"))]` gating because `nested_enabled` is not feature-gated on VmResources. However, if TEE compatibility is a concern, the implementor should check and match the existing pattern. The design plan does not specify a feature gate.

**Testing:**

Tests must verify:
- nested-virt.AC2.1: `enable_nested_virt()` sets the flag to true
- nested-virt.AC2.2: default value is false

Add to the `mod tests` block in `src/libkrun/src/lib.rs` (following the `test_balloon_handle_ac4_2_enable_balloon_sets_flag` pattern at line 1985):

```rust
#[test]
fn test_enable_nested_virt_sets_flag() {
    let mut builder = Builder::new();
    assert_eq!(
        builder.config.vmr.nested_enabled, false,
        "nested_enabled should be false by default"
    );

    builder.enable_nested_virt();
    assert_eq!(
        builder.config.vmr.nested_enabled, true,
        "nested_enabled should be true after enable_nested_virt()"
    );
}
```

**Verification:**

Run: `cargo test -p libkrun`
Expected: All tests pass, including the new nested virt test

Run: `just check`
Expected: No warnings or errors

**Commit:** `feat: add Builder::enable_nested_virt() API method`

<!-- END_TASK_1 -->
