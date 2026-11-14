//! Bounded-buffer EIF packaging from opened input files.
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;
use std::sync::Arc;

fn copy_hash(
    reader: &mut dyn Read,
    output: &mut dyn Write,
    hash: &mut Sha256,
    crc: &mut crc32fast::Hasher,
) -> io::Result<()> {
    let mut buffer = [0u8; 65536];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        crc.update(&buffer[..count]);
    }
}
fn emit(
    kernel: &mut File,
    initrd: &mut Option<File>,
    cmdline: &[u8],
    memory: u64,
    cpus: u64,
    output: &mut File,
) -> io::Result<[u8; 32]> {
    let kernel_len = kernel.metadata()?.len();
    if kernel_len == 0 || memory == 0 || cpus == 0 || cmdline.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid EIF inputs",
        ));
    }
    let mut sections = vec![(1u16, kernel_len), (2, cmdline.len() as u64)];
    if let Some(file) = initrd {
        sections.push((3, file.metadata()?.len()));
    }
    sections.push((5, super::META.len() as u64));
    let mut header = [0u8; 548];
    header[..4].copy_from_slice(b".eif");
    header[4..6].copy_from_slice(&4u16.to_be_bytes());
    header[6..8].copy_from_slice(&0xa41u16.to_be_bytes());
    header[8..16].copy_from_slice(&memory.to_be_bytes());
    header[16..24].copy_from_slice(&cpus.to_be_bytes());
    header[26..28].copy_from_slice(&(sections.len() as u16).to_be_bytes());
    let mut offset = 548u64;
    for (index, (_, size)) in sections.iter().enumerate() {
        header[28 + index * 8..36 + index * 8].copy_from_slice(&offset.to_be_bytes());
        header[284 + index * 8..292 + index * 8].copy_from_slice(&size.to_be_bytes());
        offset = offset
            .checked_add(12)
            .and_then(|n| n.checked_add(*size))
            .ok_or_else(|| io::Error::other("EIF size overflow"))?;
    }
    output.write_all(&header)?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(&header[..544]);
    let mut ignored = Sha256::new();
    for (kind, size) in sections {
        let mut section = Vec::with_capacity(12);
        section.extend_from_slice(&kind.to_be_bytes());
        section.extend_from_slice(&0u16.to_be_bytes());
        section.extend_from_slice(&size.to_be_bytes());
        output.write_all(&section)?;
        crc.update(&section);
        match kind {
            1 => {
                kernel.rewind()?;
                copy_hash(kernel, output, &mut ignored, &mut crc)?;
            }
            3 => {
                let file = initrd.as_mut().unwrap();
                file.rewind()?;
                copy_hash(file, output, &mut ignored, &mut crc)?;
            }
            _ => {
                let bytes = if kind == 2 { cmdline } else { super::META };
                output.write_all(bytes)?;
                crc.update(bytes);
            }
        }
    }
    output.seek(SeekFrom::Start(544))?;
    output.write_all(&crc.finalize().to_be_bytes())?;
    output.rewind()?;
    let mut hash = Sha256::new();
    let mut sink = io::sink();
    let mut ignored_crc = crc32fast::Hasher::new();
    copy_hash(output, &mut sink, &mut hash, &mut ignored_crc)?;
    Ok(hash.finalize().into())
}
pub fn cached_files(
    kernel_path: &Path,
    initrd_path: Option<&Path>,
    cmdline: &[u8],
    memory: u64,
    cpus: u64,
    directory: Option<&Path>,
) -> io::Result<(Arc<File>, bool)> {
    let mut kernel = File::open(kernel_path)?;
    let mut initrd = initrd_path.map(File::open).transpose()?;
    let raw = unsafe {
        libc::memfd_create(
            c"libkrun-eif".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut image = unsafe { File::from_raw_fd(raw) };
    let digest = emit(&mut kernel, &mut initrd, cmdline, memory, cpus, &mut image)?;
    if let Some(directory) = directory {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let metadata = std::fs::symlink_metadata(directory)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "image cache must be owned by VMM uid and not group/other writable",
            ));
        }
        let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let destination = directory.join(format!("{name}.eif"));
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&destination)
        };
        match open() {
            Ok(mut file) => {
                let mut hash = Sha256::new();
                let mut crc = crc32fast::Hasher::new();
                copy_hash(&mut file, &mut io::sink(), &mut hash, &mut crc)?;
                let actual: [u8; 32] = hash.finalize().into();
                if actual != digest {
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
        static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = directory.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        image.rewind()?;
        let result = io::copy(&mut image, &mut file).and_then(|_| file.sync_all());
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        std::fs::rename(&temporary, &destination)?;
        return Ok((Arc::new(open()?), false));
    }
    if unsafe {
        libc::fcntl(
            image.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((Arc::new(image), false))
}
