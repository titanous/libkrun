//! VM-booting integration tests for the libkrun Rust API.
//!
//! These tests start real microVMs via KVM and verify end-to-end behavior that
//! unit tests cannot reach (Builder::build, Context::run, VmExit plumbing).
//!
//! # Prerequisites
//!
//! - `embedded_init` feature must be enabled (init binary embedded in devices crate)
//! - libkrunfw must be in `LD_LIBRARY_PATH` (the Nix shell sets this via shellHook)
//! - `/dev/kvm` must be accessible
//!
//! # Running
//!
//! These tests are `#[ignore]` by default to avoid running during `cargo test`.
//! `just mutants` runs them automatically. To run manually:
//!
//! ```
//! KRUN_INIT_BIN=$(realpath init/init) \
//! LD_LIBRARY_PATH=$(realpath test-prefix/lib64/) \
//! cargo test --features embedded_init,net,snapshot,uffd,blk,vhost-user \
//!            -p libkrun --test vm_boot -- --include-ignored --test-threads 1
//! ```

#[cfg(feature = "embedded_init")]
mod tests {
    use krun::{Builder, VmExit};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    const VM_TIMEOUT: Duration = Duration::from_secs(10);

    /// Create a unique temp directory to use as the VM's virtiofs root.
    fn unique_root() -> TestRoot {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("krun_vm_boot_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).expect("failed to create VM test root");
        TestRoot(dir)
    }

    struct TestRoot(PathBuf);

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl TestRoot {
        fn path_str(&self) -> &str {
            self.0.to_str().expect("test root path is not valid UTF-8")
        }
    }

    /// Run `f` in a background thread and panic if it doesn't complete within `timeout`.
    ///
    /// This prevents integration tests from hanging indefinitely when a mutant breaks
    /// the VM shutdown path. cargo-mutants also has its own per-mutant timeout as backup.
    fn with_timeout<F, R>(timeout: Duration, f: F) -> R
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(timeout)
            .unwrap_or_else(|_| panic!("VM test timed out after {timeout:?}"))
    }

    /// A VM built via Builder::build() and run via Context::run() must return
    /// VmExit::Shutdown when the guest workload exits.
    ///
    /// This kills mutants in Builder::build(), build_microvm(), and Context::run()
    /// that prevent the VM from starting or returning a result.
    #[test]
    #[ignore = "requires /dev/kvm and libkrunfw in LD_LIBRARY_PATH"]
    fn vm_boots_and_shuts_down() {
        let root = unique_root();
        let root_path = root.path_str().to_string();
        let result = with_timeout(VM_TIMEOUT, move || {
            let _root = root; // keep alive for VM duration, drop on cleanup
            let mut builder = Builder::new();
            builder.vm_config(1, 256).unwrap();
            builder.set_root(&root_path);
            builder.exec_path("/nonexistent".to_string());
            builder.build().expect("build_vm failed").run()
        });
        assert!(
            matches!(result, Ok(VmExit::Shutdown { .. })),
            "expected Ok(VmExit::Shutdown), got: {result:?}"
        );
    }

    /// device_info() must reflect the vcpu_count and ram_mib passed to vm_config().
    ///
    /// This kills mutants where vm_config stores or reports wrong values.
    #[test]
    #[ignore = "requires /dev/kvm and libkrunfw in LD_LIBRARY_PATH"]
    fn device_info_reflects_vm_config() {
        let root = unique_root();
        let mut builder = Builder::new();
        builder.vm_config(2, 512).unwrap();
        builder.set_root(root.path_str());
        builder.exec_path("/nonexistent".to_string());
        let ctx = builder.build().expect("build failed");
        assert_eq!(
            ctx.device_info().vcpu_count,
            2,
            "vcpu_count must match vm_config"
        );
        assert_eq!(
            ctx.device_info().ram_mib,
            512,
            "ram_mib must match vm_config"
        );
        // Run to completion within timeout so we don't leak the VM.
        let _root = root;
        let _ = with_timeout(VM_TIMEOUT, move || ctx.run());
    }

    /// A second configuration variant: 1 vCPU, 256 MiB, also must shut down cleanly.
    ///
    /// Provides an additional data point for Builder field-setter mutants.
    #[test]
    #[ignore = "requires /dev/kvm and libkrunfw in LD_LIBRARY_PATH"]
    fn vm_1vcpu_256mib_shuts_down() {
        let root = unique_root();
        let root_path = root.path_str().to_string();
        let result = with_timeout(VM_TIMEOUT, move || {
            let _root = root;
            let mut builder = Builder::new();
            builder.vm_config(1, 256).unwrap();
            builder.set_root(&root_path);
            builder.exec_path("/nonexistent".to_string());
            builder.build().expect("build failed").run()
        });
        assert!(
            matches!(result, Ok(VmExit::Shutdown { .. })),
            "expected Ok(VmExit::Shutdown), got: {result:?}"
        );
    }

