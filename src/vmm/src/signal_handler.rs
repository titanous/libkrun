// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

use libc::{_exit, c_int, c_void, siginfo_t, SIGBUS, SIGINT, SIGSEGV, SIGSYS, SIGWINCH};
use utils::signal::register_signal_handler;

// The offset of `si_syscall` (offending syscall identifier) within the siginfo structure
// expressed as an `(u)int*`.
// Offset `6` for an `i32` field means that the needed information is located at `6 * sizeof(i32)`
// = byte offset 24.
//
// Layout verification:
//   siginfo_t on x86_64 and aarch64 (from kernel UAPI headers):
//     struct siginfo {
//       int si_signo;    // offset  0, bytes 0..4
//       int si_errno;    // offset  4, bytes 4..8
//       int si_code;     // offset  8, bytes 8..12
//       union {
//         struct {       // _sigsys
//           void *_call_addr;  // offset 12 on 32-bit, 16 on 64-bit (pointer-sized)
//           int   _syscall;    // byte offset 24 on 64-bit (pointer is 8 bytes)
//           ...
//         }
//       }
//     }
//   References:
//     - include/uapi/asm-generic/siginfo.h (si_syscall at offset 24 on 64-bit targets)
//     - https://github.com/rust-lang/libc/issues/716 (why offset differs in Rust's siginfo_t)
//
// SI_OFF_SYSCALL = 6 means element index 6 in an i32 array = byte offset 24.
// This is only correct on 64-bit architectures (x86_64, aarch64) where pointers are 8 bytes.
// The compile-time assertion below verifies the byte offset and that siginfo_t is large enough.
const SI_OFF_SYSCALL: isize = 6;

// Compile-time assertions to verify SI_OFF_SYSCALL layout assumptions.
// SI_OFF_SYSCALL * 4 must equal 24 (the known byte offset of si_syscall on 64-bit platforms).
const _: () = assert!(
    SI_OFF_SYSCALL * 4 == 24,
    "SI_OFF_SYSCALL byte offset must be 24 on 64-bit targets"
);
// siginfo_t must be large enough to hold 4 bytes (i32) at byte offset 24.
const _: () = assert!(
    core::mem::size_of::<libc::siginfo_t>() >= 28,
    "siginfo_t is too small to contain si_syscall at byte offset 24"
);

const SYS_SECCOMP_CODE: i32 = 1;

static CONSOLE_SIGWINCH_FD: AtomicI32 = AtomicI32::new(-1);
static CONSOLE_SIGINT_FD: AtomicI32 = AtomicI32::new(-1);

/// Signal handler for `SIGSYS`.
///
/// Increments the `seccomp.num_faults` metric, logs an error message and terminates the process
/// with a specific exit code.
///
/// # Safety
///
/// This function is called by the OS as a signal handler. The `info` pointer is valid and points
/// to a `siginfo_t` for the duration of the call. The `si_syscall` field is read by casting
/// `info` to `*const i32` and indexing at `SI_OFF_SYSCALL` (element index 6, byte offset 24).
/// This is correct on 64-bit architectures (x86_64, aarch64) per the kernel UAPI layout in
/// `include/uapi/asm-generic/siginfo.h`. The compile-time assertions above verify the offset
/// arithmetic and that `siginfo_t` is large enough. If `libc::siginfo_t` ever changes its size
/// or layout, those assertions will fail at compile time.
extern "C" fn sigsys_handler(num: c_int, info: *mut siginfo_t, _unused: *mut c_void) {
    // Safe because we're just reading some fields from a supposedly valid argument.
    let si_signo = unsafe { (*info).si_signo };
    let si_code = unsafe { (*info).si_code };

    // Sanity check. The condition should never be true.
    if num != si_signo || num != SIGSYS || si_code != SYS_SECCOMP_CODE {
        // Safe because we're terminating the process anyway.
        unsafe { _exit(i32::from(super::FC_EXIT_CODE_UNEXPECTED_ERROR)) };
    }

    // Other signals which might do async unsafe things incompatible with the rest of this
    // function are blocked due to the sa_mask used when registering the signal handler.
    let syscall = unsafe { *(info as *const i32).offset(SI_OFF_SYSCALL) as usize };
    error!("Shutting down VM after intercepting a bad syscall ({syscall}).");
    // Safe because we're terminating the process anyway. We don't actually do anything when
    // running unit tests.
    #[cfg(not(test))]
    unsafe {
        _exit(i32::from(super::FC_EXIT_CODE_BAD_SYSCALL))
    };
}

