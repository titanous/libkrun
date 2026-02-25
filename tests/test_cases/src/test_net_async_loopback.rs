//! Integration test for async net loopback backend.
//!
//! Tests:
//! - AC3.1: Uses CustomAsyncFactory with LoopbackFactory
//! - AC3.2: Guest configures eth0 with 192.168.100.2/24 and pings 192.168.100.1
//! - AC3.3: Guest receives ICMP echo reply within timeout

use macros::{guest, host};

pub struct TestNetAsyncLoopback;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::loopback_net::LoopbackFactory;
    use crate::{Test, TestSetup};
    use std::thread;

    impl Test for TestNetAsyncLoopback {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;

            // AC3.1: Use CustomAsyncFactory with LoopbackFactory
            builder.add_net_device(
                krun::VirtioNetBackend::CustomAsyncFactory(Box::new(LoopbackFactory::new())),
                [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee], // Guest MAC
                0,                                    // features
            );

            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());
            vm_thread.join().ok();
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::net_helpers::{configure_eth0, test_ping};
    use crate::Test;

    impl Test for TestNetAsyncLoopback {
        fn in_guest(self: Box<Self>) {
            configure_eth0();

            // AC3.2: Test ICMP echo connectivity with loopback backend
            test_ping();

            println!("OK");
        }
    }
}
