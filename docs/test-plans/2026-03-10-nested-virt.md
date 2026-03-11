# Nested Virtualization Test Plan

## Prerequisites
- NixOS dev shell active (`nix develop`)
- `just test` passing (unit tests green)
- `just integration` passing for baseline tests
- `/dev/kvm` accessible on the host
- Host CPU supports nested virtualization (Intel VT-x or AMD-V with nesting enabled)

## Phase 1: Unit Test Verification

| Step | Action | Expected |
|------|--------|----------|
| 1.1 | Run `just test` | All unit tests pass, including the 7 new nested-virt CPUID and Builder tests |
| 1.2 | Verify Intel CPUID test: `cargo test -p cpuid test_nested_virt_vmx` | Both `vmx_enabled` and `vmx_disabled` pass |
| 1.3 | Verify AMD CPUID tests: `cargo test -p cpuid test_nested_virt_svm test_nested_virt_npt` | All 4 AMD tests pass (`svm_enabled`, `svm_disabled`, `npt_enabled`, `npt_disabled`) |
| 1.4 | Verify Builder API test: `cargo test -p krun test_enable_nested_virt_sets_flag` | Test passes, confirming default=false and setter=true |

## Phase 2: Nix Build Verification

| Step | Action | Expected |
|------|--------|----------|
| 2.1 | Run `nix build .#libkrunfw-nested` | Build succeeds, `result/lib/libkrunfw.so` exists |
| 2.2 | Enter dev shell and check `ls -la test-prefix/lib64/libkrunfw-nested*` | `libkrunfw-nested.so` symlink exists and points to a valid file |
| 2.3 | Verify kernel includes KVM: `strings test-prefix/lib64/libkrunfw-nested.so \| grep -i "kvm"` | Output contains KVM-related strings confirming KVM is compiled in |

## Phase 3: Integration Test (Nested-Virt Supported Host)

| Step | Action | Expected |
|------|--------|----------|
| 3.1 | Confirm host nested virt: `cat /sys/module/kvm_intel/parameters/nested` (or `kvm_amd`) | Output is `1` or `Y` |
| 3.2 | Run `just integration nested-virt` | Test passes within 180s timeout |
| 3.3 | Observe test output | Output contains "OK" (not "SKIP"); no panics or assertion failures |
| 3.4 | Verify timing is reasonable | Test completes in under 120s (nested boot has overhead but should not hang) |

## Phase 4: Integration Test (Nested-Virt Unsupported Host)

| Step | Action | Expected |
|------|--------|----------|
| 4.1 | On a host without nested virt support (or temporarily disable: `echo 0 > /sys/module/kvm_intel/parameters/nested`) | Parameter reads "0" or "N" |
| 4.2 | Run `just integration nested-virt` | Test passes (exit code 0) |
| 4.3 | Observe test output | Output contains "SKIP: host does not support nested virtualization" followed by "OK" |
| 4.4 | Confirm no error messages or panics in output | Clean skip, no stack traces |

## End-to-End: Full L0->L1->L2 Verification Chain

| Step | Action | Expected |
|------|--------|----------|
| E2E.1 | Run `just integration nested-virt` on a nested-virt-capable host | Test passes |
| E2E.2 | Add temporary debug prints to `run_l1()` and `run_l2()` and rerun | stderr shows L1 starting, L2 running, confirming both levels execute |
| E2E.3 | Verify L1 VM config: 2 vCPUs, 1024 MB RAM, virtiofs mount with libkrun + libkrunfw-nested | L1 boots with correct resources and shared libraries |
| E2E.4 | Verify L2 VM config: L2 gets 1 vCPU, 256 MB RAM, re-uses L1 root filesystem | L2 boots and prints "OK" |

## Traceability

| Acceptance Criterion | Automated Test | Manual Step |
|----------------------|----------------|-------------|
| AC1.1 Intel VMX bit set | `test_nested_virt_vmx_enabled` | Phase 1, Step 1.2 |
| AC1.2 AMD SVM bit set | `test_nested_virt_svm_enabled` | Phase 1, Step 1.3 |
| AC1.3 AMD NPT bit set | `test_nested_virt_npt_enabled` | Phase 1, Step 1.3 |
| AC1.4 Bits not set when disabled | `test_nested_virt_vmx_disabled`, `svm_disabled`, `npt_disabled` | Phase 1, Steps 1.2-1.3 |
| AC2.1 Builder API | `test_enable_nested_virt_sets_flag` | Phase 1, Step 1.4 |
| AC2.2 Default false | `test_enable_nested_virt_sets_flag` (first assert) | Phase 1, Step 1.4 |
| AC3.1 Kernel build | N/A (Nix build) | Phase 2, Steps 2.1-2.3 |
| AC4.1 L1 /dev/kvm | `nested-virt` integration test `run_l1()` | Phase 3, Steps 3.2-3.3 |
| AC4.2 L2 boot | `nested-virt` integration test `run_l1()` + `run_l2()` | Phase 3, Step 3.2 |
| AC4.3 Verification chain | `nested-virt` integration test `check()` | E2E Steps |
| AC4.4 Skip on unsupported | `nested-virt` integration test `host_supports_nested_virt()` | Phase 4, Steps 4.1-4.4 |
