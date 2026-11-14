//! Descriptor-transfer fixture for a daemon supervising one Nitro guest.
//!
//! The duplicate VM descriptor is sent from inside `build`, right after the create ioctl
//! (`Builder::nitro_descriptor_handoff`); the supervisor acknowledges with `G` before `run`.
use krun::{Builder, NitroBackendConfig, NitroSerialization};
use std::io::Read;
use std::os::unix::net::UnixStream;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        5,
        "usage: nitro_supervised IMAGE LOCK SOCKET CID"
    );
    let mut builder = Builder::new();
    let _signal = builder.install_nitro_sigterm_handler()?;
    builder.nitro_backend(NitroBackendConfig {
        image: args[1].clone().into(),
        cid: args[4].parse()?,
        watchdog: std::time::Duration::from_secs(10),
    });
    builder.disable_implicit_console()?;
    builder.vm_config(1, 1024)?;
    builder.nitro_lifecycle_lock(args[2].clone().into(), NitroSerialization::Teardown);
    let stream = UnixStream::connect(&args[3])?;
    builder.nitro_descriptor_handoff(stream.try_clone()?);
    let context = builder.build()?;
    let mut stream = stream;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut byte = [0u8];
    stream.read_exact(&mut byte)?;
    assert_eq!(byte, [b'G']);
    let exit = context.run()?;
    println!("SUPERVISED_EXIT {exit:?}");
    Ok(())
}
