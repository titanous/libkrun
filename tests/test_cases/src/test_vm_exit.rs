//! Test suite for VM exit handling: verify context.run() returns VmExit and resources are cleaned up.
//!
//! Tests:
//! - AC1.4: Context::run() returns control (process does not terminate)
//! - AC4.1: Thread count returns to baseline after VM exits
//! - AC4.2: File descriptor count returns to baseline after VM exits
//! - AC4.3: Memory map count returns to baseline after VM exits

use macros::{guest, host};

pub struct TestVmExit;
pub struct TestVmExitObserver;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::fs;

    /// Count entries in a /proc/self directory.
    fn count_proc_entries(path: &str) -> usize {
        fs::read_dir(path)
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    /// Count lines in /proc/self/maps.
    fn count_maps() -> usize {
        fs::read_to_string("/proc/self/maps")
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    impl Test for TestVmExit {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            // Snapshot resource counts before VM
            let threads_before = count_proc_entries("/proc/self/task");
            let fds_before = count_proc_entries("/proc/self/fd");
            let maps_before = count_maps();

            // Build and run VM
            let mut builder = krun::Builder::new();
            builder.vm_config(1, 256)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;
            let result = context.run();

            // AC1.4: Caller regains control — we're here, process didn't terminate
            let vm_exit = result?;

            // Verify VmExit::Shutdown { exit_code: 0 }
            assert_eq!(
                vm_exit,
                krun::VmExit::Shutdown { exit_code: 0 },
                "Expected clean shutdown with exit_code 0, got {vm_exit:?}"
            );

            // Small sleep to allow threads to finish cleanup
            std::thread::sleep(std::time::Duration::from_millis(100));

            // AC4.1: Thread count returns to baseline (±2 tolerance for async
            // runtime threads that may not have terminated yet)
            let threads_after = count_proc_entries("/proc/self/task");
            let thread_diff = (threads_after as i64 - threads_before as i64).unsigned_abs();
            assert!(
                thread_diff <= 2,
                "Thread leak: before={threads_before}, after={threads_after}, diff={thread_diff}"
            );

            // AC4.2: FD count returns to baseline (±2 tolerance)
            let fds_after = count_proc_entries("/proc/self/fd");
            let fd_diff = (fds_after as i64 - fds_before as i64).unsigned_abs();
            assert!(
                fd_diff <= 2,
                "FD leak: before={fds_before}, after={fds_after}, diff={fd_diff}"
            );

            // AC4.3: Memory map count returns to baseline (±2 tolerance)
            let maps_after = count_maps();
            let maps_diff = (maps_after as i64 - maps_before as i64).unsigned_abs();
            assert!(
                maps_diff <= 2,
                "Memory map leak: before={maps_before}, after={maps_after}, diff={maps_diff}"
            );

            println!("OK");
            Ok(())
        }
    }

    impl Test for TestVmExitObserver {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            // AC3.1: Exit observers fire before run() returns
            let observer_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = observer_called.clone();

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 256)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;

            // Register a mock exit observer that sets the flag
            context.register_exit_observer(std::sync::Arc::new(std::sync::Mutex::new(
                move || {
                    flag.store(true, std::sync::atomic::Ordering::Release);
                },
            )));

            let vm_exit = context.run()?;

            // Verify observer was called before run() returned
            assert!(
                observer_called.load(std::sync::atomic::Ordering::Acquire),
                "Exit observer was not called before run() returned"
            );

            assert_eq!(
                vm_exit,
                krun::VmExit::Shutdown { exit_code: 0 },
                "Expected clean shutdown, got {vm_exit:?}"
            );

            println!("OK");
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    impl Test for TestVmExit {
        fn in_guest(self: Box<Self>) {
            // Trivial workload — guest exits cleanly
            println!("OK");
        }
    }

    impl Test for TestVmExitObserver {
        fn in_guest(self: Box<Self>) {
            // Guest exits cleanly — host prints OK after verifying observer
        }
    }
}
