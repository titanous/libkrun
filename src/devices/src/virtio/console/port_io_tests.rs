use super::*;
use std::io::{Read, Write};

fn emit(fd: RawFd) {
    let mut output = output_to_raw_fd_dup_blocking(fd).unwrap();
    let mut bytes = *b"console";
    let slice = unsafe { VolatileSlice::new(bytes.as_mut_ptr(), bytes.len()) };
    assert_eq!(output.write_volatile(&slice).unwrap(), bytes.len());
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
fn output_descriptor_pipe() {
    let (mut reader, writer) = std::io::pipe().unwrap();
    let flags = unsafe { fcntl(writer.as_raw_fd(), F_GETFL) };
    emit(writer.as_raw_fd());
    let mut bytes = [0; 7];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"console");
    assert_eq!(unsafe { fcntl(writer.as_raw_fd(), F_GETFL) }, flags);
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
