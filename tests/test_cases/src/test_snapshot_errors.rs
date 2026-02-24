use macros::{guest, host};

pub struct TestSnapshotWrongMagic;
pub struct TestSnapshotVcpuMismatch;
pub struct TestSnapshotNestedMismatch;

#[host]
mod host {
    use super::*;
    use crate::{Test, TestSetup};
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    /// Write a minimal snapshot directory for error-path testing.
    /// Produces a valid bincode-encoded VmSnapshot at `dir/vmstate` with the
    /// given header fields, and an empty `dir/memory` file (error happens before
    /// memory is read for AC6.3/6.4/6.5).
    fn write_vmstate(dir: &Path, magic: u32, version: u32, vcpu_count: u32, nested: bool) {
        // Actual RAM layout from vm_config(1, 128): three regions
        // (verified from actual error messages when layout mismatch fires)
        let ram_regions = vec![
            (0u64, 16777216u64),         // Region 1: base=0, size=16MiB
            (16777216u64, 21168128u64),  // Region 2: base=16MiB, size=~20.2MiB
            (37945344u64, 134217728u64), // Region 3: base=~36.2MiB, size=128MiB
        ];
        write_vmstate_with_regions(dir, magic, version, vcpu_count, nested, ram_regions);
    }

    /// Helper to encode snapshot with explicit RAM regions (for testing purposes).
    fn write_vmstate_with_regions(
        dir: &Path,
        magic: u32,
        version: u32,
        vcpu_count: u32,
        nested: bool,
        ram_regions: Vec<(u64, u64)>,
    ) {
        fs::create_dir_all(dir).unwrap();

        // Hand-encode a minimal VmSnapshot in bincode 1.3 format (little-endian,
        // u64 length prefix for collections).
        let mut data = Vec::new();

        // SnapshotHeader
        data.extend_from_slice(&magic.to_le_bytes());       // magic: u32
        data.extend_from_slice(&version.to_le_bytes());      // version: u32
        data.extend_from_slice(&vcpu_count.to_le_bytes());   // vcpu_count: u32
        // ram_regions: Vec<(u64,u64)>
        data.extend_from_slice(&(ram_regions.len() as u64).to_le_bytes()); // Vec length
        for (base, size) in &ram_regions {
            data.extend_from_slice(&base.to_le_bytes());
            data.extend_from_slice(&size.to_le_bytes());
        }
        data.push(nested as u8);                             // nested_enabled: bool

        // vcpu_states: Vec<Vec<u8>> with vcpu_count entries (each empty)
        data.extend_from_slice(&(vcpu_count as u64).to_le_bytes()); // Vec length = vcpu_count
        for _ in 0..vcpu_count {
            data.extend_from_slice(&0u64.to_le_bytes()); // each vCPU state is empty Vec<u8>
        }
        // device_states: empty Vec<(String,Vec<u8>)> = length 0
        data.extend_from_slice(&0u64.to_le_bytes());
        // gic_state: None = 0x00 (bincode Option::None discriminant)
        data.push(0u8);
        // vm_state: None = 0x00
        data.push(0u8);

        let mut f = fs::File::create(dir.join("vmstate")).unwrap();
        f.write_all(&data).unwrap();

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

    fn expect_snapshot_error(result: Result<(), krun::StartError>, expected_msg_fragment: &str) {
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains(expected_msg_fragment),
                    "Expected error containing '{expected_msg_fragment}', got: {msg}"
                );
            }
            Ok(()) => panic!("Expected error but restore_and_run succeeded"),
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
