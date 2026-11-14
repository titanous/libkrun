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
        return Err(
            "usage: nitro-template-capture marked-source.eif [receiver.eif] (source command line must include cooperative_template)"
                .into(),
        );
    }
    let request = vmm::capture_request::CaptureRequest::default();
    let mut runtime = Runtime::with_lifecycle_lock(
        &args[1],
        1 << 30,
        1,
        707,
        std::time::Duration::from_secs(15),
        Some(LifecycleLock {
            path: nitro_sandbox::lifecycle::LIFECYCLE_LOCK_PATH.into(),
            serialization: Serialization::StartAndTeardown,
        }),
    )?;
    runtime.set_capture_handler(Box::new(vmm::nitro_capture::NitroCapture {
        request: request.clone(),
        vcpus: 1,
        ram_mib: 1024,
        topology: b"diagnostic-no-devices".to_vec(),
        compatibility: b"nitro-arm64-v1".to_vec(),
        export_path: None,
    }));
    let unarmed = std::env::var_os("CAPTURE_UNARMED").is_some();
    runtime.set_console(nitro_sandbox::console::Console::new(
        Some(Box::new(std::fs::File::create("shared-console.log")?)),
        1024 * 1024,
        1024 * 1024,
    ));
    // Arm only after the runtime has confirmed the guest's boot marker
    // (cooperative_template on the image's command line). Never before.
    let arm_request = request.clone();
    let armer = (!unarmed).then(|| {
        std::thread::spawn(move || -> std::io::Result<()> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while arm_request.require_marker().is_err() {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "boot marker was never confirmed; image must carry cooperative_template",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            arm_request.arm(std::time::Duration::from_secs(15))
        })
    });
    let exit = runtime.run()?;
    if let Some(armer) = armer {
        armer.join().map_err(|_| "arming thread panicked")??;
    }
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
    if args.len() == 3 && std::env::var_os("RECEIVER_ID_MISMATCH").is_some() {
        let mut receiver = nitro_sandbox::host::Sandbox::with_lifecycle_lock(
            1 << 30,
            1,
            None,
            Some(LifecycleLock {
                path: nitro_sandbox::lifecycle::LIFECYCLE_LOCK_PATH.into(),
                serialization: Serialization::StartAndTeardown,
            }),
        )?;
        receiver.load(&args[2])?;
        receiver.start(708)?;
        let base = receiver.ram.address() as usize;
        let control = (base + nitro_sandbox::receiver::CONTROL_OFFSET) as *const u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while unsafe { control.read_volatile() } != nitro_sandbox::receiver::READY {
            if std::time::Instant::now() >= deadline {
                return Err("receiver READY timeout".into());
            }
            std::thread::yield_now();
        }
        let before = unsafe { (base as *const u64).read_volatile() };
        let mut mismatched = reopened;
        *mismatched
            .compatibility
            .last_mut()
            .ok_or("missing identity")? ^= 1;
        if vmm::nitro_restore::validate_parked_receiver(&mismatched, &receiver).is_ok() {
            return Err("mismatched receiver accepted".into());
        }
        if unsafe { control.add(1).read_volatile() } != 0
            || unsafe { (base as *const u64).read_volatile() } != before
        {
            return Err("refusal changed receiver".into());
        }
        println!("RECEIVER_ID_MISMATCH_REFUSED release=0 memory_unchanged=1");
        return Ok(());
    }
    if args.len() == 3 {
        let compatibility = reopened.compatibility.clone();
        let mut receiver = Runtime::with_lifecycle_lock(
            &args[2],
            1 << 30,
            1,
            708,
            std::time::Duration::from_secs(15),
            Some(LifecycleLock {
                path: nitro_sandbox::lifecycle::LIFECYCLE_LOCK_PATH.into(),
                serialization: Serialization::StartAndTeardown,
            }),
        )?;
        receiver.set_restore_handler(Box::new(vmm::nitro_restore::NitroRestore {
            template: reopened,
            vcpus: 1,
            ram_mib: 1024,
            topology: b"diagnostic-no-devices".to_vec(),
            compatibility,
        }))?;
        receiver.set_console(nitro_sandbox::console::Console::new(
            Some(Box::new(std::fs::File::create(
                "shared-restore-console.log",
            )?)),
            1024 * 1024,
            1024 * 1024,
        ));
        if receiver.run()? != Exit::PowerOff {
            return Err("restored runtime did not power off".into());
        }
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
