# Nested Virtualization Implementation Plan

**Goal:** Create a `libkrunfw-nested` kernel variant with KVM guest support compiled in.

**Architecture:** A new Nix derivation extends `libkrunfw-vmgenid` by appending `CONFIG_KVM=y`, `CONFIG_KVM_INTEL=y`, `CONFIG_KVM_AMD=y` to the kernel config. The shellHook symlinks it alongside the standard libkrunfw for integration test access.

**Tech Stack:** Nix, Linux kernel config

**Scope:** 4 phases from original design (phases 1-4)

**Codebase verified:** 2026-03-10

---

## Acceptance Criteria Coverage

This phase implements and tests:

### nested-virt.AC3: Guest kernel with KVM
- **nested-virt.AC3.1 Success:** libkrunfw-nested kernel variant builds and includes KVM module support

---

<!-- START_TASK_1 -->
### Task 1: Add `libkrunfw-nested` derivation to flake.nix

**Verifies:** None (infrastructure)

**Files:**
- Modify: `flake.nix:217-220` (add derivation after `libkrunfw-vmgenid`, add package export)
- Modify: `flake.nix:312-315` (add shellHook symlink for nested variant)

**Implementation:**

**1. Add the derivation** after the `libkrunfw-vmgenid` closing `});` (line 217), before the `in` keyword (line 218):

```nix
        # libkrunfw variant with KVM support for nested virtualization.
        # The L1 guest kernel needs CONFIG_KVM to expose /dev/kvm so it can
        # act as a hypervisor and run L2 VMs.
        libkrunfw-nested = libkrunfw-vmgenid.overrideAttrs (old: {
          postPatch = (old.postPatch or "") + ''
            cat >> config-libkrunfw_x86_64 <<'KCONFIG_EOF'
# KVM support for nested virtualization (L1 guest acts as hypervisor)
CONFIG_KVM=y
CONFIG_KVM_INTEL=y
CONFIG_KVM_AMD=y
KCONFIG_EOF
          '';
        });
```

This extends `libkrunfw-vmgenid` (not the base `pkgs.libkrunfw`), so all existing patches and config tweaks (VMGENID, jitterentropy removal, serial8250, filesystem removal, etc.) are preserved.

**2. Export the package** (after line 220):

```nix
        packages.libkrunfw-nested = libkrunfw-nested;
```

**3. Add shellHook symlink** for the nested variant (after the existing libkrunfw symlink loop at line 315):

```nix
            # Symlink libkrunfw-nested for nested virt integration tests.
            # Direct symlink avoids fragile sed-based renaming of versioned .so names.
            ln -sf ${libkrunfw-nested}/lib/libkrunfw.so "$(pwd)/test-prefix/lib64/libkrunfw-nested.so"
```

This creates `test-prefix/lib64/libkrunfw-nested.so` (distinct name to avoid collision with the standard `libkrunfw.so` variant). The L1 guest will load this variant via `LD_LIBRARY_PATH`.

**Verification:**

Run: `nix build .#libkrunfw-nested`
Expected: Build succeeds, produces `result/lib/libkrunfw.so`

Run: `ls -la test-prefix/lib64/libkrunfw*` (after re-entering nix shell)
Expected: Both `libkrunfw.so` (standard) and `libkrunfw-nested.so` (KVM-enabled) are present

**Commit:** `feat: add libkrunfw-nested kernel variant with KVM support`

<!-- END_TASK_1 -->
