#[cfg(all(target_os = "linux", feature = "devices"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nitro_sandbox::{
        capture::{CaptureHandler, PreparedCapture},
        lifecycle::{LifecycleLock, Serialization},
        runtime::{Exit, Runtime},
    };
    use std::{
        io,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};
    struct Handler(Arc<Mutex<Option<usize>>>);
    struct Pending {
        bytes: Vec<Vec<u8>>,
        result: Arc<Mutex<Option<usize>>>,
    }
    impl PreparedCapture for Pending {
        fn accept(&mut self) -> bool {
            true
        }
        fn complete(self: Box<Self>) {
            *self.result.lock().unwrap() = Some(self.bytes.iter().map(Vec::len).sum());
        }
    }
    impl CaptureHandler for Handler {
        fn prepare(
            &mut self,
            memory: &vm_memory::GuestMemoryMmap,
            _: u64,
        ) -> io::Result<Option<Box<dyn PreparedCapture>>> {
            let mut bytes = Vec::new();
            for region in memory.iter() {
                let mut copy = vec![0; region.len() as usize];
                memory
                    .read_slice(&mut copy, GuestAddress(region.start_addr().raw_value()))
                    .map_err(io::Error::other)?;
                bytes.push(copy);
            }
            Ok(Some(Box::new(Pending {
                bytes,
                result: self.0.clone(),
            })))
        }
        fn failed(&mut self, error: io::Error) {
            eprintln!("CAPTURE_FAILED {error}");
        }
    }
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 2 {
        return Err("usage: runtime-capture source.eif".into());
    }
    let result = Arc::new(Mutex::new(None));
    let mut runtime = Runtime::with_lifecycle_lock(
        &args[1],
        1 << 30,
        1,
        706,
        Duration::from_secs(15),
        Some(LifecycleLock {
            path: "/run/holodeck/nitro-lifecycle.lock".into(),
            serialization: Serialization::StartAndTeardown,
        }),
    )?;
    runtime.set_console(nitro_sandbox::console::Console::new(
        Some(Box::new(std::fs::File::create("runtime-console.log")?)),
        1024 * 1024,
        1024 * 1024,
    ));
    runtime.set_capture_handler(Box::new(Handler(result.clone())));
    if runtime.run()? != Exit::Frozen {
        return Err("expected frozen exit".into());
    }
    let bytes = result
        .lock()
        .unwrap()
        .ok_or("completion missing after run")?;
    println!("RUNTIME_CAPTURE_COMPLETE bytes={bytes}");
    Ok(())
}
#[cfg(not(all(target_os = "linux", feature = "devices")))]
fn main() {
    panic!("requires Linux/devices");
}
