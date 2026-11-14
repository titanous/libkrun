//! What the kernel releases first when a process dies holding both the lifecycle lock and the
//! only reference to a VM (review nitrod-opus K4: a VMM killed between the create call and the
//! handoff of its descriptor). Needs a Nitro host and root.
//!
//! `release_order LOCK ROUNDS` runs ROUNDS children per variant. Each child takes the flock on
//! LOCK, creates a 256 MiB VM (under the lock, like the fork's create in `Teardown` mode), and is
//! killed with SIGKILL while holding both. The parent waits for the flock on its own open file
//! description and, the moment it gets it, reads the Nitro memory pool's `in_use`. If the VM's
//! 256 MiB are already gone, the VM was released before the lock.
//!
//! Variants: `low` (the lock's descriptor is numbered below the VM's) and `high` (the lock's only
//! descriptor is moved above the VM's with dup2), to tell descriptor order from other orders.
//! Other programs on the host change `in_use` too; each line prints the raw numbers.
use nitro_sandbox::host::Sandbox;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

const VM_BYTES: u64 = 256 << 20;
const IN_USE: &str = "/sys/class/misc/nitro_enclaves/mem_pool/in_use";

fn in_use() -> u64 {
    std::fs::read_to_string(IN_USE)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

fn child(lock: &str, variant: &str) -> ! {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock)
        .expect("open lock");
    let mut fd = file.as_raw_fd();
    std::mem::forget(file);
    assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX) }, 0, "flock");
    let sandbox = Sandbox::create(VM_BYTES as usize, 1, None, None, None).expect("create");
    if variant == "high" {
        // Same open file description, now only under a descriptor above the VM's.
        assert_eq!(unsafe { libc::dup2(fd, 200) }, 200);
        unsafe { libc::close(fd) };
        fd = 200;
    }
    println!(
        "ready lock_fd={fd} vm_fd={} in_use={}",
        sandbox.vm_fd(),
        in_use()
    );
    std::io::stdout().flush().unwrap();
    let mut go = [0u8];
    let _ = std::io::stdin().read(&mut go);
    std::mem::forget(sandbox);
    unsafe { libc::raise(libc::SIGKILL) };
    unreachable!()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("child") {
        child(&args[2], &args[3]);
    }
    let lock = args
        .get(1)
        .expect("usage: release_order LOCK ROUNDS")
        .clone();
    let rounds: usize = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(10);
    for variant in ["low", "high"] {
        for round in 0..rounds {
            let baseline = in_use();
            let mut child = std::process::Command::new(&args[0])
                .args(["child", &lock, variant])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut ready = String::new();
            let mut stdout = child.stdout.take().unwrap();
            let mut byte = [0u8];
            while stdout.read(&mut byte).unwrap_or(0) == 1 && byte[0] != b'\n' {
                ready.push(byte[0] as char);
            }
            // Wait for the lock on a separate open file description, from another thread.
            let waiter_lock = lock.clone();
            let waiter = std::thread::spawn(move || {
                let file = std::fs::File::open(&waiter_lock).unwrap();
                assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
                let at_lock = in_use();
                let acquired = Instant::now();
                // How long until the VM's bytes are gone (bounded).
                let mut gone_after = None;
                while acquired.elapsed() < Duration::from_secs(5) {
                    if in_use() + VM_BYTES / 2 <= at_lock {
                        gone_after = Some(acquired.elapsed());
                        break;
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
                (at_lock, gone_after)
            });
            std::thread::sleep(Duration::from_millis(100));
            let killed = Instant::now();
            child.stdin.take().unwrap().write_all(b"g").unwrap();
            let status = child.wait().unwrap();
            let reaped = killed.elapsed();
            let (at_lock, gone_after) = waiter.join().unwrap();
            let held_at_lock = at_lock.saturating_sub(baseline);
            println!(
                "variant={variant} round={round} {ready} baseline={baseline} at_lock={at_lock} \
                 vm_counted_at_lock={} gone_after_lock={gone_after:?} reaped_after={reaped:?} status={status}",
                held_at_lock >= VM_BYTES
            );
        }
    }
}
