# Rust Init Migration - Phase 1: Rust Init Binary Scaffolding

**Goal:** Establish the Rust init crate, build system integration, and a complete binary that boots successfully — mounts filesystems, configures environment, forks and execs the workload, reports exit code.

**Architecture:** Single Rust binary compiled to `x86_64-unknown-linux-musl` in a separate Cargo workspace at `init/`. The binary at `init/init` is picked up by `include_bytes!` in `src/devices/src/virtio/fs/linux/passthrough.rs:38`. All guest config arrives via kernel cmdline env vars (`KRUN_*`). Sequential execution — no threads.

**Tech Stack:** Rust, libc crate, musl static linking

**Scope:** 4 phases from original design (phase 1 of 4)

**Codebase verified:** 2026-03-04

---

## Acceptance Criteria Coverage

This phase implements and tests:

### rust-init.AC1: Rust init binary builds and boots
- **rust-init.AC1.1 Success:** `just build-init` produces a static musl binary at `init/init`
- **rust-init.AC1.2 Success:** Binary is statically linked with no dynamic library dependencies
- **rust-init.AC1.3 Success:** VM boots with Rust init and successfully execs a workload (e.g. `/bin/sh`)
- **rust-init.AC1.4 Failure:** Init prints error and exits non-zero if mount_filesystems() fails

### rust-init.AC4: Env var config works correctly
- **rust-init.AC4.1 Success:** `KRUN_INIT` sets the exec path for the workload
- **rust-init.AC4.2 Success:** `KRUN_WORKDIR` sets the working directory before exec
- **rust-init.AC4.3 Success:** `KRUN_RLIMITS` sets resource limits (id,cur,max triples)
- **rust-init.AC4.4 Success:** `HOSTNAME` sets the guest hostname
- **rust-init.AC4.5 Success:** `KRUN_HOME` / `KRUN_TERM` set HOME / TERM env vars
- **rust-init.AC4.6 Edge:** Missing optional env vars (KRUN_WORKDIR, KRUN_RLIMITS, etc.) are handled gracefully

### rust-init.AC5: Exit code and lifecycle contracts preserved
- **rust-init.AC5.1 Success:** Workload exit code is reported to host via virtiofs KRUN_EXIT_CODE_IOCTL
- **rust-init.AC5.2 Success:** When KRUN_INIT_PID1=1, init execs workload directly (no fork)
- **rust-init.AC5.3 Success:** When KRUN_INIT_PID1 is unset, init forks child, waits for it, reports exit code
- **rust-init.AC5.4 Success:** Signal-killed workloads report exit code = signal + 128
- **rust-init.AC5.5 Success:** stdout/stderr are drained (tcdrain) before init exits
- **rust-init.AC5.6 Success:** Block root device pivot (KRUN_BLOCK_ROOT_DEVICE) works with chroot + re-mount

---

<!-- START_TASK_1 -->
### Task 1: Create init crate scaffolding (Cargo.toml, .cargo/config.toml)

**Files:**
- Create: `init/Cargo.toml`
- Create: `init/.cargo/config.toml`
- Create: `init/src/main.rs` (minimal placeholder)

**Step 1: Create `init/Cargo.toml`**

```toml
[package]
name = "krun-init"
version = "0.1.0"
edition = "2021"

[workspace]

[dependencies]
libc = "0.2"

[profile.release]
opt-level = "z"
lto = true
strip = true
panic = "abort"
```

This is a separate workspace (not part of the root `Cargo.toml` workspace). The `[workspace]` key makes it self-contained. `opt-level = "z"` + LTO + strip minimizes binary size. `panic = "abort"` avoids unwinding machinery.

**Step 2: Create `init/.cargo/config.toml`**

```toml
[build]
target = "x86_64-unknown-linux-musl"
```

This ensures `cargo build` in the init directory always targets musl without needing `--target` flag.

**Step 3: Create minimal `init/src/main.rs`**

```rust
fn main() {
    // Placeholder — will be replaced in Task 2
    eprintln!("krun-init: not yet implemented");
    std::process::exit(1);
}
```

**Step 4: Verify it compiles**

```bash
cd init && cargo build --release
```

Expected: Compiles successfully. Binary at `init/target/x86_64-unknown-linux-musl/release/krun-init`.

**Step 5: Verify static linking**

