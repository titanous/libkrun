# Rust Init Migration - Phase 2: Host-Side Port Path Injection

**Goal:** Host computes virtio-console device paths and injects them into the kernel cmdline as `KRUN_STDIN_DEV`, `KRUN_STDOUT_DEV`, `KRUN_STDERR_DEV`.

**Architecture:** After console port registration in `build_microvm()`, iterate `device_info.console_ports` to find named ports (`krun-stdin`, `krun-stdout`, `krun-stderr`). For each, insert the corresponding `KRUN_*_DEV=/dev/vportXpY` env var into `vmm.kernel_cmdline`. This happens before `load_cmdline()` writes the cmdline to guest memory.

The design plan suggests adding fields to `ContextConfig` and routing paths back through it. However, investigation shows the cmdline and console ports are both assembled inside `build_microvm()` in `builder.rs` — the simplest correct approach is to inject directly there, after console device attachment (line ~1431) and before `load_cmdline()` (line ~1531).

**Tech Stack:** Rust

**Scope:** 4 phases from original design (phase 2 of 4)

**Codebase verified:** 2026-03-04

---

## Acceptance Criteria Coverage

This phase implements and tests:

### rust-init.AC2: Host injects port device paths into cmdline
- **rust-init.AC2.1 Success:** When non-terminal stdin port is configured, `KRUN_STDIN_DEV=/dev/vportXpY` appears on guest kernel cmdline
- **rust-init.AC2.2 Success:** Same for `KRUN_STDOUT_DEV` and `KRUN_STDERR_DEV`
- **rust-init.AC2.3 Edge:** When port is a terminal (not a pipe), the corresponding `KRUN_*_DEV` env var is absent from cmdline
- **rust-init.AC2.4 Edge:** When no console ports are configured, all three env vars are absent

---

<!-- START_TASK_1 -->
### Task 1: Inject KRUN_*_DEV env vars into kernel cmdline after console port registration

**Verifies:** rust-init.AC2.1, rust-init.AC2.2, rust-init.AC2.3, rust-init.AC2.4

**Files:**
- Modify: `src/vmm/src/builder.rs:1431` (after console device attachment loop, before `load_cmdline`)

**Implementation:**

In `build_microvm()`, after the console device attachment loop (which ends at line ~1431 with `console_id += 1;`) and before the `timer.checkpoint("attach_fs + ...")` call, insert code that scans `device_info.console_ports` for named ports and adds the corresponding env vars to `vmm.kernel_cmdline`.

The insertion point is after line 1431 (`console_id += 1;` closing brace) and before line 1433 (`timer.checkpoint`).

Insert this block:

```rust
    // Inject KRUN_*_DEV env vars for named console ports so the guest init
    // can open them directly without scanning sysfs.
    for port in &device_info.console_ports {
        match port.name.as_deref() {
            Some("krun-stdin") => {
                vmm.kernel_cmdline
                    .insert_str(&format!("KRUN_STDIN_DEV={}", port.device_path))
                    .unwrap();
            }
            Some("krun-stdout") => {
                vmm.kernel_cmdline
                    .insert_str(&format!("KRUN_STDOUT_DEV={}", port.device_path))
                    .unwrap();
            }
            Some("krun-stderr") => {
                vmm.kernel_cmdline
                    .insert_str(&format!("KRUN_STDERR_DEV={}", port.device_path))
                    .unwrap();
            }
            _ => {}
        }
    }
```

**Why this works:**
- Console ports are registered during `attach_console_devices()` (line ~1408-1431), which populates `device_info.console_ports` with `ConsolePortInfo` structs including `name` and `device_path` fields
- Named ports (`krun-stdin`, `krun-stdout`, `krun-stderr`) are only created when the corresponding fd is NOT a terminal (see `autoconfigure_console_ports()` at line ~2753-2771 in builder.rs)
- When all fds are terminals, no named ports are created, so no `KRUN_*_DEV` vars are injected (satisfies AC2.3 and AC2.4)
- `vmm.kernel_cmdline` is an `arch::kernel::cmdline::Cmdline` — `insert_str()` appends space-separated tokens
- `load_cmdline()` (line ~1531) writes the final cmdline to guest memory after this injection

**Verification:**

```bash
just check
just test
```

Expected: No compilation errors, clippy passes, unit tests pass.

To manually verify the env vars appear on the cmdline, run an integration test that reads the guest's `/proc/cmdline`:

```bash
just integration configure-vm-1cpu-256MiB
```

If deeper verification is needed, temporarily add a debug print in `build_microvm()` after the injection to log the cmdline contents.

**Commit:** `feat(vmm): inject KRUN_*_DEV port paths into guest kernel cmdline`
<!-- END_TASK_1 -->
