# Test Requirements: Rust Init Migration

Generated: 2026-03-04
Source: docs/design-plans/2026-03-04-rust-init.md

## Methodology

The init binary runs as PID 1 inside a microVM. It cannot be unit tested in isolation
because its operations (mount, fork, dup2, setsid, ioctl, chroot) require a full
kernel environment. All behavioral verification must happen via integration tests that
boot a VM and observe outcomes from both host and guest sides.

The project has 49 existing integration tests that exercise the boot path end-to-end.
Every test implicitly validates that the init binary:
- Mounts filesystems successfully
- Redirects stdio to virtio-console ports
- Reads KRUN_INIT/KRUN_WORKDIR from env
- Forks, execs the guest-agent, waits, reports exit code
- Drains output before exit

New tests should only be written where existing coverage has gaps.

Test file location convention: tests/test_cases/src/test_<name>.rs
Run: `just integration` (all) or `just integration <name>` (single)

---

## rust-init.AC1: Rust init binary builds and boots

### rust-init.AC1.1 — `just build-init` produces a static musl binary at init/init

| Field | Value |
|-------|-------|
| Test type | Automated (build verification) |
| Method | `just build-init` succeeds; `file init/init` contains "statically linked" |
| Where | CI pipeline / `just all` (build-init is a dependency of build) |
| New test needed | No — build failure breaks all downstream targets |

### rust-init.AC1.2 — Binary is statically linked with no dynamic library dependencies

| Field | Value |
|-------|-------|
| Test type | Automated (build verification) |
| Method | `file init/init` output contains "statically linked"; `ldd init/init` reports "not a dynamic executable" |
| Where | CI pipeline (one-time verification; musl target guarantees this) |
| New test needed | No — inherent property of x86_64-unknown-linux-musl target with static libc |

### rust-init.AC1.3 — VM boots with Rust init and successfully execs a workload

| Field | Value |
|-------|-------|
| Test type | Automated (integration / e2e) |
| Covered by | All 49 existing integration tests. Every test boots a VM, which means init ran, mounted filesystems, read KRUN_INIT, forked, and exec'd the guest-agent. |
| Key tests | `configure-vm-1cpu-256MiB`, `vm-exit-clean-shutdown`, `boot-timing-e2e` |
| File | `tests/test_cases/src/test_vm_config.rs`, `tests/test_cases/src/test_vm_exit.rs`, `tests/test_cases/src/test_boot_timing_e2e.rs` |
| New test needed | No |

### rust-init.AC1.4 — Init prints error and exits non-zero if mount_filesystems() fails

| Field | Value |
|-------|-------|
| Test type | Human verification |
| Justification | Triggering a mount failure inside a VM requires a corrupted or missing devtmpfs, which is a kernel-level condition that cannot be reliably manufactured from host configuration. The mount syscall is unconditional at PID 1 startup before any guest-agent code runs. |
| Verification approach | Code review: confirm `mount_filesystems()` returns `Err` on failure and `main()` calls `exit(-2)` on that error path. The pattern is: `if let Err(e) = mount_filesystems() { eprintln!(...); exit(-2); }` |

---

## rust-init.AC2: Host injects port device paths into cmdline

### rust-init.AC2.1 — KRUN_STDIN_DEV=/dev/vportXpY appears on guest kernel cmdline

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Covered by | All tests using piped (non-terminal) stdio implicitly depend on this for stdio to work after Phase 3. Before Phase 3, the sysfs fallback masks this. After Phase 3 completion, every test that reads stdout from the guest validates that KRUN_STDOUT_DEV was injected and consumed. |
| Explicit verification | New integration test recommended |
| Test name | `init-cmdline-port-vars` |
| File | `tests/test_cases/src/test_init_cmdline.rs` |
| Description | Guest reads `/proc/cmdline`, asserts `KRUN_STDIN_DEV=`, `KRUN_STDOUT_DEV=`, `KRUN_STDERR_DEV=` are present and match `/dev/vport\d+p\d+` pattern. Reports result over vsock. |

### rust-init.AC2.2 — Same for KRUN_STDOUT_DEV and KRUN_STDERR_DEV

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Covered by | Same test as AC2.1 — single test checks all three vars |

### rust-init.AC2.3 — When port is a terminal, KRUN_*_DEV env var is absent