```bash
file init/target/x86_64-unknown-linux-musl/release/krun-init
```

Expected: Output contains "statically linked".

**Step 6: Note that `init/Cargo.lock` is generated**

The first `cargo build` generates `init/Cargo.lock`. Commit this file alongside the crate scaffolding.

**Commit:** `chore(init): scaffold Rust init crate with musl target`

Include `init/Cargo.toml`, `init/Cargo.lock`, `init/.cargo/config.toml`, and `init/src/main.rs` in the commit.
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Implement complete init binary (main.rs)

**Verifies:** rust-init.AC1.3, rust-init.AC1.4, rust-init.AC4.1, rust-init.AC4.2, rust-init.AC4.3, rust-init.AC4.4, rust-init.AC4.5, rust-init.AC4.6, rust-init.AC5.1, rust-init.AC5.2, rust-init.AC5.3, rust-init.AC5.4, rust-init.AC5.5, rust-init.AC5.6

**Files:**
- Replace: `init/src/main.rs`

This is the complete Rust init binary that replaces `init/init.c` (1254 lines). It must replicate all non-SEV/non-TDX functionality from the C init.

**Implementation:**

The file follows the same sequential flow as `init/init.c:main()` (lines 1035-1253). Here is the complete implementation:

```rust
//! krun-init: PID 1 inside libkrun virtual machines.
//!
//! Mounts filesystems, configures the environment, and exec's the workload.
//! All configuration arrives via kernel cmdline environment variables (KRUN_*).

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::{env, ptr};

// ── Constants ──────────────────────────────────────────────────────

const KRUN_EXIT_CODE_IOCTL: libc::c_ulong = 0x7602;
const KRUN_REMOVE_ROOT_DIR_IOCTL: libc::c_ulong = 0x7603;
const VIRTIOFS_MAGIC: libc::c_long = 0x6573_5546;
const DEFAULT_INIT: &str = "/bin/sh";

// ── Filesystem mounting ────────────────────────────────────────────

fn mount_filesystems() -> Result<(), String> {
    let dirs_l1 = ["/dev", "/proc", "/sys"];
    let dirs_l2 = ["/dev/pts", "/dev/shm"];

    for dir in &dirs_l1 {
        let c = CString::new(*dir).unwrap();
        unsafe {
            if libc::mkdir(c.as_ptr(), 0o755) < 0 && *libc::__errno_location() != libc::EEXIST {
                return Err(format!("mkdir({})", dir));
            }
        }
    }

    c_mount("devtmpfs", "/dev", "devtmpfs", libc::MS_RELATIME, true)?;
    c_mount(
        "proc",
        "/proc",
        "proc",
        libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NOSUID | libc::MS_RELATIME,
        false,
    )?;
    c_mount(
        "sysfs",
        "/sys",
        "sysfs",
        libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NOSUID | libc::MS_RELATIME,
        false,
    )?;

    for dir in &dirs_l2 {
        let c = CString::new(*dir).unwrap();
        unsafe {
            if libc::mkdir(c.as_ptr(), 0o755) < 0 && *libc::__errno_location() != libc::EEXIST {
                return Err(format!("mkdir({})", dir));
            }
        }
    }

    c_mount(
        "devpts",
        "/dev/pts",
        "devpts",
        libc::MS_NOEXEC | libc::MS_NOSUID | libc::MS_RELATIME,
        false,
    )?;
    c_mount(
        "tmpfs",
        "/dev/shm",
        "tmpfs",
        libc::MS_NOEXEC | libc::MS_NOSUID | libc::MS_RELATIME,
        false,
    )?;

    // /dev/fd symlink — may fail if already exists, that's fine.
    let src = CString::new("/proc/self/fd").unwrap();
    let dst = CString::new("/dev/fd").unwrap();
    unsafe {
        libc::symlink(src.as_ptr(), dst.as_ptr());
    }

    Ok(())
}

fn c_mount(
    source: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    ignore_ebusy: bool,
) -> Result<(), String> {
    let c_source = CString::new(source).unwrap();
    let c_target = CString::new(target).unwrap();
    let c_fstype = CString::new(fstype).unwrap();
    unsafe {
        if libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            c_fstype.as_ptr(),
            flags,
            ptr::null(),
        ) < 0
        {
            let err = *libc::__errno_location();
            if ignore_ebusy && err == libc::EBUSY {
                return Ok(());
            }
            return Err(format!("mount({}): errno {}", target, err));
        }
    }
    Ok(())
}

// ── Block root device pivot ────────────────────────────────────────

fn pivot_to_block_root(device: &str) {
    let fstype = env::var("KRUN_BLOCK_ROOT_FSTYPE").ok();
    let options = env::var("KRUN_BLOCK_ROOT_OPTIONS").ok();

    let newroot = CString::new("/newroot").unwrap();
    unsafe {
        if libc::mkdir(newroot.as_ptr(), 0o755) < 0 && *libc::__errno_location() != libc::EEXIST {
            eprintln!("mkdir(/newroot) failed");
            libc::exit(-1);
        }
    }

    if try_mount_device(device, "/newroot", fstype.as_deref(), options.as_deref()) < 0 {
        eprintln!("mount KRUN_BLOCK_ROOT_DEVICE failed");
        unsafe { libc::exit(-1) };
    }

    let c_newroot = CString::new("/newroot").unwrap();
    let c_dot = CString::new(".").unwrap();
    let c_root = CString::new("/").unwrap();
    unsafe {
        libc::chdir(c_newroot.as_ptr());

        // Tell virtiofs to remove the temporary root directory
        let fd = libc::open(c_root.as_ptr(), libc::O_RDONLY);
        if fd >= 0 {
            libc::ioctl(fd, KRUN_REMOVE_ROOT_DIR_IOCTL);
            libc::close(fd);
        }

        if libc::mount(c_dot.as_ptr(), c_root.as_ptr(), ptr::null(), libc::MS_MOVE, ptr::null())
            < 0
        {
            eprintln!("remount root failed");
            libc::exit(-1);
        }
        libc::chroot(c_dot.as_ptr());
    }

    // Re-mount filesystems after chroot
    if let Err(e) = mount_filesystems() {
        eprintln!("Couldn't mount filesystems after chroot: {e}");
        unsafe { libc::exit(-2) };
    }
}

/// Try to mount a block device. If fstype is None, iterate /proc/filesystems.
fn try_mount_device(source: &str, target: &str, fstype: Option<&str>, options: Option<&str>) -> i32 {
    let c_source = CString::new(source).unwrap();
    let c_target = CString::new(target).unwrap();
    let c_options = options.map(|o| CString::new(o).unwrap());
    let opts_ptr = c_options.as_ref().map_or(ptr::null(), |c| c.as_ptr());

    if let Some(fs) = fstype {
        let c_fs = CString::new(fs).unwrap();
        unsafe {
            return libc::mount(c_source.as_ptr(), c_target.as_ptr(), c_fs.as_ptr(), 0, opts_ptr.cast());
        }
    }

    // No fstype specified — try each non-"nodev" filesystem from /proc/filesystems
    let path = CString::new("/proc/filesystems").unwrap();
    let mode = CString::new("r").unwrap();
    unsafe {
        let f = libc::fopen(path.as_ptr(), mode.as_ptr());
        if f.is_null() {
            return -1;
        }
        let mut buf = [0u8; 129];
        while libc::fgets(buf.as_mut_ptr().cast(), buf.len() as i32, f) != ptr::null_mut() {
            let line = CStr::from_ptr(buf.as_ptr().cast());
            let line_str = line.to_string_lossy();
            if line_str.starts_with("nodev") {
                continue;
            }
            let fs_name = line_str.trim();
            if fs_name.is_empty() {
                continue;
            }
            let c_fs = CString::new(fs_name).unwrap();
            if libc::mount(c_source.as_ptr(), c_target.as_ptr(), c_fs.as_ptr(), 0, opts_ptr.cast()) == 0 {
                libc::fclose(f);
                return 0;
            }
        }
        libc::fclose(f);
    }
    -1
}

// ── Stdio redirect (sysfs scan — replaced by env vars in Phase 3) ──

fn setup_redirects() {
    // Phase 1: keep the existing sysfs-based redirect approach from init.c
    // Phase 3 will replace this with env-var-based redirect (KRUN_STDIN_DEV, etc.)
    let dir_path = CString::new("/sys/class/virtio-ports").unwrap();
    unsafe {
        let dir = libc::opendir(dir_path.as_ptr());
        if dir.is_null() {
            eprintln!("Unable to open ports directory");
            return;
        }

        loop {
            let entry = libc::readdir(dir);
            if entry.is_null() {
                break;
            }
            let name = CStr::from_ptr((*entry).d_name.as_ptr());
            let name_str = name.to_string_lossy();

            // Read the port name file
            let name_path = format!("/sys/class/virtio-ports/{}/name", name_str);
            let c_path = CString::new(name_path).unwrap();
            let mode = CString::new("r").unwrap();
            let f = libc::fopen(c_path.as_ptr(), mode.as_ptr());
            if f.is_null() {
                continue;
            }
            let mut buf = [0u8; 256];
            let ret = libc::fgets(buf.as_mut_ptr().cast(), buf.len() as i32, f);
            libc::fclose(f);
            if ret.is_null() {
                continue;
            }
            let port_name = CStr::from_ptr(buf.as_ptr().cast()).to_string_lossy();

            let dev_path = format!("/dev/{}", name_str);
            if port_name.trim_end() == "krun-stdin" {
                reopen_fd(libc::STDIN_FILENO, &dev_path, libc::O_RDONLY);
            } else if port_name.trim_end() == "krun-stdout" {
                reopen_fd(libc::STDOUT_FILENO, &dev_path, libc::O_WRONLY);
            } else if port_name.trim_end() == "krun-stderr" {
                reopen_fd(libc::STDERR_FILENO, &dev_path, libc::O_WRONLY);
            }
        }

        libc::closedir(dir);
    }
}

fn reopen_fd(fd: i32, path: &str, flags: i32) {
    let c_path = CString::new(path).unwrap();
    unsafe {
        let newfd = libc::open(c_path.as_ptr(), flags);
        if newfd < 0 {
            eprintln!("Failed to open '{}': errno {}", path, *libc::__errno_location());
            return;
        }
        if libc::dup2(newfd, fd) < 0 {
            eprintln!("dup2 failed: errno {}", *libc::__errno_location());
            libc::close(newfd);
            return;
        }
        if newfd != fd {
            libc::close(newfd);
        }
    }
}

// ── Exit code reporting ────────────────────────────────────────────

fn is_virtiofs(path: &str) -> i32 {
    let c_path = CString::new(path).unwrap();
    unsafe {
        let mut fs: libc::statfs = std::mem::zeroed();
        if libc::statfs(c_path.as_ptr(), &mut fs) != 0 {
            return -1;
        }
        if fs.f_type == VIRTIOFS_MAGIC {
            1
        } else {
            0
        }
    }
}

fn set_exit_code(code: i32) {
    let virtiofs_check = is_virtiofs("/");
    if virtiofs_check < 0 {
        eprintln!("Warning: Could not determine filesystem type for root");
    }
    if virtiofs_check != 1 {
        return;
    }
    let c_root = CString::new("/").unwrap();
    unsafe {
        let fd = libc::open(c_root.as_ptr(), libc::O_RDONLY);
        if fd < 0 {
            eprintln!("Couldn't open root filesystem to report exit code");
            return;
        }
        let ret = libc::ioctl(fd, KRUN_EXIT_CODE_IOCTL, code);
        if ret < 0 {
            eprintln!("Error using the ioctl to set the exit code");
        }
        libc::close(fd);
    }
}

// ── Resource limits ────────────────────────────────────────────────

fn set_rlimits(rlimits_str: &str) {
    let mut chars = rlimits_str;
    loop {
        let (id, rest) = parse_u64(chars);
        if rest.is_empty() || !rest.starts_with(',') {
            break;
        }
        let (cur, rest) = parse_u64(&rest[1..]);
        if rest.is_empty() || !rest.starts_with(',') {
            break;
        }
        let (max, rest) = parse_u64(&rest[1..]);

        let rlim = libc::rlimit {
            rlim_cur: cur,
            rlim_max: max,
        };
        unsafe {
            if libc::setrlimit(id as libc::__rlimit_resource_t, &rlim) != 0 {
                eprintln!("Error setting rlimit for ID={id}");
            }
        }

        if rest.is_empty() {
            break;
        }
        // Skip separator (space)
        chars = rest.trim_start();
    }
}

fn parse_u64(s: &str) -> (u64, &str) {
    let end = s
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(s.len());
    let val = s[..end].parse::<u64>().unwrap_or(u64::MAX);
    (val, &s[end..])
}

// ── Loopback interface ─────────────────────────────────────────────

fn bring_up_loopback() {
    unsafe {
        let sockfd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if sockfd < 0 {
            return;
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        let lo = b"lo\0";
        ifr.ifr_name[..lo.len()].copy_from_slice(&lo.map(|b| b as libc::c_char));
        ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        libc::ioctl(sockfd, libc::SIOCSIFFLAGS, &ifr);
        libc::close(sockfd);
    }
}

// ── Main ───────────────────────────────────────────────────────────

fn main() {
    // Mount base filesystems
    if let Err(e) = mount_filesystems() {
        eprintln!("Couldn't mount filesystems: {e}");
        std::process::exit(-2);
    }

    // Block root device pivot (if configured)
    if let Ok(device) = env::var("KRUN_BLOCK_ROOT_DEVICE") {
        pivot_to_block_root(&device);
    }

    // Set shared mount propagation on root
    let c_root = CString::new("/").unwrap();
    unsafe {
        if libc::mount(
            ptr::null(),
            c_root.as_ptr(),
            ptr::null(),
            libc::MS_REC | libc::MS_SHARED,
            ptr::null(),
        ) < 0
        {
            eprintln!("Couldn't set shared propagation on the root mount");
            libc::exit(-1);
        }
    }

    // Create new session and set controlling terminal
    unsafe {
        libc::setsid();
        libc::ioctl(0, libc::TIOCSCTTY, 1);
    }

    // Bring up loopback interface
    bring_up_loopback();

    // Apply environment configuration
    if let Ok(home) = env::var("KRUN_HOME") {
        env::set_var("HOME", &home);
    }
    if let Ok(term) = env::var("KRUN_TERM") {
        env::set_var("TERM", &term);
    }

    // Set hostname
    match env::var("HOSTNAME") {
        Ok(hostname) => {
            let c_hostname = CString::new(hostname.as_str()).unwrap();
            unsafe {
                libc::sethostname(c_hostname.as_ptr(), hostname.len());
            }
        }
        Err(_) => {
            let localhost = CString::new("localhost").unwrap();
            unsafe {
                libc::sethostname(localhost.as_ptr(), 9);
            }
        }
    }

    // Apply resource limits
    if let Ok(rlimits) = env::var("KRUN_RLIMITS") {
        set_rlimits(&rlimits);
    }

    // Set working directory
    if let Ok(workdir) = env::var("KRUN_WORKDIR") {
        let c_workdir = CString::new(workdir.as_str()).unwrap();
        unsafe {
            libc::chdir(c_workdir.as_ptr());
        }
    }

    // Determine exec argv
    let krun_init = env::var("KRUN_INIT").ok();
    let exec_path = krun_init.as_deref().unwrap_or(DEFAULT_INIT);

    // Build argv from command line args
    let args: Vec<String> = env::args().collect();
    let mut exec_args: Vec<CString> = Vec::new();

    // argv[0] is always the exec path
    exec_args.push(CString::new(exec_path).unwrap());

    // Remaining args from the kernel cmdline " -- " separator
    for arg in args.iter().skip(1) {
        if let Ok(c) = CString::new(arg.as_str()) {
            exec_args.push(c);
        }
    }

    let exec_argv: Vec<*const libc::c_char> = exec_args
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(ptr::null()))
        .collect();

    // Check PID 1 mode
    let init_pid1 = env::var("KRUN_INIT_PID1")
        .map(|v| v.starts_with('1'))
        .unwrap_or(false);

    if init_pid1 {
        // Direct exec — no fork
        setup_redirects();
        unsafe {
            libc::execvp(exec_argv[0], exec_argv.as_ptr());
            let err = *libc::__errno_location();
            eprintln!(
                "Couldn't execute '{}' inside the vm: errno {}",
                exec_path, err
            );
            if err == libc::ENOENT {
                libc::exit(127);
            } else {
                libc::exit(126);
            }
        }
    }

    // Fork + exec
    let child = unsafe { libc::fork() };
    if child < 0 {
        eprintln!("fork failed");
        set_exit_code(125);
        std::process::exit(125);
    }

    if child == 0 {
        // Child: redirect stdio and exec
        setup_redirects();
        unsafe {
            libc::execvp(exec_argv[0], exec_argv.as_ptr());
            let err = *libc::__errno_location();
            eprintln!(
                "Couldn't execute '{}' inside the vm: errno {}",
                exec_path, err
            );
            if err == libc::ENOENT {
                libc::exit(127);
            } else {
                libc::exit(126);
            }
        }
    }

    // Parent: wait for workload child
    let mut status: i32 = 0;
    unsafe {
        loop {
            let pid = libc::waitpid(-1, &mut status, 0);
            if pid == child {
                break;
            }
        }
    }

    if libc::WIFEXITED(status) {
        set_exit_code(libc::WEXITSTATUS(status));
    } else if libc::WIFSIGNALED(status) {
        set_exit_code(libc::WTERMSIG(status) + 128);
    }

    // Drain console output before exit
    unsafe {
        libc::tcdrain(libc::STDOUT_FILENO);
        libc::tcdrain(libc::STDERR_FILENO);
    }
}
```

