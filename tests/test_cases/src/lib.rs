mod test_vm_config;
use test_vm_config::TestVmConfig;

mod test_vsock_guest_connect;
use test_vsock_guest_connect::TestVsockGuestConnect;

mod test_tsi_tcp_guest_connect;
use test_tsi_tcp_guest_connect::TestTsiTcpGuestConnect;

mod test_tsi_tcp_guest_listen;
use test_tsi_tcp_guest_listen::TestTsiTcpGuestListen;

mod test_multiport_console;
use test_multiport_console::TestMultiportConsole;

mod test_snapshot_restore;
use test_snapshot_restore::{TestSnapshotRestore, TestSnapshotRestoreIncremental};

mod test_snapshot_serial;
use test_snapshot_serial::TestSnapshotSerial;

mod test_snapshot_block;
use test_snapshot_block::TestSnapshotBlock;

mod test_snapshot_incremental_state;
use test_snapshot_incremental_state::TestSnapshotIncrementalState;

mod test_snapshot_net;
use test_snapshot_net::TestSnapshotNet;

mod test_snapshot_errors;
use test_snapshot_errors::{
    TestSnapshotNestedMismatch, TestSnapshotVcpuMismatch, TestSnapshotWrongMagic,
};

mod test_rust_api;
use test_rust_api::{
    TestRustApiDeviceInfo, TestRustApiPauseResume, TestRustApiShutdown, TestRustApiZeroVcpu,
};

mod test_vm_exit;
use test_vm_exit::{TestVmExit, TestVmExitObserver};

#[cfg(feature = "host")]
mod mem_block_backend;

#[cfg(feature = "host")]
mod loopback_net;

#[cfg(feature = "host")]
mod mock_snapshot_store;

mod test_custom_block_backend;
use test_custom_block_backend::TestCustomBlockBackend;

mod test_net_async_loopback;
use test_net_async_loopback::TestNetAsyncLoopback;

mod test_vhost_user_fs;
use test_vhost_user_fs::{
    TestVhostUserFsDaxAlways, TestVhostUserFsDaxInode, TestVhostUserFsDaxNever,
};

mod test_vhost_user_vsock;
use test_vhost_user_vsock::{
    TestVhostUserVsockEcho, TestVhostUserVsockFd, TestVhostUserVsockSnapshot,
};

mod test_virtiofs_generic_passthrough;
use test_virtiofs_generic_passthrough::TestVirtiofsGenericPassthrough;

#[cfg(feature = "guest")]
mod net_helpers;
#[cfg(feature = "guest")]
mod vsock_helpers;

mod test_uffd_demand_page;
use test_uffd_demand_page::TestUffdDemandPageOnly;

mod test_uffd_preload;
use test_uffd_preload::{TestUffdPreloadFull, TestUffdPreloadPartial};

mod test_uffd_incremental;
use test_uffd_incremental::TestUffdIncrementalChain;

mod test_uffd_error;
use test_uffd_error::TestUffdErrorHandling;

mod test_uffd_parallel;
use test_uffd_parallel::TestUffdParallelFaults;

mod test_balloon_inflate;
use test_balloon_inflate::TestBalloonInflateDeflateStats;

mod test_balloon_snapshot;
use test_balloon_snapshot::{TestBalloonIncrementalReclaimed, TestBalloonSnapshotExcludes};

mod test_balloon_uffd;
use test_balloon_uffd::TestBalloonUffdZeroFill;

