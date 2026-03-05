# Rust Init Migration - Phase 3: Guest Stdio Redirect from Env Vars

**Goal:** Replace the sysfs-scanning `setup_redirects()` in the Rust init with a simple env-var-based approach that reads `KRUN_STDIN_DEV`, `KRUN_STDOUT_DEV`, `KRUN_STDERR_DEV` directly.

**Architecture:** The host (Phase 2) injects `KRUN_STDIN_DEV=/dev/vport3p1` etc. into the kernel cmdline. The Rust init reads these from its process environment and opens the device nodes directly with `open()` + `dup2()`. No sysfs interaction. If a var is absent, the corresponding fd is not redirected.

**Tech Stack:** Rust, libc crate

**Scope:** 4 phases from original design (phase 3 of 4)

**Codebase verified:** 2026-03-04

---

## Acceptance Criteria Coverage

This phase implements and tests:

### rust-init.AC3: Guest stdio redirect from env vars
- **rust-init.AC3.1 Success:** Guest stdin reads from the device specified by `KRUN_STDIN_DEV`
- **rust-init.AC3.2 Success:** Guest stdout writes to the device specified by `KRUN_STDOUT_DEV`
- **rust-init.AC3.3 Success:** Guest stderr writes to the device specified by `KRUN_STDERR_DEV`
- **rust-init.AC3.4 Edge:** When `KRUN_STDIN_DEV` is absent, stdin is not redirected (uses kernel default)
- **rust-init.AC3.5 Failure:** When `KRUN_STDIN_DEV` contains a non-existent path, init prints error but continues (doesn't abort boot)

---

<!-- START_TASK_1 -->
### Task 1: Replace setup_redirects() with env-var-based redirect

**Verifies:** rust-init.AC3.1, rust-init.AC3.2, rust-init.AC3.3, rust-init.AC3.4, rust-init.AC3.5

**Files:**
- Modify: `init/src/main.rs` (replace `setup_redirects()` function)

**Implementation:**

Replace the entire `setup_redirects()` function (which scans `/sys/class/virtio-ports/`) with a simple function that reads three env vars and calls `reopen_fd()` for each.

The existing `reopen_fd()` helper (already written in Phase 1) handles errors gracefully — prints an error message but does not abort. This satisfies AC3.5.

New `setup_redirects()`:

```rust
fn setup_redirects() {
    if let Ok(path) = env::var("KRUN_STDIN_DEV") {
        reopen_fd(libc::STDIN_FILENO, &path, libc::O_RDONLY);
    }
    if let Ok(path) = env::var("KRUN_STDOUT_DEV") {
        reopen_fd(libc::STDOUT_FILENO, &path, libc::O_WRONLY);
    }
    if let Ok(path) = env::var("KRUN_STDERR_DEV") {
        reopen_fd(libc::STDERR_FILENO, &path, libc::O_WRONLY);
    }
}
```

This replaces the ~50-line sysfs scanning implementation from Phase 1. The `reopen_fd()` function remains unchanged — it opens the path, dup2's to the target fd, closes the original if different.

**Key behaviors:**
- When env var is absent (`Err`), the corresponding fd is not redirected (AC3.4)
- When env var contains a non-existent path, `reopen_fd()` prints an error but returns without aborting (AC3.5)
- When env var is present with a valid path, the fd is redirected to that device (AC3.1, AC3.2, AC3.3)

**Step 1: Replace `setup_redirects()` in `init/src/main.rs` with the code above**

Remove the entire old `setup_redirects()` function (the sysfs scanning version) and replace it with the 9-line env-var version.

**Step 2: Verify it compiles**

```bash
cd init && cargo build --release
```

Expected: Compiles with no errors.

**Step 3: Rebuild and run integration tests**

```bash
just build-init
just integration
```

Expected: All integration tests pass. The host now injects `KRUN_STDIN_DEV`/`KRUN_STDOUT_DEV`/`KRUN_STDERR_DEV` (Phase 2), and the init reads them directly.

**Commit:** `feat(init): replace sysfs port scan with env-var-based stdio redirect`
<!-- END_TASK_1 -->