**Key differences from C init:**
- **BREAKING CHANGE:** JSON config file support (`KRUN_CONFIG`, `/.krun_config.json`) is intentionally removed per design. The C init's `config_parse_file()` parsed `Env`, `args`/`Cmd`, `WorkingDir`/`Cwd`, and `Entrypoint` fields from a JSON file. Users relying on container-style JSON config must migrate to kernel cmdline env vars (`KRUN_INIT`, `KRUN_WORKDIR`, etc.). Flag this in the commit message and PR description.
- No `#ifdef SEV` / `#ifdef TDX` branches (out of scope per design)
- No `#ifdef __TIMESYNC__` clock worker (out of scope)
- `setup_redirects()` still uses sysfs scanning in Phase 1 (will be replaced in Phase 3 with env-var-based approach)
- Kept `try_mount_device()` for block root device fstype auto-detection (iterates `/proc/filesystems`)
- `set_exit_code()` skips the ioctl on `statfs` error (C init falls through to ioctl on error — this is an intentional improvement)

**Step 1: Replace `init/src/main.rs` with the code above**

**Step 2: Verify it compiles**

```bash
cd init && cargo build --release
```

Expected: Compiles with no errors. Warnings about unused imports are acceptable.

**Commit:** `feat(init): implement Rust init binary replacing C init`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Add justfile `build-init` rule and update `build`

