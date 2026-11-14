//! Privileged terminal-byte channel acceptance; temporary raw-code diagnostics.
use krun::{Builder, NitroBackendConfig, VmExit};
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
    builder.set_kernel_cmdline(vec!["console=hvc0", "quiet", "panic=-1", "rdinit=/init"]);
    builder.nitro_stop_reason(true);
    builder.nitro_console_sink(Box::new(std::io::stdout()), 65536, 65536);
    builder.nitro_heartbeat(
        7998,
        std::env::current_dir()?.join("terminal-heartbeat.sock"),
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
    let start = std::time::Instant::now();
    let exit = context.run()?;
    let go_at = go.lock().unwrap().expect("guest handshake occurred");
    let observed: u64 = match &exit {
        VmExit::Error { message } => message
            .split("observed_micros=Some(")
            .nth(1)
            .and_then(|value| value.strip_suffix(')'))
            .expect("observation timestamp")
            .parse()?,
        _ => panic!("unexpected {exit:?}"),
    };
    let detection_ms = observed.checked_sub(go_at).expect("ordered host clocks") as f64 / 1000.;
    peer.join().unwrap();
    std::fs::remove_file(path)?;
    assert!(
        detection_ms < 100.0,
        "terminal reason detection too slow: {detection_ms}ms"
    );
    match &exit {
        VmExit::Error { message } => assert!(
            message.starts_with(&format!("Nitro stated terminal reason {} ", args[3])),
            "{exit:?}"
        ),
        _ => panic!("unexpected {exit:?}"),
    };
    println!(
        "TERMINAL_OK reason={} seconds={:.6} detection_upper_ms={detection_ms:.3} exit={exit:?}",
        args[3],
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
