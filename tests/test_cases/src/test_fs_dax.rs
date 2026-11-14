use macros::{guest, host};

pub struct TestFsDax(pub &'static str);

#[host]
impl crate::Test for TestFsDax {
    fn start_vm(self: Box<Self>, setup: crate::TestSetup) -> anyhow::Result<()> {
        use std::process::{Child, Command};
        use std::time::{Duration, Instant};
        struct Daemon(Child);
        impl Drop for Daemon {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut builder = krun::Builder::new();
        builder.vm_config(1, 512)?;
        crate::krun_rust::setup_fs_builder(&mut builder, &setup)?;
        let _daemon;
        if self.0 == "in-process" {
            let root = setup.tmp_dir.join("dax-root");
            std::fs::create_dir(&root)?;
            std::fs::write(root.join("hello.txt"), vec![0xBB; 8192])?;
            let fs = krun::passthrough::PassthroughFs::new(krun::passthrough::Config {
                root_dir: root.to_str().unwrap().to_owned(),
                ..Default::default()
            })?;
            builder.add_virtiofs("testfs", Box::new(fs), Some(32 << 20));
        } else {
            let socket = setup.tmp_dir.join("dax.sock");
            _daemon = Daemon(
                Command::new(std::env::var("KRUN_TEST_DAEMON_PATH")?)
                    .arg("--socket-path")
                    .arg(&socket)
                    .spawn()?,
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while !socket.exists() {
                anyhow::ensure!(Instant::now() < deadline, "test daemon socket timeout");
                std::thread::sleep(Duration::from_millis(10));
            }
            builder.add_virtiofs_vhost_user("testfs", socket.to_str().unwrap(), Some(32))?;
        }
        builder.build()?.run()?;
        Ok(())
    }
}

#[guest]
impl crate::Test for TestFsDax {
    fn in_guest(self: Box<Self>) {
        use std::ffi::CString;
        use std::io::Write;
        std::fs::create_dir_all("/mnt/testfs").unwrap();
        let source = CString::new("testfs").unwrap();
        let target = CString::new("/mnt/testfs").unwrap();
        let kind = CString::new("virtiofs").unwrap();
        let mode = if self.0 == "in-process" {
            "always"
        } else {
            self.0
        };
        let options = CString::new(format!("dax={mode}")).unwrap();
        // SAFETY: all mount arguments point to live NUL-terminated strings.
        assert_eq!(
            unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    kind.as_ptr(),
                    0,
                    options.as_ptr().cast(),
                )
            },
            0,
            "mount: {}",
            std::io::Error::last_os_error()
        );
        let path = "/mnt/testfs/hello.txt";
        let data = std::fs::read(path).unwrap();
        assert!(!data.is_empty());
        let expected = if mode == "never" { 0xAA } else { 0xBB };
        assert!(
            data.iter().all(|&b| b == expected),
            "DAX mode {mode}: wrong read path"
        );
        if mode == "inode" {
            let data = std::fs::read("/mnt/testfs/nodax.txt").unwrap();
            assert!(!data.is_empty());
            assert!(data.iter().all(|&b| b == 0xAA));
        }
        if mode != "never" {
            let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            file.write_all(&vec![0xCC; 4096]).unwrap();
            file.flush().unwrap();
            let data = std::fs::read(path).unwrap();
            assert!(data[..4096].iter().all(|&b| b == 0xCC));
        }
        println!("OK");
    }
}
