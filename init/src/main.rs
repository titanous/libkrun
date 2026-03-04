//! krun-init: PID 1 inside libkrun virtual machines.
//!
//! Mounts filesystems, configures the environment, and exec's the workload.
//! All configuration arrives via kernel cmdline environment variables (KRUN_*).

use std::ffi::{CStr, CString};
use std::{env, ptr};

// ── Constants ──────────────────────────────────────────────────────

const KRUN_EXIT_CODE_IOCTL: i32 = 0x7602;
const KRUN_REMOVE_ROOT_DIR_IOCTL: i32 = 0x7603;
const VIRTIOFS_MAGIC: u64 = 0x6573_5546;
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
            let _ = libc::ioctl(fd, KRUN_REMOVE_ROOT_DIR_IOCTL);
            libc::close(fd);
        }

        if libc::mount(
            c_dot.as_ptr(),
            c_root.as_ptr(),
            ptr::null(),
            libc::MS_MOVE,
            ptr::null(),
        ) < 0
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
fn try_mount_device(
    source: &str,
    target: &str,
    fstype: Option<&str>,
    options: Option<&str>,
) -> i32 {
    let c_source = CString::new(source).unwrap();
    let c_target = CString::new(target).unwrap();
    let c_options = options.map(|o| CString::new(o).unwrap());
    let opts_ptr = c_options.as_ref().map_or(ptr::null(), |c| c.as_ptr());

    if let Some(fs) = fstype {
        let c_fs = CString::new(fs).unwrap();
        unsafe {
            return libc::mount(
                c_source.as_ptr(),
                c_target.as_ptr(),
                c_fs.as_ptr(),
                0,
                opts_ptr.cast(),
            );
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
        while !libc::fgets(buf.as_mut_ptr().cast(), buf.len() as i32, f).is_null() {
            // Safety: fgets guarantees null-termination within n bytes and the buffer is zero-initialized
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
            if libc::mount(
                c_source.as_ptr(),
                c_target.as_ptr(),
                c_fs.as_ptr(),
                0,
                opts_ptr.cast(),
            ) == 0
            {
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
            eprintln!(
                "Failed to open '{}': errno {}",
                path,
                *libc::__errno_location()
            );
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
        if (fs.f_type as u64) == VIRTIOFS_MAGIC {
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
            if libc::setrlimit(id as i32, &rlim) != 0 {
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
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
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
        let _ = libc::ioctl(sockfd, libc::SIOCSIFFLAGS as i32, &ifr);
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
            if pid == child || pid < 0 {
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
