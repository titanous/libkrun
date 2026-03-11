# Test Requirements: Nested Virtualization

## Automated Tests

### nested-virt.AC1: CPUID bits exposed correctly

**nested-virt.AC1.1 Success:** On Intel hosts, CPUID leaf 0x1 ECX bit 5 (VMX) is set when nested_enabled=true

- Test type: unit
- Test file: `src/cpuid/src/transformer/intel.rs` (mod tests)
- Test name: `test_nested_virt_vmx_enabled`
- Description: Creates a VmSpec with `nested_enabled=true`, calls `update_feature_info_entry()`, asserts ECX bit 5 is set

**nested-virt.AC1.2 Success:** On AMD hosts, CPUID leaf 0x80000001 ECX bit 2 (SVM) is set when nested_enabled=true

- Test type: unit
- Test file: `src/cpuid/src/transformer/amd.rs` (mod tests)
- Test name: `test_nested_virt_svm_enabled`
- Description: Creates a VmSpec with `nested_enabled=true`, calls `update_extended_feature_info_entry()`, asserts ECX bit 2 (SVM) is set

**nested-virt.AC1.3 Success:** On AMD hosts, CPUID leaf 0x8000000A EDX bit 0 (NPT) is set when nested_enabled=true

- Test type: unit
- Test file: `src/cpuid/src/transformer/amd.rs` (mod tests)
- Test name: `test_nested_virt_npt_enabled`
- Description: Creates a VmSpec with `nested_enabled=true`, calls `update_svm_features_entry()`, asserts EDX bit 0 (NPT) is set

**nested-virt.AC1.4 Failure:** VMX/SVM/NPT bits are NOT set when nested_enabled=false (default)

- Test type: unit
- Test files: `src/cpuid/src/transformer/intel.rs` and `src/cpuid/src/transformer/amd.rs` (mod tests)
- Test names: `test_nested_virt_vmx_disabled`, `test_nested_virt_svm_disabled`, `test_nested_virt_npt_disabled`
- Description: Creates VmSpec with `nested_enabled=false`, calls the respective transformer functions, asserts VMX (ECX bit 5), SVM (ECX bit 2), and NPT (EDX bit 0) are all NOT set

### nested-virt.AC2: Builder API

**nested-virt.AC2.1 Success:** `Builder::enable_nested_virt()` sets nested_enabled on VmResources

- Test type: unit
- Test file: `src/libkrun/src/lib.rs` (mod tests)
- Test name: `test_enable_nested_virt_sets_flag`
- Description: Calls `Builder::new()` then `enable_nested_virt()`, asserts `config.vmr.nested_enabled` is true

**nested-virt.AC2.2 Success:** nested_enabled defaults to false (disabled by default)

- Test type: unit
- Test file: `src/libkrun/src/lib.rs` (mod tests)
- Test name: `test_enable_nested_virt_sets_flag` (first assertion)
- Description: After `Builder::new()` and before calling `enable_nested_virt()`, asserts `config.vmr.nested_enabled` is false

### nested-virt.AC3: Guest kernel with KVM

**nested-virt.AC3.1 Success:** libkrunfw-nested kernel variant builds and includes KVM module support

- Test type: unit (Nix build verification)
- Test file: N/A (Nix derivation in `flake.nix`)
- Description: `nix build .#libkrunfw-nested` succeeds and produces `result/lib/libkrunfw.so`; verified by presence of `test-prefix/lib64/libkrunfw-nested.so` after shellHook runs

### nested-virt.AC4: End-to-end nested boot

**nested-virt.AC4.1 Success:** L1 guest with nested virt enabled can access /dev/kvm

- Test type: integration
- Test file: `tests/test_cases/src/test_nested_virt.rs`
- Test name: `nested-virt` (run via `just integration nested-virt`)
- Description: L1 guest asserts `/dev/kvm` exists; failure panics with "L1: /dev/kvm not found -- nested virt not working"

**nested-virt.AC4.2 Success:** L1 guest runs libkrun to boot L2 VM, L2 executes guest code and exits with expected code

- Test type: integration
- Test file: `tests/test_cases/src/test_nested_virt.rs`
- Test name: `nested-virt` (run via `just integration nested-virt`)
- Description: L1 guest uses `krun::Builder` API to build an L2 VM (1 vCPU, 256 MB), runs it with `KRUN_NESTING_LEVEL=2`; L2 guest prints "OK" to stdout

**nested-virt.AC4.3 Success:** Full verification chain passes: L2 exits 42 -> L1 asserts and exits 0 -> host asserts L1 exits 0

- Test type: integration
- Test file: `tests/test_cases/src/test_nested_virt.rs`
- Test name: `nested-virt` (run via `just integration nested-virt`)
- Description: L2 prints "OK", L1 observes L2 completion and prints its own "OK", host `check()` method asserts stdout contains "OK\n". Note: the design plan's exit-code-42 mechanism is adapted to the framework's stdout "OK" checking convention; the verification chain is functionally equivalent (L2 succeeds -> L1 succeeds -> host verifies)

**nested-virt.AC4.4 Edge:** Test skips cleanly on hosts without nested virt support (no error, prints skip message)

- Test type: integration
- Test file: `tests/test_cases/src/test_nested_virt.rs`
- Test name: `nested-virt` (run via `just integration nested-virt`)
- Description: Host-side `start_vm()` checks `/sys/module/kvm_intel/parameters/nested` and `/sys/module/kvm_amd/parameters/nested`; if neither shows "1" or "Y", prints "SKIP: host does not support nested virtualization" and "OK", returning early without error

## Human Verification

### nested-virt.AC3.1: libkrunfw-nested kernel variant builds and includes KVM module support

- **Why it cannot be fully automated:** The Nix build (`nix build .#libkrunfw-nested`) is not run as part of `just test` or `just integration`. The kernel config additions (`CONFIG_KVM=y`, `CONFIG_KVM_INTEL=y`, `CONFIG_KVM_AMD=y`) are applied via Nix's `overrideAttrs` mechanism, and verifying that the resulting kernel binary actually includes KVM support would require inspecting the kernel binary or booting a VM with it (which is covered by AC4.1).
- **Verification approach:**
  1. Run `nix build .#libkrunfw-nested` and confirm it succeeds
  2. After entering the Nix dev shell, confirm `test-prefix/lib64/libkrunfw-nested.so` exists
  3. The integration test (AC4.1) provides indirect verification: if the L1 guest can access `/dev/kvm`, the kernel was built with KVM support

### nested-virt.AC4.4: Test skips cleanly on hosts without nested virt support

- **Why it cannot be fully automated in CI:** If CI hosts support nested virtualization, the skip path is never exercised. Conversely, if CI hosts do not support nested virt, the main test path (AC4.1-AC4.3) is never exercised. Full coverage of both paths requires running on both types of hosts.
- **Verification approach:**
  1. On a host with nested virt enabled: run `just integration nested-virt` and confirm the full L0->L1->L2 chain passes
  2. On a host without nested virt (or with `nested=0`): run `just integration nested-virt` and confirm output contains "SKIP: host does not support nested virtualization" with a passing exit code
  3. Code review of `host_supports_nested_virt()` to verify it checks both `kvm_intel` and `kvm_amd` parameter files and accepts both "1" and "Y" values
