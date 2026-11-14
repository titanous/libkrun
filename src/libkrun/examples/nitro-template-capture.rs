#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nitro_sandbox::{
        lifecycle::{LifecycleLock, Serialization},
        runtime::{Exit, Runtime},
    };
    let args: Vec<_> = std::env::args().collect();
    if !(2..=3).contains(&args.len()) {
        return Err("usage: nitro-template-capture identity-source.eif [receiver.eif]".into());
    }
    let request = vmm::capture_request::CaptureRequest::default();
    let mut runtime = Runtime::with_lifecycle_lock(
        &args[1],
        1 << 30,
        1,
        707,
        std::time::Duration::from_secs(15),
        Some(LifecycleLock {
            path: "/run/holodeck/nitro-lifecycle.lock".into(),
            serialization: Serialization::StartAndTeardown,
        }),
    )?;
    runtime.set_capture_handler(Box::new(vmm::nitro_capture::NitroCapture {
        request: request.clone(),
        vcpus: 1,
        ram_mib: 1024,
        topology: b"diagnostic-no-devices".to_vec(),
        compatibility: b"nitro-arm64-v1".to_vec(),
    }));
    let unarmed = std::env::var_os("CAPTURE_UNARMED").is_some();
    runtime.set_console(nitro_sandbox::console::Console::new(
        Some(Box::new(std::fs::File::create("shared-console.log")?)),
        1024 * 1024,
        1024 * 1024,
    ));
    if !unarmed {
        request.arm(std::time::Duration::from_secs(15))?;
    }
    let exit = runtime.run()?;
    if unarmed {
        if exit != Exit::PowerOff {
            return Err("unarmed source did not recover and power off".into());
        }
        let console = std::fs::read_to_string("shared-console.log")?;
        if !console.contains("COPY_VERIFY ok=1") || !console.contains("NITRO_RESUMED path=1") {
            return Err("unarmed source recovery markers missing".into());
        }
        println!("UNARMED_SOURCE_RECOVERED");
        return Ok(());
    }
    if exit != Exit::Frozen {
        return Err("capture did not freeze source".into());
    }
    let template = request.wait()?;
    let reopened = vmm::template::Template::open(template.file.try_clone()?)?;
    println!(
        "SHARED_TEMPLATE_CAPTURE ram_mib={} regions={} identity_bytes={}",
        reopened.ram_mib,
        reopened.regions.len(),
        reopened.compatibility.len()
    );
    if args.len() == 3 {
        use std::{
            io::Read,
            time::{Duration, Instant, SystemTime, UNIX_EPOCH},
        };
        let mut receiver = nitro_sandbox::host::Sandbox::with_lifecycle_lock(
            1 << 30,
            1,
            None,
            Some(LifecycleLock {
                path: "/run/holodeck/nitro-lifecycle.lock".into(),
                serialization: Serialization::StartAndTeardown,
            }),
        )?;
        receiver.load(&args[2])?;
        receiver.start(708)?;
        let base = receiver.ram.address() as usize;
        let ready = (base + nitro_sandbox::receiver::CONTROL_OFFSET) as *const u64;
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { ready.read_volatile() } != nitro_sandbox::receiver::READY {
            if Instant::now() >= deadline {
                return Err("receiver ready deadline".into());
            }
            std::thread::yield_now();
        }
        vmm::nitro_restore::validate(
            &reopened,
            1,
            1024,
            b"diagnostic-no-devices",
            &reopened.compatibility,
        )?;
        let mut seed = [0; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
        unsafe {
            vmm::nitro_restore::copy_and_release(
                &reopened,
                base as *mut u8,
                1024,
                seed,
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64,
            )?;
        }
        let mut console = nitro_sandbox::console::Console::new(
            Some(Box::new(std::fs::File::create(
                "shared-restore-console.log",
            )?)),
            1024 * 1024,
            1024 * 1024,
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        while !receiver.powered_off(0)? {
            receiver.drain_console(&mut console)?;
            if Instant::now() >= deadline {
                return Err("sealed restore deadline".into());
            }
            std::thread::yield_now();
        }
        receiver.drain_console(&mut console)?;
        if !std::fs::read_to_string("shared-restore-console.log")?
            .contains("COPY_VERIFY ok=1 bad=0")
        {
            return Err("sealed restore verification missing".into());
        }
        println!("SHARED_TEMPLATE_RESTORE_VERIFIED");
    }
    Ok(())
}
#[cfg(not(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
)))]
fn main() {
    eprintln!("requires arm64 Linux Nitro feature");
}
