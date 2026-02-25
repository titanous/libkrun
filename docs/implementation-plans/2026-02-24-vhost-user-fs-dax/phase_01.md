# VhostUserFs with DAX Implementation Plan - Phase 1

**Goal:** Establish vhost-user infrastructure by integrating PR #527's generic VhostUserDevice wrapper and memfd-backed guest memory, stripped of C API, RNG device logic, and example changes.

**Architecture:** PR #527 introduces: (1) `VhostUserDevice` — a generic vhost-user frontend that handles socket connection, feature negotiation, memory sharing, vring setup, and interrupt forwarding; (2) memfd-backed guest RAM enabling fd-passing to vhost-user daemons via the vhost-user memory table. This phase cherry-picks the infrastructure commits while stripping the RNG device specialization and C API surface, retaining only the generic foundation.

**Tech Stack:** Rust, vhost crate v0.15 (vhost-user-frontend feature)

**Scope:** 8 phases from original design (phase 1 of 8)

**Codebase verified:** 2026-02-24

---

## Acceptance Criteria Coverage

This phase verifies operationally:

### vhost-user-fs-dax.AC1: VhostUserDevice generic wrapper works
- **vhost-user-fs-dax.AC1.4 Success:** Existing tests pass without regression after PR #527 integration

---

<!-- START_TASK_1 -->
### Task 1: Fetch and cherry-pick PR #527 infrastructure commits

**Files:**
- New: `src/devices/src/virtio/vhost_user/device.rs` (VhostUserDevice, ~450 lines)
- New: `src/devices/src/virtio/vhost_user/mod.rs` (module re-export)
- Modify: `src/devices/Cargo.toml` (vhost-user feature + vhost dep)
- Modify: `src/devices/src/virtio/mod.rs` (vhost_user module registration)
- Modify: `src/vmm/Cargo.toml` (vhost-user feature)
- Modify: `src/libkrun/Cargo.toml` (vhost-user feature)
- Modify: `src/vmm/src/builder.rs` (memfd-backed memory)
- Modify: `src/vmm/src/resources.rs` (VhostUserDeviceConfig)
- Modify: `src/vmm/src/device_manager/kvm/mmio.rs` (error variant)
- Modify: `src/libkrun/src/lib.rs` (C API — will be stripped in Task 2)
- Modify: `include/libkrun.h` (C API — will be stripped in Task 2)
- Modify: `Makefile` (VHOST_USER flag)
- Modify: `Cargo.lock` (vhost dep)

**Step 1: Fetch PR #527 branch from upstream**

The `upstream` remote (containers/libkrun) is already configured. Fetch the PR head:

```bash
git fetch upstream pull/527/head:pr-527
```

**Step 2: Cherry-pick the three infrastructure commits**

PR #527 has 5 commits. Cherry-pick only the first 3 (infrastructure, memfd, VhostUserDevice). Skip commits 4 and 5 (RNG device implementation, example changes):

```bash
# Commit 1: Feature flags, Cargo.toml changes, C API, module setup
git cherry-pick 1900376 --no-commit

# Commit 2: Memfd-backed guest memory in builder.rs
git cherry-pick 635abe7 --no-commit

# Commit 3: Core VhostUserDevice implementation
git cherry-pick e5510dc --no-commit
```

Use `--no-commit` to stage all changes without committing, allowing stripping before the first commit.

**If cherry-pick conflicts arise:** Resolve each conflict, favoring our branch's existing code structure while adding the new PR content. Key conflict areas:
- `src/libkrun/src/lib.rs` — our branch has additional Builder methods; add PR functions alongside existing ones
- `src/vmm/src/builder.rs` — our branch has snapshot/restore changes; add memfd paths alongside, preserving existing `create_guest_memory` and `load_payload` signatures
- `src/vmm/src/resources.rs` — our branch has additional config types; add `VhostUserDeviceConfig` and fields
- `Cargo.lock` — accept both sides, then run `cargo update -p vhost` to resolve

**Fallback if cherry-pick fails entirely:**

Download the combined diff and apply manually:
```bash
curl -L https://github.com/containers/libkrun/pull/527.diff -o /tmp/527.diff
git apply --reject /tmp/527.diff
# Manually resolve any .rej files
```

**Step 3: Verify key files exist**

```bash
ls src/devices/src/virtio/vhost_user/device.rs
ls src/devices/src/virtio/vhost_user/mod.rs
grep 'vhost-user' src/devices/Cargo.toml
grep 'vhost-user' src/vmm/Cargo.toml
grep 'vhost-user' src/libkrun/Cargo.toml
```

Expected: All files exist, all three Cargo.toml files have `vhost-user` feature.
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Strip C API, RNG logic, header and example additions

This task removes everything from PR #527 that is RNG-specific or C-API-specific, retaining only the generic vhost-user infrastructure.

**Files:**
- Modify: `src/libkrun/src/lib.rs` (remove 2 C API functions)
- Revert: `include/libkrun.h` (remove all PR additions)
- Modify: `src/vmm/src/builder.rs` (remove RNG conditionals and attach function)
- Modify: `src/vmm/src/resources.rs` (remove disable_implicit_rng)

