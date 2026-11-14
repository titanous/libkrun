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
    fn offset(gpa: u64) -> Option<usize> {
        if (0x40000000..0x80000000).contains(&gpa) {
            Some((gpa - 0x40000000) as usize)
        } else if gpa >= 0x400000000 {
            Some((gpa - 0x400000000 + 0x40000000) as usize)
        } else {
            None
        }
    }
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 {
        return Err("usage: copy-probe source.eif receiver.eif console.log copies".into());
    }
    let memory_mib: usize = std::env::var("COPY_MEMORY_MIB")
        .unwrap_or_else(|_| "1024".into())
        .parse()?;
    if ![1024, 4096].contains(&memory_mib) {
        return Err("memory must be1024or4096MiB".into());
    }
    let memory_size = memory_mib << 20;
    let cpus: u32 = std::env::var("COPY_CPUS")
        .unwrap_or_else(|_| "1".into())
        .parse()?;
    if ![1, 4, 8].contains(&cpus) {
        return Err("CPU count must be 1,4,8".into());
    }
    let concurrent = std::env::var_os("COPY_CONCURRENT").is_some();
    let copies: usize = args[4].parse()?;
    if concurrent && copies != 4 {
        return Err("concurrent diagnostic requires exactly four copies".into());
    }
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
    let mut source = Sandbox::with_lifecycle_lock(memory_size, cpus, None, lock())?;
    eprintln!("SOURCE_CREATED");
    source.load(&args[1])?;
    eprintln!("SOURCE_LOADED");
    source.start(701)?;
    eprintln!("SOURCE_STARTED");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut control = None;
    let mut owned = nitro_sandbox::owned_memory::BootMemory::default();
    let mut freeze_begin = None;
    loop {
        source.drain_console(&mut console)?;
        let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
        std::fs::write(&args[3], &text)?;
        if freeze_begin.is_none() && text.contains("COPY_FREEZE_BEGIN") {
            freeze_begin = Some(Instant::now());
        }
        for line in text.lines() {
            if let Some(s) = line.split("NITRO_FREEZE_CONTROL phys=").nth(1) {
                control = u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
            }
            if line.ends_with(']') {
                owned.observe(line)?;
            }
        }
        if let Some(gpa) = control {
            if word(
                source.ram.address() as usize,
                offset(gpa).ok_or("invalid control GPA")?,
            ) == nitro_sandbox::cooperative::FROZEN_MAGIC
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
    println!(
        "FREEZE_OBSERVED us={}",
        freeze_begin
            .ok_or("missing freeze marker")?
            .elapsed()
            .as_micros()
    );
    let plan = owned.ranges(memory_size)?;
    let owned = plan
        .iter()
        .map(|&(gpa, _, len)| (gpa, gpa + len as u64))
        .collect::<std::collections::BTreeMap<_, _>>();
    let ranges = plan
        .iter()
        .map(|&(_, at, len)| (at, len))
        .collect::<Vec<_>>();
    let control_offset = offset(control).ok_or("control outside RAM")?;
    let base = source.ram.address() as usize;
    let page = unsafe { std::slice::from_raw_parts((base + control_offset) as *const u8, 4096) };
    let entry = nitro_sandbox::cooperative::ResumeEntry::parse(page, |a, n| {
        owned
            .iter()
            .any(|(&start, &end)| a >= start && a.checked_add(n).is_some_and(|v| v <= end))
    })?;
    let begin = Instant::now();
    let mut template = Vec::new();
    for &(at, len) in &ranges {
        let bytes = unsafe { std::slice::from_raw_parts((base + at) as *const u8, len) };
        let mut copy = vec![0u8; len];
        let reserved = if at == 0 {
            0x30000000..0x30200000
        } else {
            0..0
        };
        nitro_sandbox::copy_memory::copy_extent(bytes, &mut copy, reserved, 16)?;
        template.push(copy);
    }
    println!(
        "CAPTURE bytes={} us={}",
        ranges.iter().map(|r| r.1).sum::<usize>(),
        begin.elapsed().as_micros()
    );
    drop(source);
    let mut alive = Vec::new();
    for copy in 0..copies {
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut console = Console::new(
            Some(Box::new(Sink(output.clone()))),
            1024 * 1024,
            1024 * 1024,
        );
        let mut receiver = Sandbox::with_lifecycle_lock(memory_size, cpus, None, lock())?;
        receiver.load(&args[2])?;
        let receiver_start = Instant::now();
        receiver.start(702 + copy as u64)?;
        let base = receiver.ram.address() as usize;
        let ready = base + 0x30002000;
        let deadline = Instant::now() + Duration::from_secs(5);
        while word(ready, 0) != 0x5944414552564352 {
            if Instant::now() >= deadline {
                return Err("receiver ready deadline".into());
            }
            std::thread::yield_now();
        }
        println!("RECEIVER_START us={}", receiver_start.elapsed().as_micros());
        let begin = Instant::now();
        for (&(at, len), copy) in ranges.iter().zip(&template) {
            let bytes = unsafe { std::slice::from_raw_parts_mut((base + at) as *mut u8, len) };
            let reserved = if at == 0 {
                0x30000000..0x30200000
            } else {
                0..0
            };
            nitro_sandbox::copy_memory::copy_extent(copy, bytes, reserved, 16)?;
        }
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
        // SAFETY: READY was observed in the live receiver reservation; all
        // owned RAM was copied, and this diagnostic has no device backends.
        unsafe {
            nitro_sandbox::receiver::release(ready as *mut u64, entry)?;
        }
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
            if concurrent
                && verified
                && text.lines().any(|line| {
                    line.contains("COPY_VERIFY ok=1")
                        && line.contains("realtime=")
                        && text.ends_with('\n')
                })
            {
                std::fs::write(format!("{}.copy{copy}", args[3]), &text)?;
                break;
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
        if concurrent {
            alive.push((receiver, console, output));
        }
    }
    if concurrent {
        if alive.len() != 4
            || alive
                .iter()
                .any(|(receiver, _, _)| receiver.powered_off(0).unwrap_or(true))
        {
            return Err("four simultaneous live copies not observed".into());
        }
        println!("FOUR_ALIVE count=4");
        for (index, (mut receiver, mut console, output)) in alive.into_iter().enumerate() {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !receiver.powered_off(0)? {
                receiver.drain_console(&mut console)?;
                if Instant::now() >= deadline {
                    return Err("concurrent shutdown deadline".into());
                }
                std::thread::yield_now();
            }
            receiver.drain_console(&mut console)?;
            std::fs::write(format!("{}.copy{index}", args[3]), &*output.lock().unwrap())?;
            drop(receiver);
        }
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("Linux required");
}
