use macros::{guest, host};

pub struct TestVmGenerationChange;
const PORT: u32 = 7815;

#[host]
mod host_side {
    use super::*;
    use crate::{setup_fs_builder, Test, TestSetup};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    impl Test for TestVmGenerationChange {
        fn start_vm(self: Box<Self>, setup: TestSetup) -> anyhow::Result<()> {
            let socket = setup.tmp_dir.join("generation.sock");
            let listener = UnixListener::bind(&socket)?;
            listener.set_nonblocking(true)?;
            let mut builder = krun::Builder::new();
            builder.vm_config(1, 256)?;
            setup_fs_builder(&mut builder, &setup)?;
            builder.add_vsock_port(PORT, socket, false);
            let context = builder.build()?;
            let handle = context.vm_handle();
            let vm = std::thread::spawn(move || context.run());
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        anyhow::ensure!(Instant::now() < deadline, "guest did not connect");
                        std::thread::yield_now();
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(15)))?;
            stream.set_write_timeout(Some(Duration::from_secs(15)))?;
            let mut previous = None;
            for _ in 0..2 {
                let mut ready = [0u8; 1];
                stream.read_exact(&mut ready)?;
                anyhow::ensure!(ready == [1], "guest not ready for generation notification");
                let id = handle.notify_vm_generation_change()?;
                anyhow::ensure!(previous != Some(id), "generation identifier did not change");
                previous = Some(id);
                stream.write_all(&[1])?;
            }
            let mut done = [0u8; 1];
            stream.read_exact(&mut done)?;
            anyhow::ensure!(done == [2], "guest did not observe both notifications");
            vm.join().expect("VM thread panicked")?;
            Ok(())
        }
    }
}

#[guest]
mod guest_side {
    use super::*;
    use crate::Test;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    fn reseeds() -> usize {
        // SYSLOG_ACTION_READ_ALL reads without consuming the kernel log.
        let mut log = vec![0u8; 1 << 20];
        let len = unsafe { libc::klogctl(3, log.as_mut_ptr().cast(), log.len() as i32) };
        assert!(len >= 0, "cannot read guest kernel log: {}", std::io::Error::last_os_error());
        String::from_utf8_lossy(&log[..len as usize])
            .matches("crng reseeded due to virtual machine fork")
            .count()
    }

    impl Test for TestVmGenerationChange {
        fn in_guest(self: Box<Self>) {
            // Ensure the CRNG is ready, so the driver performs and logs an immediate reseed.
            let mut random = std::fs::File::open("/dev/random").unwrap();
            random.read_exact(&mut [0u8; 32]).unwrap();
            let mut stream = crate::vsock_helpers::vsock_connect(PORT);
            for _ in 0..2 {
                let before = reseeds();
                stream.write_all(&[1]).unwrap();
                stream.read_exact(&mut [0u8; 1]).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while reseeds() <= before {
                    assert!(Instant::now() < deadline, "guest kernel did not handle generation change");
                    std::thread::yield_now();
                }
            }
            stream.write_all(&[2]).unwrap();
            println!("OK");
        }
    }
}
