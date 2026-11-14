//! Published-agent boot timing with a guest-originated first protocol message.
use krun::{Builder, NitroBackendConfig};
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        4,
        "usage: nitro_agent_timing IMAGE INITRAMFS CACHE_DIRECTORY"
    );
    let socket = std::env::current_dir()?.join(format!("agent-timing-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let start = Instant::now();
    let mut builder = Builder::new();
    let _signal = builder.install_nitro_sigterm_handler()?;
    builder.nitro_backend(NitroBackendConfig {
        image: "unused".into(),
        cid: 225,
        watchdog: Duration::from_secs(10),
    });
    builder.disable_implicit_console()?;
    builder.vm_config(1, 1024)?;
    builder.external_kernel(
        args[1].clone().into(),
        krun::KernelFormat::Raw,
        Some(args[2].clone().into()),
    )?;
    builder.set_kernel_cmdline(vec!["console=hvc0", "quiet", "panic=-1", "rdinit=/init"]);
    builder.nitro_image_cache(args[3].clone().into());
    builder.add_vsock_port(7000, socket.clone(), false);
    let context = builder.build()?;
    let built = start.elapsed();
    let handle = context.vm_handle();
    let peer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut byte = [0u8; 1];
                    stream.read_exact(&mut byte).unwrap();
                    let first = start.elapsed();
                    handle.trigger_shutdown_event().unwrap();
                    return first;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "agent message timeout");
                    std::thread::park_timeout(Duration::from_millis(1));
                }
                Err(error) => panic!("agent listener: {error}"),
            }
        }
    });
    let exit = context.run()?;
    let first = peer.join().unwrap();
    std::fs::remove_file(socket)?;
    println!("AGENT_TIMING build_ms={:.3} first_message_ms={:.3} guest_after_build_ms={:.3} total_ms={:.3} exit={exit:?}",built.as_secs_f64()*1000.,first.as_secs_f64()*1000.,(first-built).as_secs_f64()*1000.,start.elapsed().as_secs_f64()*1000.);
    Ok(())
}