**Step 1: Remove C API functions from lib.rs**

Remove these two functions entirely (both the `#[cfg(feature = "vhost-user")]` implementation AND the `#[cfg(not(feature = "vhost-user"))]` stub):

1. `krun_add_vhost_user_device()` — search for `fn krun_add_vhost_user_device`, remove the entire function including doc comments, `#[no_mangle]`, both cfg variants
2. `krun_disable_implicit_rng()` — search for `fn krun_disable_implicit_rng`, remove the entire function including doc comments

**Step 2: Revert include/libkrun.h**

Remove all PR #527 additions from the header file:
- `KRUN_VIRTIO_DEVICE_RNG`, `KRUN_VIRTIO_DEVICE_SND`, `KRUN_VIRTIO_DEVICE_CAN` constants
- `KRUN_VHOST_USER_RNG_NUM_QUEUES`, `KRUN_VHOST_USER_RNG_QUEUE_SIZES` macros
- `krun_add_vhost_user_device()` declaration and all its documentation
- `krun_disable_implicit_rng()` declaration and all its documentation

Simplest approach: `git checkout HEAD -- include/libkrun.h` to revert to pre-cherry-pick state.

**Step 3: Remove RNG suppression logic from builder.rs**

In `build_microvm()`, find the vhost-user conditional that iterates devices and suppresses RNG:

```rust
// REMOVE this entire block:
#[cfg(feature = "vhost-user")]
{
    for device_config in &vm_resources.vhost_user_devices {
        attach_vhost_user_device(&mut vmm, intc.clone(), device_config)?;
    }
}

let has_vhost_user_rng = vm_resources.vhost_user_devices
    .iter()
    .any(|dev| dev.device_type == VIRTIO_ID_RNG);

if !vm_resources.disable_implicit_rng && !has_vhost_user_rng {
    attach_rng_device(&mut vmm, event_manager, intc.clone())?;
}
```

Replace with the original unconditional RNG attachment:
```rust
attach_rng_device(&mut vmm, event_manager, intc.clone())?;
```

**Step 4: Remove attach_vhost_user_device() function from builder.rs**

Remove the entire `attach_vhost_user_device()` function (including its `#[cfg]` attributes). This function is RNG/generic-device specific and becomes dead code after Step 3. Phase 4 will add a new `attach_vhost_user_fs_device()` for the FS device.

**Step 5: Remove disable_implicit_rng from resources.rs**

Remove the `disable_implicit_rng: bool` field from `VmResources` struct and its initialization in `Default` impl. Keep `VhostUserDeviceConfig` struct and `vhost_user_devices` field.

**Step 6: Remove examples/chroot_vm.c changes**

If commit 5 was not cherry-picked (it shouldn't have been), no changes to revert. Verify:
```bash
git diff HEAD -- examples/chroot_vm.c
```
Expected: No changes. If changes exist, revert with `git checkout HEAD -- examples/chroot_vm.c`.

**Step 7: Clean up unused imports**

After the removals, check for and remove any unused imports in builder.rs and lib.rs that were only needed by the removed code (e.g., `VhostUserDeviceConfig` import in builder.rs, `VIRTIO_ID_RNG` constant).

**Verification:**

```bash
# Ensure no RNG/C-API references remain
grep -rn 'krun_add_vhost_user_device\|krun_disable_implicit_rng\|disable_implicit_rng\|attach_vhost_user_device\|has_vhost_user_rng' \
  src/libkrun/src/lib.rs src/vmm/src/builder.rs src/vmm/src/resources.rs include/libkrun.h
```

Expected: No matches.

**Commit:**
```bash
git add -A
git commit -m "feat: add vhost-user infrastructure from PR #527, stripped of C API and RNG"
```
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Verify build and existing tests

**Step 1: Build with vhost-user feature**

```bash
cargo build --features vhost-user
```

Expected: Builds without errors. The `vhost` crate v0.15 should resolve and compile. `VhostUserDevice` and memfd paths compile but are not yet exercised (no devices configured).

**Step 2: Build without vhost-user feature**

```bash
cargo build
```

Expected: Builds without errors. All vhost-user code is conditionally compiled out.

**Step 3: Run existing unit tests**

```bash
cargo test -p devices --features net,snapshot
cargo test -p vmm --features snapshot
```

Expected: All existing tests pass. The memfd path is only activated when `vhost_user_devices` is non-empty, so existing tests use the standard mmap path.

**Step 4: Fix any compilation or test issues**

If there are unused code warnings or test failures from the integration, fix them. Common issues:
- Unused import warnings: remove the imports
- Feature flag gating: ensure all new code is behind `#[cfg(feature = "vhost-user")]`
- Type mismatches from merge conflicts: align types with our branch's conventions

**Commit (if fixups needed):**
```bash
git add -A
git commit -m "fix: resolve compilation issues from vhost-user integration"
```
<!-- END_TASK_3 -->
