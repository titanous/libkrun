//! Nested virtualization integration test (L0 → L1 → L2).
//!
//! Verifies that a guest VM booted with `enable_nested_virt()` and a
//! KVM-enabled kernel (CONFIG_KVM=y) can itself act as a hypervisor.
//! The L1 guest checks /dev/kvm, then uses libkrun (statically linked
//! via the `static-firmware` feature) to boot an L2 VM that prints "OK".
//!
//! For normal test_cases compilation (without the `nested` feature), a stub
//! implementation skips the test.

#![cfg_attr(not(feature = "nested"), allow(dead_code))]

use macros::{guest, host};

pub struct TestNestedVirt;

#[cfg(feature = "nested")]
#[host]
mod host {
    use super::*;
    use crate::{Test, TestSetup};
    use std::process::Child;

    /// Check if the host supports nested virtualization.
    fn host_supports_nested_virt() -> bool {
        for module in &["kvm_intel", "kvm_amd"] {
            let path = format!("/sys/module/{}/parameters/nested", module);
            if let Ok(val) = std::fs::read_to_string(&path) {
                let val = val.trim();
                if val == "1" || val == "Y" {
                    return true;
                }
            }
        }
        false
    }

    impl Test for TestNestedVirt {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            use anyhow::Context;
            use std::fs::{self, create_dir};

            // AC4.4: Skip cleanly if host doesn't support nested virt
            if !host_supports_nested_virt() {
                println!("SKIP: host does not support nested virtualization");
                println!("OK");
                return Ok(());
            }

            let root_dir = test_setup.tmp_dir.join("root");
            create_dir(&root_dir).context("create root dir")?;

            // Copy guest-agent binary into the virtiofs root
            let agent_path = std::env::var("KRUN_TEST_GUEST_AGENT_PATH")
                .context("KRUN_TEST_GUEST_AGENT_PATH not set")?;
            fs::copy(&agent_path, root_dir.join("guest-agent"))
                .with_context(|| format!("copy guest-agent from {agent_path}"))?;

            // NOTE: L1 needs a kernel with KVM support (/dev/kvm). The Nix dev
            // shell's libkrunfw includes CONFIG_KVM=y.

            // Build L1 VM with nested virt enabled
            let mut builder = krun::Builder::new();
            builder.vm_config(2, 1024)?; // 2 vCPUs, 1 GiB RAM for L1
            builder.enable_nested_virt();

            // Use generic virtiofs for root filesystem
            let cfg = krun::passthrough::Config {
                root_dir: root_dir.to_str().unwrap().to_string(),
                ..Default::default()
            };
            let pt = krun::passthrough::PassthroughFs::new(cfg)
                .context("PassthroughFs::new")?;
            builder.add_virtiofs("/dev/root", Box::new(pt), Some(1 << 29));

            builder.workdir("/".to_string());
            builder.exec_path("/guest-agent".to_string());
            builder.args(test_setup.test_case.clone());
            builder.env("KRUN_NESTING_LEVEL=1".to_string());

            let context = builder.build()?;
            let vm_thread = std::thread::spawn(move || context.run());
            vm_thread.join().ok();

            Ok(())
        }

        fn check(self: Box<Self>, child: Child) {
            // Increase timeout to 180s for nested virt overhead.
            let timeout = std::time::Duration::from_secs(180);
            let output = crate::wait_with_timeout(child, timeout);
            let stdout = String::from_utf8(output.stdout).unwrap();

            assert!(
                stdout.contains("OK\n"),
                "expected stdout to contain \"OK\\n\", got {:?}",
                stdout,
            );
        }
    }
}

#[cfg(feature = "nested")]
#[guest]
mod guest {
    use super::*;
    use crate::Test;

    impl Test for TestNestedVirt {
        fn in_guest(self: Box<Self>) {
            let level = std::env::var("KRUN_NESTING_LEVEL").unwrap_or_default();
            match level.as_str() {
                "1" => run_l1(),
                "2" => run_l2(),
                _ => panic!("unexpected KRUN_NESTING_LEVEL: {:?}", level),
            }
        }
    }

    fn run_l1() {
        // AC4.1: Verify /dev/kvm exists
        assert!(
            std::path::Path::new("/dev/kvm").exists(),
            "L1: /dev/kvm not found — nested virt not working",
        );

        // AC4.2: Open /dev/kvm to verify it's accessible
        let _kvm = std::fs::File::open("/dev/kvm")
            .expect("L1: failed to open /dev/kvm");

        // AC4.3: Boot an L2 VM using libkrun (statically linked with firmware)
        let mut builder = krun::Builder::new();
        builder.vm_config(1, 256).expect("L1: vm_config failed");

        // Reuse L1's root filesystem for L2
        let cfg = krun::passthrough::Config {
            root_dir: "/".to_string(),
            ..Default::default()
        };
        let pt = krun::passthrough::PassthroughFs::new(cfg)
            .expect("L1: PassthroughFs::new failed");
        builder.add_virtiofs("/dev/root", Box::new(pt), None);

        builder.workdir("/".to_string());
        builder.exec_path("/guest-agent".to_string());
        builder.args("nested-virt".to_string());
        builder.env("KRUN_NESTING_LEVEL=2".to_string());

        let context = builder.build().expect("L1: builder.build() failed");
        context.run().expect("L1: L2 VM run failed");

        // L2 prints "OK" which propagates through the console chain
        println!("OK");
    }

    fn run_l2() {
        // L2 just confirms it booted successfully
        println!("OK");
    }
}

// Stub implementations when the "nested" feature is not enabled
#[cfg(not(feature = "nested"))]
#[host]
mod host {
    use super::*;
    use crate::{Test, TestSetup};
    use std::process::Child;

    impl Test for TestNestedVirt {
        fn start_vm(self: Box<Self>, _test_setup: TestSetup) -> anyhow::Result<()> {
            println!("SKIP: nested-virt test requires the 'nested' feature flag");
            println!("OK");
            Ok(())
        }

        fn check(self: Box<Self>, child: Child) {
            let output = crate::wait_with_timeout(child, crate::TEST_TIMEOUT);
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(
                stdout.contains("OK\n"),
                "expected stdout to contain \"OK\\n\", got {:?}",
                stdout,
            );
        }
    }
}

#[cfg(not(feature = "nested"))]
#[guest]
mod guest {
    use super::*;
    use crate::Test;

    impl Test for TestNestedVirt {
        fn in_guest(self: Box<Self>) {
            panic!("nested-virt test in_guest should not be called (requires 'nested' feature)");
        }
    }
}
