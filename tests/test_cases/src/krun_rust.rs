//! Host-side helpers for integration tests using the Rust libkrun API.
//!
//! Mirrors `common.rs` which serves C-API tests. Use these helpers to set up
//! a `krun::Builder` with the standard test root filesystem and guest-agent.

use anyhow::Context as AnyhowContext;
use std::fs;
use std::fs::create_dir;
use std::path::Path;

use crate::TestSetup;

fn copy_guest_agent(dir: &Path) -> anyhow::Result<()> {
    let path = std::env::var_os("KRUN_TEST_GUEST_AGENT_PATH")
        .context("KRUN_TEST_GUEST_AGENT_PATH env variable not set")?;
    let output_path = dir.join("guest-agent");
    fs::copy(path, output_path).context("Failed to copy executable into vm")?;
    Ok(())
}

/// Configure `builder` with:
/// - a virtiofs root at `test_setup.tmp_dir/root` containing the guest-agent binary
/// - workdir = "/"
/// - exec_path = "/guest-agent" with the test case name as the argument
///
/// Call this before `builder.build()`. The returned `Context` can then be
/// run with `context.run()` or used with `context.vm_handle()` first.
pub fn setup_fs_builder(builder: &mut krun::Builder, test_setup: &TestSetup) -> anyhow::Result<()> {
    let root_dir = test_setup.tmp_dir.join("root");
    create_dir(&root_dir).context("Failed to create root directory")?;
    copy_guest_agent(&root_dir)?;

    builder.set_root(
        root_dir
            .to_str()
            .context("root_dir path is not valid UTF-8")?,
    );
    builder.workdir("/".to_string());
    builder.exec_path("/guest-agent".to_string());
    builder.args(test_setup.test_case.clone());

    Ok(())
}