/// Signal handler for `SIGBUS` and `SIGSEGV`.
///
/// Logs an error message and terminates the process with a specific exit code.
extern "C" fn sigbus_sigsegv_handler(num: c_int, info: *mut siginfo_t, _unused: *mut c_void) {
    // Safe because we're just reading some fields from a supposedly valid argument.
    let si_signo = unsafe { (*info).si_signo };
    let si_code = unsafe { (*info).si_code };

    // Sanity check. The condition should never be true.
    if num != si_signo || (num != SIGBUS && num != SIGSEGV) {
        // Safe because we're terminating the process anyway.
        unsafe { _exit(i32::from(super::FC_EXIT_CODE_UNEXPECTED_ERROR)) };
    }

    error!("Shutting down VM after intercepting signal {si_signo}, code {si_code}.");

    // Safe because we're terminating the process anyway. We don't actually do anything when
    // running unit tests.
    #[cfg(not(test))]
    unsafe {
        _exit(i32::from(match si_signo {
            SIGBUS => super::FC_EXIT_CODE_SIGBUS,
            SIGSEGV => super::FC_EXIT_CODE_SIGSEGV,
            _ => super::FC_EXIT_CODE_UNEXPECTED_ERROR,
        }))
    };
}

extern "C" fn sigwinch_handler(num: c_int, info: *mut siginfo_t, _unused: *mut c_void) {
    // Safe because we're just reading some fields from a supposedly valid argument.
    let si_signo = unsafe { (*info).si_signo };

    // Sanity check. The condition should never be true.
    if num != si_signo || num != SIGWINCH {
        // Safe because we're terminating the process anyway.
        unsafe { _exit(i32::from(super::FC_EXIT_CODE_UNEXPECTED_ERROR)) };
    }

    let val: u64 = 1;
    let console_fd = CONSOLE_SIGWINCH_FD.load(Ordering::Relaxed);
    let _ = unsafe { libc::write(console_fd, &val as *const _ as *const c_void, 8) };
}

extern "C" fn sigint_handler(num: c_int, info: *mut siginfo_t, _unused: *mut c_void) {
    // Safe because we're just reading some fields from a supposedly valid argument.
    let si_signo = unsafe { (*info).si_signo };

    // Sanity check. The condition should never be true.
    if num != si_signo || num != SIGINT {
        // Safe because we're terminating the process anyway.
        unsafe { _exit(i32::from(super::FC_EXIT_CODE_UNEXPECTED_ERROR)) };
    }

    let val: u64 = 1;
    let console_fd = CONSOLE_SIGINT_FD.load(Ordering::Relaxed);
    let _ = unsafe { libc::write(console_fd, &val as *const _ as *const c_void, 8) };
}

pub fn register_sigwinch_handler(console_fd: RawFd) -> utils::errno::Result<()> {
    CONSOLE_SIGWINCH_FD.store(console_fd, Ordering::Relaxed);

    register_signal_handler(SIGWINCH, sigwinch_handler)?;

    Ok(())
}

pub fn register_sigint_handler(sigint_fd: RawFd) -> utils::errno::Result<()> {
    CONSOLE_SIGINT_FD.store(sigint_fd, Ordering::Relaxed);

    register_signal_handler(SIGINT, sigint_handler)?;

    Ok(())
}

/// Registers all the required signal handlers.
///
/// Custom handlers are installed for: `SIGBUS`, `SIGSEGV`, `SIGSYS`.
pub fn register_signal_handlers() -> utils::errno::Result<()> {
    // Call to unsafe register_signal_handler which is considered unsafe because it will
    // register a signal handler which will be called in the current thread and will interrupt
    // whatever work is done on the current thread, so we have to keep in mind that the registered
    // signal handler must only do async-signal-safe operations.
    register_signal_handler(SIGSYS, sigsys_handler)?;
    register_signal_handler(SIGBUS, sigbus_sigsegv_handler)?;
    register_signal_handler(SIGSEGV, sigbus_sigsegv_handler)?;

    Ok(())
}
