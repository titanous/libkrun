//! Privileged terminal-byte channel acceptance using shared public outcomes.
#[allow(dead_code)]
mod terminal_result;
use krun::{Builder, NitroBackendConfig};
#[derive(Clone, Default)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn monotonic_micros() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) },
        0
    );
    time.tv_sec as u64 * 1_000_000 + time.tv_nsec as u64 / 1000
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        4,
        "usage: nitro_terminal Image INITRAMFS REASON"
    );
    let mut builder = Builder::new();
    builder.nitro_backend(NitroBackendConfig {
        image: "unused".into(),
        cid: 216,
        watchdog: std::time::Duration::from_secs(5),
    });
    builder.disable_implicit_console()?;
    builder.vm_config(1, 1024)?;
    builder.external_kernel(
        args[1].clone().into(),
        krun::KernelFormat::Raw,
        Some(args[2].clone().into()),
    )?;
    let mut cmdline = vec!["console=hvc0", "quiet", "panic=-1", "rdinit=/init"];
    if std::env::var_os("NITRO_LONG_PANIC").is_some() {
        cmdline.push("ignore_loglevel");
        cmdline.push("printk.devkmsg=on");
    }
    builder.set_kernel_cmdline(cmdline);
    builder.nitro_stop_reason(true);
    let capture = Capture::default();
    builder.nitro_console_sink(Box::new(capture.clone()), 1024 * 1024, 1024 * 1024);
    use std::os::unix::fs::DirBuilderExt;
    let heartbeat_directory =
        std::env::current_dir()?.join(format!("terminal-private-{}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&heartbeat_directory)?;
    builder.nitro_heartbeat(
        7998,
        heartbeat_directory.join("heartbeat.sock"),
        std::time::Duration::from_secs(3),
    );
    use std::io::Write;
    let path = std::env::current_dir()?.join(format!("terminal-go-{}.sock", std::process::id()));
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    builder.add_vsock_port(7997, path.clone(), false);
    let go = std::sync::Arc::new(std::sync::Mutex::new(None));
    let peer_go = go.clone();
    let peer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    *peer_go.lock().unwrap() = Some(monotonic_micros());
                    stream.write_all(b"G").unwrap();
                    std::thread::park_timeout(std::time::Duration::from_millis(100));
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "terminal handshake timeout"
                    );
                    std::thread::park_timeout(std::time::Duration::from_millis(1));
                }
                Err(error) => panic!("terminal peer: {error}"),
            }
        }
    });
    let context = builder.build()?;
    let observation = context
        .stop_observation()
        .expect("Nitro observation channel");
    let start = std::time::Instant::now();
    let exit = context.run()?;
    if std::env::var_os("NITRO_LONG_PANIC").is_some() {
        let bytes = capture.0.lock().unwrap();
        std::fs::write("review-t2-console.log", &*bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        for line in 0..1024 {
            assert!(
                text.contains(&format!(
                    "PANIC_BURST_{line:04} retained kernel diagnostic line"
                )),
                "missing panic line {line}"
            );
        }
        assert!(
            text.contains("Kernel panic - not syncing"),
            "missing panic diagnostic"
        );
        println!("PANIC_LINES_OK lines=1024 bytes={}", bytes.len());
    }
    let go_at = go.lock().unwrap().expect("guest handshake occurred");
    let observed = observation.lock().unwrap().expect("observation timestamp");
    let detection_ms = observed.checked_sub(go_at).expect("ordered host clocks") as f64 / 1000.;
    peer.join().unwrap();
    std::fs::remove_file(path)?;
    std::fs::remove_dir(heartbeat_directory)?;
    assert!(
        std::env::var_os("NITRO_LONG_PANIC").is_some() || detection_ms < 100.0,
        "terminal reason detection too slow: {detection_ms}ms"
    );
    assert!(
        terminal_result::terminal_matches(&exit, args[3].parse()?),
        "{exit:?}"
    );
    println!(
        "TERMINAL_OK reason={} seconds={:.6} detection_upper_ms={detection_ms:.3} exit={exit:?}",
        args[3],
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
