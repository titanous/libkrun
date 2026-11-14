//! Deterministic arm64 EIF v4 packaging matching holodeck guest/image/eif.py.
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::{Arc, Mutex, OnceLock};
const HEADER: usize = 548;
const META: &[u8] = br#"{"ImageName":"holodeck","ImageVersion":"1","BuildMetadata":{"BuildTime":"","BuildTool":"holodeck","BuildToolVersion":"1","OperatingSystem":"Linux","KernelVersion":""},"DockerInfo":{},"CustomMetadata":null}"#;

pub fn package(
    kernel: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &[u8],
    memory: u64,
    cpus: u64,
) -> io::Result<Vec<u8>> {
    if kernel.is_empty() || memory == 0 || cpus == 0 || cmdline.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "nonempty kernel, positive memory/CPUs and NUL-free command line required",
        ));
    }
    let mut sections = vec![(1u16, kernel), (2, cmdline)];
    if let Some(initrd) = initrd {
        sections.push((3, initrd));
    }
    sections.push((5, META));
    let mut image = vec![0; HEADER];
    image[..4].copy_from_slice(b".eif");
    image[4..6].copy_from_slice(&4u16.to_be_bytes());
    image[6..8].copy_from_slice(&0xa41u16.to_be_bytes());
    image[8..16].copy_from_slice(&memory.to_be_bytes());
    image[16..24].copy_from_slice(&cpus.to_be_bytes());
    image[26..28].copy_from_slice(&(sections.len() as u16).to_be_bytes());
    for (index, (kind, payload)) in sections.into_iter().enumerate() {
        let offset = image.len() as u64;
        image[28 + index * 8..36 + index * 8].copy_from_slice(&offset.to_be_bytes());
        image[284 + index * 8..292 + index * 8]
            .copy_from_slice(&(payload.len() as u64).to_be_bytes());
        image.extend_from_slice(&kind.to_be_bytes());
        image.extend_from_slice(&0u16.to_be_bytes());
        image.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        image.extend_from_slice(payload);
    }
    let mut crc = crc32fast::Hasher::new();
    crc.update(&image[..544]);
    crc.update(&image[548..]);
    image[544..548].copy_from_slice(&crc.finalize().to_be_bytes());
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_python_builder_byte_for_byte_with_and_without_initramfs() {
        for initrd in [None, Some(&b"initramfs\0bytes"[..])] {
            let output = std::env::temp_dir().join(format!(
                "nitro-eif-{}-{}.bin",
                std::process::id(),
                initrd.is_some()
            ));
            let oracle =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/eif_reference.py");
            assert!(std::process::Command::new("python3")
                .arg(oracle)
                .arg(if initrd.is_some() { "some" } else { "none" })
                .arg(&output)
                .status()
                .unwrap()
                .success());
            let expected = std::fs::read(&output).unwrap();
            std::fs::remove_file(output).unwrap();
            assert_eq!(
                package(
                    b"kernel\0bytes",
                    initrd,
                    b"console=hvc0 panic=-1",
                    1024 << 20,
                    2
                )
                .unwrap(),
                expected
            );
        }
    }
    #[test]
    fn identical_content_hits_cache_and_images_are_sealed() {
        let (first, _) = cached(b"cache-kernel", None, b"", 1024 << 20, 1).unwrap();
        let (second, hit) = cached(b"cache-kernel", None, b"", 1024 << 20, 1).unwrap();
        assert!(hit);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(
            unsafe { libc::fcntl(first.as_raw_fd(), libc::F_GET_SEALS) } & libc::F_SEAL_WRITE != 0
        );
        assert!(package(b"", None, b"", 1, 1).is_err());
        assert!(package(b"k", None, b"bad\0cmd", 1, 1).is_err());
    }
}

type Cache = VecDeque<([u8; 32], Arc<File>)>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

/// Immutable memfd stays alive through image load. Identical content reuses the
/// packaged image; content is hashed on every call to avoid stale path metadata.
pub fn cached(
    kernel: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &[u8],
    memory: u64,
    cpus: u64,
) -> io::Result<(Arc<File>, bool)> {
    let mut hash = Sha256::new();
    hash.update(b"holodeck-eif-v4-arm64-1");
    for bytes in [kernel, cmdline] {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    hash.update([u8::from(initrd.is_some())]);
    if let Some(bytes) = initrd {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    hash.update(memory.to_be_bytes());
    hash.update(cpus.to_be_bytes());
    let key: [u8; 32] = hash.finalize().into();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((_, file)) = cache
        .lock()
        .unwrap()
        .iter()
        .find(|(cached, _)| cached == &key)
    {
        return Ok((file.clone(), true));
    }
    let bytes = package(kernel, initrd, cmdline, memory, cpus)?;
    let fd = unsafe {
        libc::memfd_create(
            c"libkrun-eif".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(&bytes)?;
    if unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let file = Arc::new(file);
    let mut cache = cache.lock().unwrap();
    if cache.len() == 8 {
        cache.pop_front();
    }
    cache.push_back((key, file.clone()));
    Ok((file, false))
}
