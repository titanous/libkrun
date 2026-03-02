use macros::{guest, host};

pub struct TestVirtiofsGenericPassthrough;

#[host]
mod host_impl {
    use super::*;
    use crate::{Test, TestSetup};

    impl Test for TestVirtiofsGenericPassthrough {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            use anyhow::Context;
            use std::fs::{self, create_dir};

            let root_dir = test_setup.tmp_dir.join("root");
            create_dir(&root_dir).context("create root dir")?;

            // Copy guest-agent into root
            let agent_path = std::env::var_os("KRUN_TEST_GUEST_AGENT_PATH")
                .context("KRUN_TEST_GUEST_AGENT_PATH not set")?;
            fs::copy(&agent_path, root_dir.join("guest-agent"))
                .context("copy guest-agent")?;

            // Create a test data file that the guest will read
            fs::write(root_dir.join("test-data.txt"), b"hello from generic virtiofs")
                .context("write test-data.txt")?;

            // Construct PassthroughFs manually and pass via the generic API
            let cfg = krun::passthrough::Config {
                root_dir: root_dir.to_str().unwrap().to_string(),
                ..Default::default()
            };
            let pt = krun::passthrough::PassthroughFs::new(cfg)
                .context("PassthroughFs::new")?;

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 256)?;

            // This is the key call: generic add_virtiofs with Box<dyn FileSystem>
            builder.add_virtiofs(
                "/dev/root",
                Box::new(pt),
                Some(1 << 29), // 512 MiB DAX window
            );

            builder.workdir("/".to_string());
            builder.exec_path("/guest-agent".to_string());
            builder.args(test_setup.test_case.clone());

            let context = builder.build()?;
            let vm_thread = std::thread::spawn(move || context.run());
            vm_thread.join().ok();

            Ok(())
        }
    }
}

#[guest]
mod guest_impl {
    use super::*;
    use crate::Test;

    impl Test for TestVirtiofsGenericPassthrough {
        fn in_guest(self: Box<Self>) {
            use std::fs;

            // 1. Read the test file created by the host through the generic virtiofs path
            let data = fs::read_to_string("/test-data.txt").unwrap();
            assert_eq!(
                data, "hello from generic virtiofs",
                "read mismatch: got {:?}",
                data
            );

            // 2. Write a new file and read it back
            fs::write("/write-test.txt", b"generic virtiofs write test").unwrap();
            let data = fs::read_to_string("/write-test.txt").unwrap();
            assert_eq!(
                data, "generic virtiofs write test",
                "write readback mismatch: got {:?}",
                data
            );

            println!("OK");
        }
    }
}