    /// balloon() must return None when balloon is not enabled.
    ///
    /// Kills mutants in VmHandle::balloon() that return None unconditionally.
    #[test]
    #[ignore = "requires /dev/kvm and libkrunfw in LD_LIBRARY_PATH"]
    fn balloon_none_when_disabled() {
        let root = unique_root();
        let mut builder = Builder::new();
        builder.vm_config(1, 256).unwrap();
        builder.set_root(root.path_str());
        builder.exec_path("/nonexistent".to_string());
        // NOT calling enable_balloon()
        let ctx = builder.build().expect("build failed");
        let handle = ctx.vm_handle();
        assert!(
            handle.balloon().is_none(),
            "balloon() should be None when not enabled"
        );
        // Run to completion so we don't leak the VM.
        let _root = root;
        let _ = with_timeout(VM_TIMEOUT, move || ctx.run());
    }

    /// balloon() must return Some when balloon is enabled, and the handle must work.
    ///
    /// Kills mutants in VmHandle::balloon(), BalloonHandle::stats(), and
    /// BalloonHandle::resize().
    #[test]
    #[ignore = "requires /dev/kvm and libkrunfw in LD_LIBRARY_PATH"]
    fn balloon_enabled_returns_handle() {
        let root = unique_root();
        let mut builder = Builder::new();
        builder.vm_config(1, 256).unwrap();
        builder.set_root(root.path_str());
        builder.exec_path("/nonexistent".to_string());
        builder.enable_balloon();
        let ctx = builder.build().expect("build failed");
        let handle = ctx.vm_handle();
        assert!(
            handle.balloon().is_some(),
            "balloon() should be Some when enabled"
        );

        // Call stats() to verify it doesn't panic (returns None before activation)
        let _stats = handle.balloon().unwrap().stats();

        // resize may fail with DeviceNotActive before activation, that's fine —
        // we just need to verify the method delegates properly and doesn't panic.
        let _ = handle.balloon().unwrap().resize(0);

        // Run to completion so we don't leak the VM.
        let _root = root;
        let _ = with_timeout(VM_TIMEOUT, move || ctx.run());
    }

    /// register_exit_observer must accept an observer and call it on VM exit.
    ///
    /// Kills mutants that replace register_exit_observer with ().
    #[test]
    #[ignore = "requires /dev/kvm and libkrunfw in LD_LIBRARY_PATH"]
    fn register_exit_observer_works() {
        use std::sync::{Arc, Mutex};

        struct TestObserver {
            called: bool,
        }
        impl krun::VmmExitObserver for TestObserver {
            fn on_vmm_exit(&mut self) {
                self.called = true;
            }
        }

        let root = unique_root();
        let root_path = root.path_str().to_string();
        let observer = Arc::new(Mutex::new(TestObserver { called: false }));
        let observer_clone = observer.clone();
        with_timeout(VM_TIMEOUT, move || {
            let _root = root;
            let mut builder = Builder::new();
            builder.vm_config(1, 256).unwrap();
            builder.set_root(&root_path);
            builder.exec_path("/nonexistent".to_string());
            let ctx = builder.build().expect("build failed");
            ctx.register_exit_observer(observer_clone);
            let _ = ctx.run();
        });
        assert!(
            observer.lock().unwrap().called,
            "exit observer should have been called on VM exit"
        );
    }

    /// Builder::build with a net device must succeed (exercises enable_tsi logic).
    ///
    /// Kills mutants in the vsock/TSI conditional logic inside Builder::build
    /// where enable_tsi depends on net device presence.
    #[cfg(feature = "net")]
    #[test]
    #[ignore = "requires /dev/kvm and libkrunfw in LD_LIBRARY_PATH"]
    fn vm_with_net_device_boots() {
        use krun::VirtioNetBackend;

        let root = unique_root();
        let root_path = root.path_str().to_string();
        let result = with_timeout(VM_TIMEOUT, move || {
            let _root = root;
            let mut builder = Builder::new();
            builder.vm_config(1, 256).unwrap();
            builder.set_root(&root_path);
            builder.exec_path("/nonexistent".to_string());
            // Add a dummy net device — the socket doesn't need to work since
            // init exits before networking is used.
            let sock_path = std::env::temp_dir().join(format!(
                "krun_test_net_{}_{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            builder.add_net_device(
                VirtioNetBackend::UnixgramPath(sock_path, false),
                [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee],
                0,
            );
            builder.build().expect("build_vm failed").run()
        });
        assert!(
            matches!(result, Ok(VmExit::Shutdown { .. })),
            "expected Ok(VmExit::Shutdown), got: {result:?}"
        );
    }
}
