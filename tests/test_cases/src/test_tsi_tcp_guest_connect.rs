use crate::tcp_tester::TcpTester;
use macros::{guest, host};

const PORT: u16 = 8000;

pub struct TestTsiTcpGuestConnect {
    tcp_tester: TcpTester,
}

impl TestTsiTcpGuestConnect {
    pub fn new() -> TestTsiTcpGuestConnect {
        Self {
            tcp_tester: TcpTester::new(PORT),
        }
    }
}

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::thread;

    impl Test for TestTsiTcpGuestConnect {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let listener = self.tcp_tester.create_server_socket();
            thread::spawn(move || self.tcp_tester.run_server(listener));

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            let context = builder.build()?;
            context.run()?;
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    impl Test for TestTsiTcpGuestConnect {
        fn in_guest(self: Box<Self>) {
            self.tcp_tester.run_client();
            println!("OK");
        }
    }
}
