#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nitro_sandbox::{
        console::Console,
        host::Sandbox,
        lifecycle::{LifecycleLock, Serialization},
    };
    use std::{
        io::Write,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn word(base: usize, offset: usize) -> u64 {
        unsafe { ((base + offset) as *const u64).read_volatile() }
    }
    fn put(base: usize, offset: usize, value: u64) {
        unsafe { ((base + offset) as *mut u64).write_volatile(value) }
    }
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 {
        return Err("usage: copy-probe source.eif receiver.eif console.log copies".into());
    }
    let copies: usize = args[4].parse()?;
    if !(1..=20).contains(&copies) {
        return Err("copies must be 1..20".into());
    }
    let lock = || {
        Some(LifecycleLock {
            path: "/run/holodeck/nitro-lifecycle.lock".into(),
            serialization: Serialization::StartAndTeardown,
        })
    };
    let output = Arc::new(Mutex::new(Vec::new()));
    let mut console = Console::new(
        Some(Box::new(Sink(output.clone()))),
        1024 * 1024,
        1024 * 1024,
    );
    let mut source = Sandbox::with_lifecycle_lock(1 << 30, 1, None, lock())?;
    eprintln!("SOURCE_CREATED");
    source.load(&args[1])?;
    eprintln!("SOURCE_LOADED");
    source.start(701)?;
    eprintln!("SOURCE_STARTED");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut control = None;
    let mut end = None;
    loop {
        source.drain_console(&mut console)?;
        let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
        std::fs::write(&args[3], &text)?;
        for line in text.lines() {
            if let Some(s) = line.split("NITRO_FREEZE_CONTROL phys=").nth(1) {
                control = u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
            }
            if line.contains("Initmem setup node 0 [mem 0x0000000040000000-") {
                if let Some(s) = line.split('-').next_back() {
                    end = u64::from_str_radix(s.trim_end_matches(']').trim_start_matches("0x"), 16)
                        .ok()
                        .map(|n| n + 1);
                }
            }
        }
        if let Some(gpa) = control {
            if word(source.ram.address() as usize, (gpa - 0x40000000) as usize)
                == nitro_sandbox::cooperative::FROZEN_MAGIC
            {
                break;
            }
        }
        if Instant::now() >= deadline {
            std::fs::write(&args[3], text)?;
            return Err("source freeze deadline".into());
        }
        std::thread::yield_now();
    }
    let control = control.unwrap();
    eprintln!("SOURCE_FROZEN control={control:x}");
    let end = end.ok_or("missing boot-owned RAM endpoint")?;
    eprintln!("OWNED_END={end:x}");
    let len = (end - 0x40000000) as usize;
    if len > 1 << 30 || len < 0x30200000 {
        return Err("invalid owned RAM endpoint".into());
    }
    let control_offset = (control - 0x40000000) as usize;
    let base = source.ram.address() as usize;
    let page = unsafe { std::slice::from_raw_parts((base + control_offset) as *const u8, 4096) };
    let entry = nitro_sandbox::cooperative::ResumeEntry::parse(page, |a, n| {
        a >= 0x40000000 && a.checked_add(n).is_some_and(|v| v <= end)
    })?;
    let begin = Instant::now();
    let mut template = vec![0u8; len];
    let bytes = unsafe { std::slice::from_raw_parts(base as *const u8, len) };
    nitro_sandbox::copy_memory::copy_extent(bytes, &mut template, 0x30000000..0x30200000, 16)?;
    println!("CAPTURE bytes={} us={}", len, begin.elapsed().as_micros());
    drop(source);
    for copy in 0..copies {
        output.lock().unwrap().clear();
        let mut receiver = Sandbox::with_lifecycle_lock(1 << 30, 1, None, lock())?;
        receiver.load(&args[2])?;
        receiver.start(702)?;
        let base = receiver.ram.address() as usize;
        let ready = base + 0x30002000;
        let deadline = Instant::now() + Duration::from_secs(5);
        while word(ready, 0) != 0x5944414552564352 {
            if Instant::now() >= deadline {
                return Err("receiver ready deadline".into());
            }
            std::thread::yield_now();
        }
        let begin = Instant::now();
        let bytes = unsafe { std::slice::from_raw_parts_mut(base as *mut u8, len) };
        nitro_sandbox::copy_memory::copy_extent(&template, bytes, 0x30000000..0x30200000, 16)?;
        println!("COPY_IN us={}", begin.elapsed().as_micros());
        let ctrl = base + control_offset;
        let realtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos() as u64;
        put(ctrl, 64, realtime);
        let mut seed = [0u8; 32];
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
        unsafe {
            std::ptr::copy_nonoverlapping(seed.as_ptr(), (ctrl + 80) as *mut u8, 32);
        }
        put(ctrl, 112, 1);
        put(ready, 16, entry.entry);
        put(ready, 24, entry.gic_list);
        put(ready, 32, entry.gic_count);
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        put(ready, 8, 1);
        let resume = Instant::now();
        let deadline = resume + Duration::from_secs(15);
        let mut verified = false;
        loop {
            receiver.drain_console(&mut console)?;
            let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
            if !verified && text.contains("COPY_VERIFY ok=1") {
                println!("RESUME_VERIFY us={}", resume.elapsed().as_micros());
                verified = true;
            }
            if receiver.powered_off(0)? {
                receiver.drain_console(&mut console)?;
                std::fs::write(format!("{}.copy{copy}", args[3]), &*output.lock().unwrap())?;
                if !verified {
                    return Err("receiver powered off without verified continuation".into());
                }
                break;
            }
            if Instant::now() >= deadline {
                std::fs::write(&args[3], text)?;
                return Err(format!("resume deadline progress={}", word(ready, 40)).into());
            }
            std::thread::yield_now();
        }
        println!("COPY_DONE index={copy}");
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("Linux required");
}
