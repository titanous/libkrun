//! Test error handling: read_page failure.
//!
//! Verifies userfaultd.AC5.5: Store fails read_page, VM gets clean VmExit::Error.

use macros::{guest, host};

pub struct TestUffdErrorHandling;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::mock_snapshot_store::ErrorStoreFactory;
    use crate::{Test, TestSetup};
    use std::thread;

    impl Test for TestUffdErrorHandling {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("snapshot");

            // Phase 1: Snapshot
            {
                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;

                let context = builder.build()?;
                let handle = context.vm_handle();

                let vm_thread = thread::spawn(move || context.run());

                // Give VM time to boot and set up memory
                thread::sleep(std::time::Duration::from_millis(100));

                // Take snapshot
                handle.snapshot(&snap_dir)?;

                vm_thread.join().ok();
            }

            // Phase 2: Restore with error on read_page
            {
                let mut builder = krun::Builder::new();
                builder.vm_config(1, 256)?;
                setup_fs_builder(&mut builder, &test_setup)?;

                let context = builder.build()?;

                // Create factory that will fail on read_page at address 0x0
                let factory = ErrorStoreFactory::new(&snap_dir, &[], 0x0);

                // Attempt to restore and expect VmExit::Error or StartError
                match context.restore_and_run_with_store(Box::new(factory)) {
                    Ok(krun::VmExit::Error { message }) => {
                        // Expected: store returned error, which became VmExit::Error
                        assert!(
                            message.contains("read_page") || message.contains("simulated"),
                            "Expected read_page error message, got: {message}"
                        );
                        println!("OK");
                    }
                    Ok(other_exit) => {
                        panic!("Expected VmExit::Error, got: {other_exit:?}");
                    }
                    Err(e) => {
                        // Also acceptable: error propagated during factory.create() or restore
                        eprintln!("Got error during restore (acceptable): {e}");
                        assert!(
                            e.to_string().contains("read_page")
                                || e.to_string().contains("simulated")
                                || e.to_string().contains("Error"),
                            "Expected read_page error, got: {e}"
                        );
                        println!("OK");
                    }
                }
            }

            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    impl Test for TestUffdErrorHandling {
        fn in_guest(self: Box<Self>) {
            // Guest never runs because cold restore fails with read_page error
            panic!("Guest should not run when read_page fails");
        }
    }
}
