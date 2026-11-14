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
    fn persistent_cache_reopens_identical_content() {
        use std::os::unix::fs::FileExt;
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("nitro-image-cache-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (first, hit) = cached_in(
            b"disk-cache-kernel",
            None,
            b"",
            1024 << 20,
            1,
            Some(&directory),
        )
        .unwrap();
        assert!(!hit);
        drop(first);
        let (second, hit) = cached_in(
            b"disk-cache-kernel",
            None,
            b"",
            1024 << 20,
            1,
            Some(&directory),
        )
        .unwrap();
        assert!(hit);
        let expected = package(b"disk-cache-kernel", None, b"", 1024 << 20, 1).unwrap();
        let mut bytes = vec![0; expected.len()];
        second.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(bytes, expected);
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(cached_in(b"k", None, b"", 1, 1, Some(&directory)).is_err());
        std::fs::remove_dir_all(directory).unwrap();
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
    cached_in(kernel, initrd, cmdline, memory, cpus, None)
}

pub fn cached_in(
    kernel: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &[u8],
    memory: u64,
    cpus: u64,
    directory: Option<&std::path::Path>,
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
    if let Some(directory) = directory {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let metadata = std::fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "image cache must be a daemon-owned directory not writable by group/other",
            ));
        }
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join(".lock"))?;
        let lock = crate::lifecycle::LifecycleLock {
            path: directory.join(".lock"),
            serialization: crate::lifecycle::Serialization::Teardown,
        }
        .open()?;
        let _guard = lock.acquire()?;
        drop(lock_file);
        let name: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
        let destination = directory.join(format!("{name}.eif"));
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&destination)
        };
        match open() {
            Ok(file) => return Ok((Arc::new(file), true)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let bytes = package(kernel, initrd, cmdline, memory, cpus)?;
        let temporary = directory.join(format!(".{name}.tmp"));
        // Exclusive cache lock proves no cooperating writer still owns this temp.
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        if let Err(error) = output.write_all(&bytes).and_then(|_| output.sync_all()) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        std::fs::rename(&temporary, &destination)?;
        return Ok((Arc::new(open()?), false));
    }
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
