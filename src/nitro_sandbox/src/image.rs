//! Deterministic arm64 EIF v4 packaging matching holodeck guest/image/eif.py.
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
#[path = "image_stream.rs"]
mod stream;
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::{Arc, Mutex, OnceLock};
pub use stream::cached_files;
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
    fn streamed_files_match_package_and_reject_corrupted_cache() {
        use std::os::unix::fs::{FileExt, PermissionsExt};
        let directory = std::env::temp_dir().join(format!("nitro-stream-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let kernel = directory.join("kernel");
        let initrd = directory.join("initrd");
        std::fs::write(&kernel, b"kernel bytes").unwrap();
        std::fs::write(&initrd, b"initrd bytes").unwrap();
        for init in [None, Some(initrd.as_path())] {
            let (image, _) =
                cached_files(&kernel, init, b"console=hvc0 panic=-1", 1024 << 20, 1, None).unwrap();
            let expected = package(
                b"kernel bytes",
                init.map(|_| &b"initrd bytes"[..]),
                b"console=hvc0 panic=-1",
                1024 << 20,
                1,
            )
            .unwrap();
            let mut bytes = vec![0; expected.len()];
            image.read_exact_at(&mut bytes, 0).unwrap();
            assert_eq!(bytes, expected);
        }
        let (_, hit) = cached_files(&kernel, None, b"", 1024 << 20, 1, Some(&directory)).unwrap();
        assert!(!hit);
        assert!(
            cached_files(&kernel, None, b"", 1024 << 20, 1, Some(&directory))
                .unwrap()
                .1
        );
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                cached_files(
                    &kernel,
                    None,
                    b"concurrent",
                    1024 << 20,
                    1,
                    Some(&directory),
                )
                .unwrap()
            });
            let second = scope.spawn(|| {
                cached_files(
                    &kernel,
                    None,
                    b"concurrent",
                    1024 << 20,
                    1,
                    Some(&directory),
                )
                .unwrap()
            });
            first.join().unwrap();
            second.join().unwrap();
        });
        for cached in std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "eif"))
        {
            std::fs::write(cached, b"corrupted").unwrap();
        }
        assert_eq!(
            cached_files(&kernel, None, b"", 1024 << 20, 1, Some(&directory))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(directory).unwrap();
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
        // Immutable entries are published by atomic rename. Readers neither
        // acquire an exclusive lock nor trust the content-addressed filename.
        let expected = package(kernel, initrd, cmdline, memory, cpus)?;
        let expected_hash: [u8; 32] = Sha256::digest(&expected).into();
        let name: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
        let destination = directory.join(format!("{name}.eif"));
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&destination)
        };
        match open() {
            Ok(mut file) => {
                use std::io::Read;
                let mut hash = Sha256::new();
                let mut buffer = [0u8; 65536];
                loop {
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    hash.update(&buffer[..count]);
                }
                let actual: [u8; 32] = hash.finalize().into();
                if actual != expected_hash {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "cached EIF content hash mismatch",
                    ));
                }
                return Ok((Arc::new(file), true));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let bytes = expected;
        static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = directory.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
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
    const CACHE_BYTES: u64 = 64 * 1024 * 1024;
    let size = file.metadata()?.len();
    if size <= CACHE_BYTES {
        let mut used: u64 = cache
            .iter()
            .map(|(_, file)| {
                file.metadata()
                    .map(|meta| meta.len())
                    .unwrap_or(CACHE_BYTES)
            })
            .sum();
        while used.saturating_add(size) > CACHE_BYTES {
            let Some((_, old)) = cache.pop_front() else {
                break;
            };
            used = used.saturating_sub(old.metadata()?.len());
        }
        cache.push_back((key, file.clone()));
    }
    Ok((file, false))
}