**Verifies:** rust-init.AC1.1

**Files:**
- Modify: `justfile:1-14` (add `build-init` rule, update `build` to depend on it)

**Implementation:**

Add a `build-init` recipe that compiles the init binary and copies it to the expected `init/init` path. Update the `build` recipe to depend on `build-init`.

After line 2 (`features := ...`), add:

```just
# Build the guest init binary (static musl)
build-init:
    cd init && cargo build --release
    cp init/target/x86_64-unknown-linux-musl/release/krun-init init/init
```

Change the existing `build` recipe from:
```just
build:
    cargo build --release -p libkrun --features {{features}}
```
to:
```just
build: build-init
    cargo build --release -p libkrun --features {{features}}
```

**Step 1: Modify the justfile as described above**

**Step 2: Verify `just build-init` works**

```bash
just build-init
```

Expected: Builds the init crate and copies the binary to `init/init`.

**Step 3: Verify the binary is static**

```bash
file init/init
ldd init/init
```

Expected: `file` shows "statically linked". `ldd` shows "not a dynamic executable" or similar.

**Commit:** `chore(build): add just build-init rule for Rust init`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Verify integration tests pass with Rust init

**Verifies:** rust-init.AC1.3, rust-init.AC4.1, rust-init.AC4.2, rust-init.AC5.1, rust-init.AC5.3, rust-init.AC5.4, rust-init.AC5.5

**Files:**
- No code changes — verification only

**Step 1: Build the init binary**

```bash
just build-init
```

**Step 2: Build the full library**

```bash
just build
```

**Step 3: Run the core integration tests**

```bash
just integration configure-vm-1cpu-256MiB
just integration rust-api-builder-lifecycle
just integration vm-exit-clean-shutdown
```

Expected: All three tests pass. These exercise the boot path, env var config (KRUN_INIT, KRUN_WORKDIR), exit code reporting, and the fork+exec lifecycle.

**Step 4: Run the full integration suite**

```bash
just integration
```

Expected: All 49 tests pass (some may be flaky under load — re-run failures individually).

**Commit:** No commit — verification only.
<!-- END_TASK_4 -->
