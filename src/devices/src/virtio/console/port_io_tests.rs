use super::*;
use libc::{fcntl, F_GETFL};
use std::io::{Read, Write};

fn emit(fd: RawFd) {
    let mut output = output_to_raw_fd_dup_blocking(fd).unwrap();
    let mut bytes = *b"console";
    let slice = unsafe { VolatileSlice::new(bytes.as_mut_ptr(), bytes.len()) };
    assert_eq!(output.write_volatile(&slice).unwrap(), bytes.len());
}

#[test]
fn two_writers_on_one_pipe_have_private_nonblocking_descriptions() {
    let (mut reader, writer) = std::io::pipe().unwrap();
    let original = unsafe { fcntl(writer.as_raw_fd(), F_GETFL) };
    let first = prepare_output_fd(writer.as_raw_fd()).unwrap();
    let second = prepare_output_fd(writer.as_raw_fd()).unwrap();
    assert_ne!(unsafe { fcntl(first.as_raw_fd(), F_GETFL) } & O_NONBLOCK, 0);
    assert_ne!(
        unsafe { fcntl(second.as_raw_fd(), F_GETFL) } & O_NONBLOCK,
        0
    );
    assert_eq!(unsafe { fcntl(writer.as_raw_fd(), F_GETFL) }, original);
    let threads: Vec<_> = [first, second]
        .into_iter()
        .map(|fd| {
            std::thread::spawn(move || {
                let mut output = PortOutputFd(fd);
                let mut bytes = [b'x'; 128];
                let slice = unsafe { VolatileSlice::new(bytes.as_mut_ptr(), bytes.len()) };
                for _ in 0..8 {
                    assert_eq!(output.write_volatile(&slice).unwrap(), 128);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let mut bytes = [0; 2048];
    reader.read_exact(&mut bytes).unwrap();
    assert!(bytes.iter().all(|byte| *byte == b'x'));
    assert_eq!(unsafe { fcntl(writer.as_raw_fd(), F_GETFL) }, original);
}

#[test]
fn input_does_not_change_shared_description_flags() {
    let (reader, _writer) = std::io::pipe().unwrap();
    let flags = unsafe { fcntl(reader.as_raw_fd(), F_GETFL) };
    let _input = input_to_raw_fd_dup(reader.as_raw_fd()).unwrap();
    assert_eq!(unsafe { fcntl(reader.as_raw_fd(), F_GETFL) }, flags);
}

#[test]
fn output_descriptor_socket() {
    let (writer, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
    let flags = unsafe { fcntl(writer.as_raw_fd(), F_GETFL) };
    emit(writer.as_raw_fd());
    let mut bytes = [0; 7];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"console");
    assert_eq!(unsafe { fcntl(writer.as_raw_fd(), F_GETFL) }, flags);
}

#[test]
fn closed_pipe_returns_error_with_default_sigpipe() {
    const CHILD: &str = "LIBKRUN_TEST_DEFAULT_SIGPIPE";
    if std::env::var_os(CHILD).is_some() {
        let (reader, writer) = std::io::pipe().unwrap();
        let mut output = output_to_raw_fd_dup_blocking(writer.as_raw_fd()).unwrap();
        drop(reader);
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let mut bytes = *b"console";
        let slice = unsafe { VolatileSlice::new(bytes.as_mut_ptr(), bytes.len()) };
        assert_eq!(
            output.write_volatile(&slice).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        unsafe {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask),
                0
            );
            assert_eq!(libc::sigismember(&mask, libc::SIGPIPE), 0);
            let mut disposition: libc::sigaction = std::mem::zeroed();
            assert_eq!(
                libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut disposition),
                0
            );
            assert_eq!(disposition.sa_sigaction, libc::SIG_DFL);
            let mut blocked: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGPIPE);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut()),
                0
            );
            assert_eq!(libc::raise(libc::SIGPIPE), 0);
            assert_eq!(
                output.write_volatile(&slice).unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
            let mut pending: libc::sigset_t = std::mem::zeroed();
            assert_eq!(libc::sigpending(&mut pending), 0);
            assert_eq!(libc::sigismember(&pending, libc::SIGPIPE), 1);
            let mut signal = 0;
            assert_eq!(libc::sigwait(&blocked, &mut signal), 0);
            assert_eq!(signal, libc::SIGPIPE);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()),
                0
            );
        }
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "virtio::console::port_io::descriptor_tests::closed_pipe_returns_error_with_default_sigpipe"])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(
        status.success(),
        "closed pipe killed or failed child: {status}"
    );
}

#[test]
fn output_descriptor_pipe() {
    let (mut reader, writer) = std::io::pipe().unwrap();
    let flags = unsafe { fcntl(writer.as_raw_fd(), F_GETFL) };
    emit(writer.as_raw_fd());
    let mut bytes = [0; 7];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"console");
    assert_eq!(unsafe { fcntl(writer.as_raw_fd(), F_GETFL) }, flags);
}

/// A pseudo-terminal master (the embedder holds the terminal's other end) must
/// receive the console output on ITS terminal: reopening the master by path
/// would allocate a new, unconnected terminal and lose every byte.
#[test]
fn output_descriptor_pty_master_reaches_its_own_terminal() {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0);
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(master)).to_owned();
        let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | O_NONBLOCK);
        assert!(slave >= 0);
        let mut termios: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(slave, &mut termios), 0);
        libc::cfmakeraw(&mut termios);
        assert_eq!(libc::tcsetattr(slave, libc::TCSANOW, &termios), 0);
        let flags = fcntl(master, F_GETFL);
        emit(master);
        let mut received = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while received.len() < 7 && std::time::Instant::now() < deadline {
            let mut poll = libc::pollfd {
                fd: slave,
                events: libc::POLLIN,
                revents: 0,
            };
            libc::poll(&mut poll, 1, 100);
            let mut bytes = [0u8; 16];
            let n = libc::read(slave, bytes.as_mut_ptr().cast(), bytes.len());
            if n > 0 {
                received.extend_from_slice(&bytes[..n as usize]);
            }
        }
        assert_eq!(received, b"console");
        assert_eq!(fcntl(master, F_GETFL), flags);
        libc::close(slave);
        libc::close(master);
    }
}

#[test]
fn output_descriptor_preserves_append_and_existing_content() {
    let path = std::env::temp_dir().join(format!("console-append-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.write_all(b"existing-").unwrap();
    drop(file);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    emit(file.as_raw_fd());
    file.write_all(b"-after").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"existing-console-after");
    std::fs::remove_file(path).unwrap();
}
