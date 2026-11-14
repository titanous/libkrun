#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use krun::{Builder, NitroBackendConfig, NitroSerialization};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};
    let image = std::env::args().nth(1).ok_or("expected ordinary EIF")?;
    let socket = std::env::current_dir()?.join(format!("ordinary-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let mut builder = Builder::new();
    builder.nitro_backend(NitroBackendConfig {
        image: image.into(),
        cid: 750,
        watchdog: Duration::from_secs(15),
    });
    builder.nitro_lifecycle_lock(
        nitro_sandbox::lifecycle::LIFECYCLE_LOCK_PATH.into(),
        NitroSerialization::StartAndTeardown,
    );
    let memory = std::env::var("COPY_MEMORY_MIB")
        .unwrap_or_else(|_| "1024".into())
        .parse()?;
    builder.vm_config(1, memory)?;
    builder.disable_implicit_console()?;
    builder.add_vsock_port(7000, socket, false);
    builder.nitro_console_sink(
        Box::new(std::fs::File::create("ordinary-console.log")?),
        1024 * 1024,
        1024 * 1024,
    );
    builder.future_template();
    let context = builder.build()?;
    let handle = context.vm_handle();
    let worker = std::thread::spawn(move || -> std::io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::yield_now()
                }
                Err(e) => return Err(e),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let error = handle
            .arm_template_capture(Duration::from_secs(2))
            .expect_err("ordinary marker must be refused");
        if !error
            .get_ref()
            .is_some_and(|e| e.is::<vmm::cooperative::TemplateMarkerMissing>())
        {
            return Err(std::io::Error::other(format!(
                "unexpected refusal: {error}"
            )));
        }
        for expected in [b'A', b'B'] {
            let mut byte = [0];
            stream.read_exact(&mut byte)?;
            if byte[0] != expected {
                return Err(std::io::Error::other("ordinary vsock mismatch"));
            }
            stream.write_all(&byte)?;
        }
        println!("ORDINARY_TYPED_MARKER_REFUSAL vsock_alive=1");
        Ok(())
    });
    context.run()?;
    worker.join().map_err(|_| "ordinary worker panic")??;
    let console = std::fs::read_to_string("ordinary-console.log")?;
    let overlap = std::env::var_os("RESERVATION_OVERLAP").is_some();
    let expected = if overlap {
        "OVERLAP TRIGGER PASS refused vsock-alive"
    } else {
        "ORDINARY TRIGGER PASS absent vsock-alive"
    };
    if !console.contains(expected) {
        return Err("guest trigger evidence missing".into());
    }
    if console.contains("NITRO_RECEIVER_RANGE reserved") {
        return Err("refused or ordinary guest advertised a receiver reservation".into());
    }
    if overlap && !console.contains("receiver range overlaps reserved boot memory") {
        return Err("overlapping boot reservation was not diagnosed".into());
    }
    println!("ORDINARY_CAPTURE_NEGATIVE_PASS trigger_absent=1 typed_error=1");
    Ok(())
}
#[cfg(not(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
)))]
fn main() {
    eprintln!("requires arm64 Linux Nitro feature");
}
