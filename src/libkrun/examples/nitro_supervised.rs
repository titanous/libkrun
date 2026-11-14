//! Descriptor-transfer fixture for a daemon supervising one Nitro guest.
use krun::{Builder, NitroBackendConfig, NitroSerialization};
use std::io::Read;
use std::os::fd::AsRawFd;
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
    let context = builder.build()?;
    let descriptor = context.duplicate_nitro_vm_descriptor()?;
    let mut stream = UnixStream::connect(&args[3])?;
    let mut byte = [b'F'];
    let mut vector = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // usize storage provides cmsghdr alignment.
    let mut control = [0usize; 8];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as _) } as usize;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as _) as usize;
        std::ptr::write(
            libc::CMSG_DATA(header).cast::<i32>(),
            descriptor.as_raw_fd(),
        );
        if libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) != 1 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    drop(descriptor);
    stream.read_exact(&mut byte)?;
    assert_eq!(byte, [b'G']);
    let exit = context.run()?;
    println!("SUPERVISED_EXIT {exit:?}");
    Ok(())
}