pub fn test_cases() -> Vec<TestCase> {
    // Register your test here:
    vec![
        TestCase::new(
            "configure-vm-1cpu-256MiB",
            Box::new(TestVmConfig {
                num_cpus: 1,
                ram_mib: 256,
            }),
        ),
        TestCase::new(
            "configure-vm-2cpu-1GiB",
            Box::new(TestVmConfig {
                num_cpus: 2,
                ram_mib: 1024,
            }),
        ),
        TestCase::new("vsock-guest-connect", Box::new(TestVsockGuestConnect)),
        TestCase::new(
            "tsi-tcp-guest-connect",
            Box::new(TestTsiTcpGuestConnect::new()),
        ),
        TestCase::new(
            "tsi-tcp-guest-listen",
            Box::new(TestTsiTcpGuestListen::new()),
        ),
        TestCase::new("multiport-console", Box::new(TestMultiportConsole)),
        TestCase::new("snapshot-restore-full", Box::new(TestSnapshotRestore)),
        TestCase::new(
            "snapshot-restore-incremental",
            Box::new(TestSnapshotRestoreIncremental),
        ),
        TestCase::new("snapshot-serial-scratch", Box::new(TestSnapshotSerial)),
        TestCase::new("snapshot-block-data", Box::new(TestSnapshotBlock)),
        TestCase::new(
            "snapshot-incremental-state",
            Box::new(TestSnapshotIncrementalState),
        ),
        TestCase::new("snapshot-net-connectivity", Box::new(TestSnapshotNet)),
        TestCase::new(
            "snapshot-error-wrong-magic",
            Box::new(TestSnapshotWrongMagic),
        ),
        TestCase::new(
            "snapshot-error-vcpu-mismatch",
            Box::new(TestSnapshotVcpuMismatch),
        ),
        TestCase::new(
            "snapshot-error-nested-mismatch",
            Box::new(TestSnapshotNestedMismatch),
        ),
        TestCase::new("rust-api-zero-vcpu", Box::new(TestRustApiZeroVcpu)),
        TestCase::new("rust-api-device-info", Box::new(TestRustApiDeviceInfo)),
        TestCase::new("rust-api-pause-resume", Box::new(TestRustApiPauseResume)),
        TestCase::new("rust-api-shutdown", Box::new(TestRustApiShutdown)),
        TestCase::new("vm-exit-clean-shutdown", Box::new(TestVmExit)),
        TestCase::new("vm-exit-observer", Box::new(TestVmExitObserver)),
        TestCase::new("custom-block-backend", Box::new(TestCustomBlockBackend)),
        TestCase::new("net-async-loopback", Box::new(TestNetAsyncLoopback)),
        TestCase::new(
            "vhost-user-fs-dax-always",
            Box::new(TestVhostUserFsDaxAlways),
        ),
        TestCase::new("vhost-user-fs-dax-inode", Box::new(TestVhostUserFsDaxInode)),
        TestCase::new("vhost-user-fs-dax-never", Box::new(TestVhostUserFsDaxNever)),
        TestCase::new("vhost-user-vsock-echo", Box::new(TestVhostUserVsockEcho)),
        TestCase::new("vhost-user-vsock-fd", Box::new(TestVhostUserVsockFd)),
        TestCase::new("vhost-user-vsock-snapshot", Box::new(TestVhostUserVsockSnapshot)),
        TestCase::new(
            "virtiofs-generic-passthrough",
            Box::new(TestVirtiofsGenericPassthrough),
        ),
        TestCase::new("uffd-demand-page-only", Box::new(TestUffdDemandPageOnly)),
        TestCase::new("uffd-preload-full", Box::new(TestUffdPreloadFull)),
        TestCase::new("uffd-preload-partial", Box::new(TestUffdPreloadPartial)),
        TestCase::new("uffd-incremental-chain", Box::new(TestUffdIncrementalChain)),
        TestCase::new("uffd-error-handling", Box::new(TestUffdErrorHandling)),
        TestCase::new("uffd-parallel-faults", Box::new(TestUffdParallelFaults)),
        TestCase::new(
            "balloon-inflate-deflate-stats",
            Box::new(TestBalloonInflateDeflateStats),
        ),
        TestCase::new(
            "balloon-snapshot-excludes-pages",
            Box::new(TestBalloonSnapshotExcludes),
        ),
        TestCase::new("balloon-uffd-zero-fill", Box::new(TestBalloonUffdZeroFill)),
        TestCase::new(
            "balloon-incremental-reclaimed",
            Box::new(TestBalloonIncrementalReclaimed),
        ),
    ]
}

////////////////////
// Implementation details:
//////////////////
use macros::{guest, host};
#[host]
use std::path::PathBuf;
#[host]
use std::process::Child;

#[cfg(all(feature = "guest", feature = "host"))]
compile_error!("Cannot enable both guest and host in the same binary!");

#[cfg(feature = "host")]
mod common;

#[cfg(feature = "host")]
mod krun;

#[cfg(feature = "host")]
mod krun_rust;
#[cfg(feature = "host")]
pub use krun_rust::*;
mod tcp_tester;

#[host]
#[derive(Clone, Debug)]
pub struct TestSetup {
    pub test_case: String,
    // A tmp directory for misc. artifacts used be the test (e.g. sockets)
    pub tmp_dir: PathBuf,
}

#[host]
pub trait Test {
    /// Start the VM
    fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()>;

    /// Checks the output of the (host) process which started the VM.
    ///
    /// Looks for "OK\n" anywhere in stdout. Kernel boot messages appear
    /// before the test output, and kernel shutdown/warning messages may
    /// appear after it — both are tolerated.
    fn check(self: Box<Self>, child: Child) {
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains("OK\n"),
            "expected stdout to contain \"OK\\n\", got {:?}",
            stdout,
        );
    }
}

#[guest]
pub trait Test {
    /// This will be executed in the guest, you can panic! if the test failed!
    fn in_guest(self: Box<Self>) {}
}

pub struct TestCase {
    pub name: &'static str,
    pub test: Box<dyn Test>,
}

impl TestCase {
    // Your test can be parametrized, so you can add the same test multiple times constructed with
    // different parameters with and specify a different name here.
    pub fn new(name: &'static str, test: Box<dyn Test>) -> Self {
        Self { name, test }
    }

    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_testcases_have_unique_names() {
        let test_cases = test_cases();
        let mut names: HashSet<&str> = HashSet::new();

        for test_case in test_cases {
            let name = test_case.name();
            let was_inserted = names.insert(name);
            if !was_inserted {
                panic!("test_cases() contains multiple items named `{name}`")
            }

            if name == "all" {
                panic!("test_cases() contains test named {name}, but the name is reseved")
            }
        }
    }
}
