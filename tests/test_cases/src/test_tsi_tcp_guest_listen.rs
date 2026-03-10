use crate::tcp_tester::TcpTester;
use macros::{guest, host};

const PORT: u16 = 8001;

pub struct TestTsiTcpGuestListen {
    tcp_tester: TcpTester,
}

impl TestTsiTcpGuestListen {
    pub fn new() -> Self {
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
    use std::collections::HashMap;
    use std::thread;
    use std::time::Duration;

    impl Test for TestTsiTcpGuestListen {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(1));
                self.tcp_tester.run_client();
            });

            let mut builder = krun::Builder::new();
            let mut port_mapping = HashMap::new();
            port_mapping.insert(PORT, PORT);
            builder
                .port_map(port_mapping)
                .map_err(|_| anyhow::anyhow!("port_map failed"))?;
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

    impl Test for TestTsiTcpGuestListen {
        fn in_guest(self: Box<Self>) {
            let listener = self.tcp_tester.create_server_socket();
            self.tcp_tester.run_server(listener);
            println!("OK");
        }
    }
}