| Field | Value |
|-------|-------|
| Test type | Human verification |
| Justification | In CI, fds are always pipes (not terminals), so the terminal path cannot be triggered without mocking isatty. The logic is in `builder.rs` `autoconfigure_console_ports()` which gates named port creation on `!isatty(fd)`. |
| Verification approach | Code review: named ports (`krun-stdin`, `krun-stdout`, `krun-stderr`) are only created when the fd is NOT a terminal (`if input_fd >= 0 && !input_is_terminal`). When all fds are terminals, no named ports are created, so the injection loop in `build_microvm()` finds no matching ports. |

### rust-init.AC2.4 — When no console ports are configured, all three env vars are absent

| Field | Value |
|-------|-------|
| Test type | Human verification |
| Justification | A VM with truly zero ports has no stdout path for the guest to report results, making assertion difficult. |
| Verification approach | Code review: the injection loop iterates `device_info.console_ports`. When no ports are registered, the vec is empty, so no `KRUN_*_DEV` vars are inserted. The match-on-name logic only fires for `krun-stdin`/`krun-stdout`/`krun-stderr` — custom port names are ignored. |

---

## rust-init.AC3: Guest stdio redirect from env vars

### rust-init.AC3.1 — Guest stdin reads from the device specified by KRUN_STDIN_DEV

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Covered by | Existing tests that send data to the guest via piped stdin. After Phase 3, stdin redirect uses KRUN_STDIN_DEV env var instead of sysfs scan. If redirect fails, guest-agent would not receive input. |
| Key tests | Any test using piped console stdin. The basic boot path validates stdout redirect (guest prints "OK"). |
| New test needed | No — existing `multiport-console` test validates port-based I/O. |

### rust-init.AC3.2 — Guest stdout writes to the device specified by KRUN_STDOUT_DEV

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Covered by | Every integration test that checks for "OK" in guest stdout. The test harness captures stdout from the VM and asserts content. After Phase 3, stdout redirect uses KRUN_STDOUT_DEV. |
| Key tests | All 49 tests — each guest prints "OK" to stdout which the host captures |
| New test needed | No |

### rust-init.AC3.3 — Guest stderr writes to the device specified by KRUN_STDERR_DEV

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Covered by | Implicitly tested — stderr redirect uses the same `reopen_fd()` code path as stdout. Any init error messages during boot would go to stderr. |
| New test needed | No — same mechanism as stdout. |

### rust-init.AC3.4 — When KRUN_STDIN_DEV is absent, stdin is not redirected

| Field | Value |
|-------|-------|
| Test type | Human verification |
| Justification | This edge case occurs when all stdio fds are terminals (AC2.3 condition). In CI, fds are always pipes, so the env vars are always present. Cannot trigger absence without mocking isatty. |
| Verification approach | Code review: `setup_redirects()` uses `if let Ok(path) = env::var("KRUN_STDIN_DEV")` — when absent, the branch is skipped. No redirect occurs, fd 0 retains its kernel-assigned default (the hvc0 console device). |

### rust-init.AC3.5 — Non-existent path in KRUN_STDIN_DEV prints error but continues

| Field | Value |
|-------|-------|
| Test type | Human verification |
| Justification | Injecting a bad path into KRUN_STDIN_DEV requires modifying the host builder to produce an invalid device path, which is not a realistic production scenario. |
| Verification approach | Code review: `reopen_fd()` checks `open()` return value. On failure (fd < 0), it prints an error message with errno and returns without calling `dup2` or `exit`. The calling function `setup_redirects()` continues to the next env var. Boot proceeds. |

---

## rust-init.AC4: Env var config works correctly

### rust-init.AC4.1 — KRUN_INIT sets the exec path for the workload

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Covered by | Every integration test. `setup_fs_builder()` in `krun_rust.rs` calls `builder.exec_path("/guest-agent")` which sets `KRUN_INIT=/guest-agent` on the kernel cmdline. |
| New test needed | No |

### rust-init.AC4.2 — KRUN_WORKDIR sets the working directory before exec

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Covered by | Every integration test sets `builder.workdir("/")`. |
| Explicit verification | New integration test recommended |
| Test name | `init-workdir` |
| File | `tests/test_cases/src/test_init_env.rs` |
| Description | Host sets `builder.workdir("/tmp")`. Guest calls `std::env::current_dir()` and asserts it equals "/tmp". Reports result over vsock. |

