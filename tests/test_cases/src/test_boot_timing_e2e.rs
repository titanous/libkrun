//! End-to-end boot timing integration test with full correlated timeline.
//!
//! Measures wall-clock time from before builder.build() to receiving a token from
//! the guest over vsock. The VM is configured with all major subsystems enabled:
//!   - In-memory virtiofs (MinimalFileSystem) serving a random 32-byte token
//!   - In-memory block device (MemBlockBackend, 32 KiB)
//!   - Loopback async net backend
//!   - Balloon device enabled
//!   - initcall_debug in kernel cmdline (kernel ring buffer, zero KVM-exit overhead)
//!
//! Wire protocol (guest → host after token):
//!   [n_milestones: u8 = 4]        CLOCK_BOOTTIME timestamps in ms
//!   [milestones: n × u64 LE]      in_guest_start / virtiofs_mounted / token_read / pre_send
//!   [kmsg_len: u16 LE]            byte length of filtered /dev/kmsg text
//!   [kmsg_text: kmsg_len bytes]   kernel messages matching timing-relevant patterns
//!
//! The host combines its own phase timestamps with the guest data to print a
//! correlated timeline. Correlation anchor: guest pre_send ≈ host vsock accept.
//!
//! Kernel cmdline: uses the builder default (which includes quiet). The ring buffer
//! still stores all kernel messages regardless of quiet; /dev/kmsg reads directly
//! from it, giving timestamps for virtio probing, subsystem init, etc.
//!
//! The first stdout line "boot_timing_e2e: NNms" is used by `just bench-boot`.

use macros::{guest, host};

pub struct TestBootTimingE2e;

