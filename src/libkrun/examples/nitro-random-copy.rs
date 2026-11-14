//! One source freeze and N copies for the random-bytes-after-resume test (guest:
//! libkrun tests/cooperative/random.c, analysis: tests/cooperative/random-run.py --nitro-results).
//!
//! usage: nitro-random-copy SOURCE.eif RECEIVER.eif OUTDIR LABEL CPUS COPIES SOURCE_CID
//! Writes OUTDIR/LABEL.source.log, OUTDIR/LABEL.ram (exported template) and OUTDIR/LABEL-copyN.log.
//! Copies use CIDs SOURCE_CID+1 .. SOURCE_CID+COPIES and run concurrently. Every guest start and
//! teardown takes the host-wide lifecycle lock.
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
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    let args: Vec<_> = std::env::args().collect();
    if args.len() != 8 {
        return Err(
            "usage: nitro-random-copy SOURCE.eif RECEIVER.eif OUTDIR LABEL CPUS COPIES SOURCE_CID"
                .into(),
        );
    }
    let (source_eif, receiver_eif) = (args[1].clone(), args[2].clone());
    let outdir = PathBuf::from(&args[3]);
    let label = args[4].clone();
    let cpus: u32 = args[5].parse()?;
    let copies: u64 = args[6].parse()?;
    let source_cid: u64 = args[7].parse()?;
    if !(2100..=2299).contains(&source_cid) || source_cid + copies > 2299 {
        return Err("CIDs must stay within 2100-2299".into());
    }
    let lock = || {
        Some(LifecycleLock {
            path: nitro_sandbox::lifecycle::LIFECYCLE_LOCK_PATH.into(),
            serialization: Serialization::StartAndTeardown,
        })
    };
    let memory = 1usize << 30;
    let request = vmm::capture_request::CaptureRequest::default();
    let mut runtime = Runtime::with_lifecycle_lock(
        &source_eif,
        memory,
        cpus,
        source_cid,
        Duration::from_secs(30),
        lock(),
    )?;
    let template_path = outdir.join(format!("{label}.ram"));
    runtime.set_capture_handler(Box::new(vmm::nitro_capture::NitroCapture {
        request: request.clone(),
        vcpus: cpus as u8,
        ram_mib: 1024,
        topology: b"diagnostic-no-devices".to_vec(),
        compatibility: b"nitro-arm64-v1".to_vec(),
        export_path: Some(template_path.clone()),
    }));
    runtime.set_console(nitro_sandbox::console::Console::new(
        Some(Box::new(std::fs::File::create(
            outdir.join(format!("{label}.source.log")),
        )?)),
        1024 * 1024,
        1024 * 1024,
    ));
    // Arm only after the runtime confirmed the guest's boot marker.
    let arm_request = request.clone();
    let armer = std::thread::spawn(move || -> std::io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while arm_request.require_marker().is_err() {
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "boot marker never confirmed",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        arm_request.arm(Duration::from_secs(30))
    });
    let exit = runtime.run()?;
    armer.join().map_err(|_| "arming thread panicked")??;
    if exit != Exit::Frozen {
        return Err(format!("source did not freeze: {exit:?}").into());
    }
    let outcome = request.wait()?;
    if let Some(export) = outcome.export {
        export?;
    } else {
        return Err("template was not exported".into());
    }
    let workers: Vec<_> = (0..copies)
        .map(|index| {
            let file = outcome.template.file.try_clone();
            let receiver_eif = receiver_eif.clone();
            let console = outdir.join(format!("{label}-copy{index}.log"));
            std::thread::spawn(move || -> Result<(), String> {
                let template = vmm::template::Template::open(file.map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
                let compatibility = template.compatibility.clone();
                let mut receiver = Runtime::with_lifecycle_lock(
                    &receiver_eif,
                    memory,
                    cpus,
                    source_cid + 1 + index,
                    Duration::from_secs(30),
                    lock(),
                )
                .map_err(|e| e.to_string())?;
                receiver
                    .set_restore_handler(Box::new(vmm::nitro_restore::NitroRestore {
                        template,
                        vcpus: cpus as u8,
                        ram_mib: 1024,
                        topology: b"diagnostic-no-devices".to_vec(),
                        compatibility,
                    }))
                    .map_err(|e| e.to_string())?;
                receiver.set_console(nitro_sandbox::console::Console::new(
                    Some(Box::new(
                        std::fs::File::create(&console).map_err(|e| e.to_string())?,
                    )),
                    1024 * 1024,
                    1024 * 1024,
                ));
                match receiver.run().map_err(|e| e.to_string())? {
                    Exit::PowerOff => Ok(()),
                    other => Err(format!("copy {index} ended {other:?}")),
                }
            })
        })
        .collect();
    let mut failed = Vec::new();
    for worker in workers {
        if let Err(e) = worker
            .join()
            .map_err(|_| "copy thread panicked".to_string())
            .and_then(|r| r)
        {
            failed.push(e);
        }
    }
    if !failed.is_empty() {
        return Err(failed.join("; ").into());
    }
    println!("RANDOM_COPY_DONE label={label} cpus={cpus} copies={copies}");
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