### rust-init.AC4.3 — KRUN_RLIMITS sets resource limits

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Test name | `init-rlimits` |
| File | `tests/test_cases/src/test_init_env.rs` |
| Description | Host sets rlimits via builder API (e.g., RLIMIT_NOFILE 1024,4096). Guest reads limits with `getrlimit()` and asserts cur=1024, max=4096. Reports over vsock. |
| New test needed | Yes — no existing test exercises KRUN_RLIMITS |

### rust-init.AC4.4 — HOSTNAME sets the guest hostname

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Test name | `init-hostname` |
| File | `tests/test_cases/src/test_init_env.rs` |
| Description | Host sets hostname via builder API. Guest reads hostname via `gethostname()` and asserts match. Reports over vsock. |
| New test needed | Yes — no existing test exercises HOSTNAME |

### rust-init.AC4.5 — KRUN_HOME / KRUN_TERM set HOME / TERM env vars

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Test name | `init-home-term` |
| File | `tests/test_cases/src/test_init_env.rs` |
| Description | Host sets HOME and TERM via builder API. Guest reads env::var("HOME") and env::var("TERM"), asserts match. Reports over vsock. |
| New test needed | Yes — no existing test exercises KRUN_HOME/KRUN_TERM |

### rust-init.AC4.6 — Missing optional env vars handled gracefully

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Covered by | Many existing tests do not set KRUN_RLIMITS, HOSTNAME, KRUN_HOME, or KRUN_TERM. The fact that these tests boot and run successfully demonstrates graceful handling of missing optional vars. |
| Key tests | `configure-vm-1cpu-256MiB` (minimal config, no optional vars) |
| New test needed | No |

---

## rust-init.AC5: Exit code and lifecycle contracts preserved

### rust-init.AC5.1 — Workload exit code reported to host via virtiofs ioctl

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Covered by | `vm-exit-clean-shutdown` asserts `VmExit::Shutdown { exit_code: 0 }`. |
| Additional coverage | New test for non-zero exit code recommended |
| Test name | `init-exit-code-nonzero` |
| File | `tests/test_cases/src/test_init_lifecycle.rs` |
| Description | Guest exits with code 42. Host asserts `VmExit::Shutdown { exit_code: 42 }`. |
| New test needed | Yes — existing tests only verify exit code 0 |

### rust-init.AC5.2 — When KRUN_INIT_PID1=1, init execs workload directly (no fork)

| Field | Value |
|-------|-------|
| Test type | Human verification |
| Justification | The builder API does not currently expose a method to set KRUN_INIT_PID1. The behavioral difference (workload is PID 1 vs PID 2+) is not observable from outside the VM via existing test infrastructure. |
| Verification approach | Code review: when `KRUN_INIT_PID1` starts with '1', the code calls `execvp()` directly without `fork()`. The `setup_redirects()` call precedes execvp. On exec failure, exit codes 126/127 are used (matching bash convention). |

### rust-init.AC5.3 — When KRUN_INIT_PID1 is unset, init forks child, waits, reports exit code

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Covered by | All existing tests use the default mode (KRUN_INIT_PID1 unset), which means fork+exec. Every test passing confirms this path works. |
| Key tests | `vm-exit-clean-shutdown`, `vm-exit-observer` |
| New test needed | No |

### rust-init.AC5.4 — Signal-killed workloads report exit code = signal + 128

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Test name | `init-exit-signal` |
| File | `tests/test_cases/src/test_init_lifecycle.rs` |
| Description | Guest sends SIGKILL to itself. Host asserts `VmExit::Shutdown { exit_code: 137 }` (9 + 128 = 137). |
| New test needed | Yes — no existing test verifies signal-based exit codes |

### rust-init.AC5.5 — stdout/stderr are drained (tcdrain) before init exits

| Field | Value |
|-------|-------|
| Test type | Human verification |
| Justification | `tcdrain` ensures buffered output reaches the host before the VM shuts down. This is observable only by its absence (missing trailing output). All existing tests already verify complete output, which implicitly confirms output is not truncated. |
| Verification approach | Code review: after the waitpid loop and exit code reporting, the init calls `tcdrain(STDOUT_FILENO)` and `tcdrain(STDERR_FILENO)` before falling off main. Combined with: all 49 existing tests printing "OK" and the host successfully reading it confirms output reaches the host. |

### rust-init.AC5.6 — Block root device pivot works with chroot + re-mount