const VSOCK_PORT: u32 = 5730;
const FS_TAG: &str = "timing-fs";
const TOKEN_FILE: &str = "token";
const TOKEN_LEN: usize = 32;
#[allow(dead_code)] // used in host mod only
const SECTOR_COUNT: u64 = 64; // 64 * 512 = 32 KiB

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::loopback_net::LoopbackFactory;
    use crate::mem_block_backend::{MemBlockBackend, MemBlockBackendFactory};
    use crate::minimal_filesystem::MinimalFileSystem;
    use crate::{Test, TestSetup};
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixListener;
    use std::process::Child;
    use std::thread;
    use std::time::{Duration, Instant};

    const MILESTONE_LABELS: &[&str] = &[
        "in_guest start",
        "virtiofs mounted",
        "token read",
        "pre-send",
    ];

    impl Test for TestBootTimingE2e {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let mut token = [0u8; TOKEN_LEN];
            std::fs::File::open("/dev/urandom")
                .unwrap()
                .read_exact(&mut token)
                .unwrap();

            let sock_path = test_setup.tmp_dir.join("boot_timing_e2e.sock");
            let listener = UnixListener::bind(&sock_path).unwrap();

            let fs = MinimalFileSystem::new(vec![(TOKEN_FILE, token.to_vec())]);
            let (backend, _data) = MemBlockBackend::new(SECTOR_COUNT, 0x00);
            let factory = MemBlockBackendFactory::new(backend);

            let block_cfg = krun::BlockDeviceConfig {
                block_id: "timing-blk".to_string(),
                cache_type: krun::CacheType::Writeback,
                disk_type: krun::BlockDeviceType::CustomAsyncFactory {
                    factory: Box::new(factory),
                },
                is_disk_read_only: false,
                direct_io: false,
            };

            let vcpus: u8 = std::env::var("KRUN_BENCH_VCPUS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);

            let mut builder = krun::Builder::new();
            builder.vm_config(vcpus, 256)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_virtiofs(FS_TAG, Box::new(fs), None);
            builder.add_block_cfg(block_cfg);
            builder.add_net_device(
                krun::VirtioNetBackend::CustomAsyncFactory(Box::new(LoopbackFactory::new())),
                [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xf0],
                0,
            );
            builder.enable_balloon();
            builder.add_vsock_port(VSOCK_PORT, sock_path, false);

            // Allow injecting extra kernel cmdline args for benchmarking (e.g. "swiotlb=noforce").
            if let Ok(extra) = std::env::var("KRUN_BENCH_EXTRA_CMDLINE") {
                if !extra.is_empty() {
                    let args: Vec<&str> = extra.split_whitespace().collect();
                    builder.add_kernel_cmdline_args(&args);
                }
            }

            let t0 = Instant::now();
            let context = builder.build()?;
            let t_build_ms = t0.elapsed().as_millis() as u64;

            let vm_thread = thread::spawn(move || context.run());

            // Set 30-second timeout on accept() so the runner fails fast instead of
            // hanging indefinitely if the guest never connects over vsock.
            unsafe {
                let timeval = libc::timeval {
                    tv_sec: 30,
                    tv_usec: 0,
                };
                libc::setsockopt(
                    listener.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &timeval as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                );
            }
            let (mut stream, _) = listener.accept().unwrap();
            let t_accept_ms = t0.elapsed().as_millis() as u64;
            stream
                .set_read_timeout(Some(Duration::from_secs(60)))
                .unwrap();

            // Read token.
            let mut received = vec![0u8; TOKEN_LEN];
            stream.read_exact(&mut received).unwrap();

            assert_eq!(
                received.as_slice(),
                &token,
                "token mismatch: expected {:?}, got {:?}",
                &token,
                received.as_slice(),
            );

            // Read guest milestones: [n: u8] [ts_ms: u64 LE each].
            let mut count_buf = [0u8; 1];
            stream.read_exact(&mut count_buf).unwrap();
            let n_milestones = count_buf[0] as usize;
            let mut ms_buf = vec![0u8; n_milestones * 8];
            stream.read_exact(&mut ms_buf).unwrap();
            let milestones: Vec<u64> = (0..n_milestones)
                .map(|i| u64::from_le_bytes(ms_buf[i * 8..i * 8 + 8].try_into().unwrap()))
                .collect();

            // Read filtered /dev/kmsg text: [len: u16 LE] [text: len bytes].
            let mut len_buf = [0u8; 2];
            stream.read_exact(&mut len_buf).unwrap();
            let kmsg_len = u16::from_le_bytes(len_buf) as usize;
            let mut kmsg_buf = vec![0u8; kmsg_len];
            if kmsg_len > 0 {
                stream.read_exact(&mut kmsg_buf).unwrap();
            }
            let t_recv_ms = t0.elapsed().as_millis() as u64;
            let kmsg_text = String::from_utf8_lossy(&kmsg_buf);

            // Correlate timelines: guest pre_send ≈ host accept.
            // kernel_start_ms = how long after T0 the kernel started running.
            let kernel_start_ms = if n_milestones >= 4 {
                let g_presend = milestones[3];
                t_accept_ms.saturating_sub(g_presend)
            } else {
                t_build_ms
            };

            // Print the summary line (used by bench-boot for stat extraction).
            println!("boot_timing_e2e: {t_recv_ms}ms");

            // Print host timeline.
            println!("  host:");
            println!("    T+{t_build_ms}ms\tbuild() complete (VMM infrastructure ready)");
            println!("    T+{t_accept_ms}ms\tvsock accepted");
            println!("    T+{t_recv_ms}ms\ttoken received (e2e total)");

            // Print guest CLOCK_BOOTTIME milestones aligned to host clock.
            println!("  guest CLOCK_BOOTTIME (kernel start ≈ T+{kernel_start_ms}ms estimated):");
            for (i, &g_ms) in milestones.iter().enumerate() {
                let label = MILESTONE_LABELS.get(i).copied().unwrap_or("?");
                let t_ms = kernel_start_ms + g_ms;
                println!("    G+{g_ms}ms\t{label}\t(≈ T+{t_ms}ms)");
            }

            // Print kernel messages with their timestamps, filtered for timing-relevant entries.
            // Format: "priority,seq,ts_usec,flags[,extra];message"
            // Timing-relevant patterns: virtio probe, net families, root mount, init.
            const KMSG_PATTERNS: &[&str] = &[
                "virtio",
                "NET: Registered",
                "VFS:",
                "Freeing unused",
                "Run /init",
                "sched_clock",
                "taskstats",
                "printk:",
                "Btrfs",
            ];
            let mut printed_kmsg = 0usize;
            for line in kmsg_text.lines() {
                if let Some((header, msg)) = line.split_once(';') {
                    let msg = msg.trim_end();
                    if !KMSG_PATTERNS.iter().any(|p| msg.contains(p)) {
                        continue;
                    }
                    if printed_kmsg == 0 {
                        println!("  kernel ring buffer (selected):");
                    }
                    let ts_usec: u64 = header
                        .split(',')
                        .nth(2)
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                    let ts_ms = ts_usec / 1000;
                    let t_ms = kernel_start_ms + ts_ms;
                    println!("    G+{ts_ms}ms\t{msg}\t(≈ T+{t_ms}ms)");
                    printed_kmsg += 1;
                }
            }

            vm_thread.join().ok();
            Ok(())
        }

        /// Forward the timing output to runner's stderr for terminal visibility.
        fn check(self: Box<Self>, child: Child) {
            let output = crate::wait_with_timeout(child, crate::TEST_TIMEOUT);
            let stdout = String::from_utf8(output.stdout).unwrap();

            // Print full timeline to stderr (visible in terminal; bench-boot only
            // parses the "boot_timing_e2e:" line so extra output is harmless there).
            // Print all timing-section lines (including interleaved "OK" from the guest
            // console, which can race with host kmsg output) to stderr.
            for line in stdout.lines() {
                if line.starts_with("boot_timing_e2e:")
                    || line.starts_with("  ")
                    || line.starts_with("    ")
                    || line == "OK"
                {
                    eprintln!("{line}");
                }
            }

            assert!(
                stdout.contains("OK\n"),
                "expected stdout to contain \"OK\\n\", got {:?}",
                stdout,
            );
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::vsock_helpers::vsock_connect;
    use crate::Test;
    use std::ffi::CString;
    use std::fs;
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::fs::OpenOptionsExt;

    const MOUNT_POINT: &str = "/mnt/timing";

    /// Returns milliseconds since kernel boot (CLOCK_BOOTTIME).
    fn boottime_ms() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
        ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
    }

    /// Reads /dev/kmsg non-blocking and returns lines matching timing-relevant patterns.
    /// Keeps initcall start/end lines, virtio probe lines, and key subsystem messages.
    fn read_kmsg_timing_lines() -> Vec<u8> {
        let mut file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/kmsg")
        {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };

        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // Convert via lossy UTF-8 and rebuild as clean text lines (handles any binary in records).
        let text = String::from_utf8_lossy(&raw);
        let mut out = String::new();
        for line in text.lines() {
            out.push_str(line);
            out.push('\n');
        }
        out.into_bytes()
    }

    impl Test for TestBootTimingE2e {
        fn in_guest(self: Box<Self>) {
            let t_start = boottime_ms();

            // Mount the in-memory virtiofs.
            fs::create_dir_all(MOUNT_POINT).expect("create mountpoint");
            let source = CString::new(FS_TAG).unwrap();
            let target = CString::new(MOUNT_POINT).unwrap();
            let fstype = CString::new("virtiofs").unwrap();
            let ret = unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    fstype.as_ptr(),
                    0,
                    std::ptr::null(),
                )
            };
            assert!(
                ret == 0,
                "mount virtiofs failed: {}",
                std::io::Error::last_os_error()
            );
            let t_mounted = boottime_ms();

            // Read token from virtiofs.
            let path = format!("{}/{}", MOUNT_POINT, TOKEN_FILE);
            let token = fs::read(&path).expect("read token from virtiofs");
            assert_eq!(token.len(), TOKEN_LEN, "unexpected token length");
            let t_read = boottime_ms();

            // Collect kernel timing from ring buffer before connecting (non-blocking).
            let kmsg_bytes = read_kmsg_timing_lines();

            let t_presend = boottime_ms();
            let milestones: [u64; 4] = [t_start, t_mounted, t_read, t_presend];

            // Send: token + milestones + kmsg.
            let mut stream = vsock_connect(VSOCK_PORT);
            stream.write_all(&token).unwrap();

            // [n_milestones: u8] [ms each: u64 LE]
            stream.write_all(&[milestones.len() as u8]).unwrap();
            for &ms in &milestones {
                stream.write_all(&ms.to_le_bytes()).unwrap();
            }

            // [kmsg_len: u16 LE] [kmsg_text]
            let kmsg_len = kmsg_bytes.len().min(u16::MAX as usize);
            stream.write_all(&(kmsg_len as u16).to_le_bytes()).unwrap();
            if kmsg_len > 0 {
                stream.write_all(&kmsg_bytes[..kmsg_len]).unwrap();
            }

            println!("OK");
        }
    }
}
