use macros::{guest, host};

pub struct TestSnapshotWrongMagic;
pub struct TestSnapshotVcpuMismatch;
pub struct TestSnapshotNestedMismatch;

#[host]
mod host {
    use super::*;
    use crate::{Test, TestSetup};
    use std::fs;
    use std::path::Path;

    /// Write a minimal snapshot directory for error-path testing.
    /// Uses the real VmSnapshot/SnapshotHeader types and save_vmstate to
    /// produce correctly-encoded bincode-next data.
    fn write_vmstate(dir: &Path, magic: u32, version: u32, vcpu_count: u32, nested: bool) {
        fs::create_dir_all(dir).unwrap();

        let snapshot = krun::VmSnapshot {
            header: krun::SnapshotHeader {
                magic,
                version,
                vcpu_count,
                ram_regions: vec![],
                nested_enabled: nested,
            },
            vcpu_states: vec![vec![]; vcpu_count as usize],
            device_states: vec![],
            gic_state: None,
            vm_state: None,
            excluded_pages: vec![],
        };
        krun::save_vmstate(&snapshot, &dir.join("vmstate")).unwrap();

        // Empty memory file — error occurs before memory is read
        fs::File::create(dir.join("memory")).unwrap();
    }

    fn build_minimal_context(_test_setup: &TestSetup) -> anyhow::Result<krun::Context> {
        let mut builder = krun::Builder::new();
        builder.vm_config(1, 128)?;
        // restore_and_run fails on snapshot validation before needing root/exec.
        // The snapshot validation happens in restore_and_run before any guest execution,
        // so we don't need to set up the guest filesystem for these error tests.
        Ok(builder.build()?)
    }

    fn expect_snapshot_error(
        result: Result<krun::VmExit, krun::StartError>,
        expected_msg_fragment: &str,
    ) {
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains(expected_msg_fragment),
                    "Expected error containing '{expected_msg_fragment}', got: {msg}"
                );
            }
            Ok(_) => panic!("Expected error but restore_and_run succeeded"),
        }
    }

    impl Test for TestSnapshotWrongMagic {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("bad_snap");
            write_vmstate(&snap_dir, 0xDEADBEEF, 1, 1, false); // wrong magic

            let context = build_minimal_context(&test_setup)?;
            let result = context.restore_and_run(&snap_dir, &[]);
            expect_snapshot_error(result, "Invalid snapshot magic number");
            println!("OK");
            Ok(())
        }
    }

    impl Test for TestSnapshotVcpuMismatch {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("bad_snap");
            write_vmstate(&snap_dir, 0x4B52_534E, 1, 4, false); // vcpu_count=4, VM has 1

            let context = build_minimal_context(&test_setup)?;
            let result = context.restore_and_run(&snap_dir, &[]);
            expect_snapshot_error(result, "vCPU count mismatch");
            println!("OK");
            Ok(())
        }
    }

    impl Test for TestSnapshotNestedMismatch {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let snap_dir = test_setup.tmp_dir.join("bad_snap");
            // nested_enabled=true in header, but VM has nested=false (default)
            write_vmstate(&snap_dir, 0x4B52_534E, 1, 1, true);

            let context = build_minimal_context(&test_setup)?;
            let result = context.restore_and_run(&snap_dir, &[]);
            expect_snapshot_error(result, "Nested virtualization enabled mismatch");
            println!("OK");
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    impl Test for TestSnapshotWrongMagic {
        fn in_guest(self: Box<Self>) {
            // These are host-only tests; guest never runs
        }
    }

    impl Test for TestSnapshotVcpuMismatch {
        fn in_guest(self: Box<Self>) {
            // These are host-only tests; guest never runs
        }
    }

    impl Test for TestSnapshotNestedMismatch {
        fn in_guest(self: Box<Self>) {
            // These are host-only tests; guest never runs
        }
    }
}