| Field | Value |
|-------|-------|
| Test type | Automated (integration) |
| Test name | `init-block-root-pivot` |
| File | `tests/test_cases/src/test_init_lifecycle.rs` |
| Description | Host configures a block device with a filesystem image and sets KRUN_BLOCK_ROOT_DEVICE. Guest verifies root is the block device filesystem, confirms /proc and /dev are re-mounted. |
| New test needed | Yes — but high complexity (requires filesystem image creation). Acceptable to defer to a separate PR. |
| Alternative | Human verification via code review. The C init's block root pivot was not integration-tested either. |

---

## rust-init.AC6: C init removed, build system updated

### rust-init.AC6.1 — init/init.c and init/jsmn.h are deleted

| Field | Value |
|-------|-------|
| Test type | Automated (build verification) |
| Method | `ls init/init.c init/jsmn.h` returns "No such file or directory" |
| New test needed | No — file absence is trivially verifiable |

### rust-init.AC6.2 — `just all` passes with Rust init

| Field | Value |
|-------|-------|
| Test type | Automated (CI suite) |
| Method | `just all` (check + test + miri + proptest + loom + shuttle) |
| New test needed | No — runs existing test infrastructure |

### rust-init.AC6.3 — `just integration` passes with Rust init

| Field | Value |
|-------|-------|
| Test type | Automated (integration suite) |
| Method | `just integration` (all 49 tests) |
| New test needed | No — runs existing test infrastructure |

---

## Summary: New Tests Needed

| Test name | File | Priority | Covers |
|-----------|------|----------|--------|
| `init-cmdline-port-vars` | `tests/test_cases/src/test_init_cmdline.rs` | High | AC2.1, AC2.2 |
| `init-exit-code-nonzero` | `tests/test_cases/src/test_init_lifecycle.rs` | High | AC5.1 (non-zero) |
| `init-exit-signal` | `tests/test_cases/src/test_init_lifecycle.rs` | High | AC5.4 |
| `init-rlimits` | `tests/test_cases/src/test_init_env.rs` | Medium | AC4.3 |
| `init-hostname` | `tests/test_cases/src/test_init_env.rs` | Medium | AC4.4 |
| `init-workdir` | `tests/test_cases/src/test_init_env.rs` | Medium | AC4.2 (explicit) |
| `init-home-term` | `tests/test_cases/src/test_init_env.rs` | Medium | AC4.5 |
| `init-block-root-pivot` | `tests/test_cases/src/test_init_lifecycle.rs` | Low (defer) | AC5.6 |

Total: 8 new tests (3 high priority, 4 medium, 1 deferrable)

## Summary: Human Verification Only

| Criterion | Justification |
|-----------|---------------|
| AC1.4 (mount failure) | Cannot trigger kernel mount failure from host config |
| AC2.3 (terminal fd = no var) | CI fds are always pipes; cannot trigger isatty=true |
| AC2.4 (no console ports) | No stdout path to report results with zero ports |
| AC3.4 (absent env var = no redirect) | Same as AC2.3; env var absence follows from terminal detection |
| AC3.5 (bad path = error + continue) | Requires injecting invalid device path into builder internals |
| AC5.2 (PID1 direct exec) | No builder API to set KRUN_INIT_PID1; behavioral difference not observable |
| AC5.5 (tcdrain before exit) | Implicitly validated by all tests receiving complete output; explicit test racy |

All human-verification items have been confirmed reviewable through code inspection of
`init/src/main.rs`. The error handling patterns are straightforward: check return value,
print error, continue or exit with distinctive code.

## Existing Test Coverage (Implicit Init Validation)

The 49 existing integration tests collectively validate the following init behaviors
without any changes needed:

- Filesystem mounting (every test boots successfully)
- KRUN_INIT env var parsing (every test sets exec_path)
- KRUN_WORKDIR env var parsing (every test sets workdir)
- Fork+exec lifecycle (every test runs guest-agent as child)
- Exit code 0 reporting via virtiofs ioctl (vm-exit-clean-shutdown)
- Stdio redirect to virtio-console ports (every test reads guest stdout)
- Loopback interface (net tests depend on it)
- Missing optional env vars (most tests omit KRUN_RLIMITS, HOSTNAME, etc.)
- Shared mount propagation (virtiofs mounts in guest work)
- setsid + TIOCSCTTY (console I/O works)
