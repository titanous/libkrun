//! Driver-reset policy and public-control regression for real Nitro hosts.
#[path = "../tests/embed_backends.rs"]
mod backends;
use krun::{AllowlistPolicy, BlockBackend, Builder, NitroBackendConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
struct Disk(backends::MemoryDisk, AtomicUsize);
impl BlockBackend for Disk {
    fn cache_type(&self) -> krun::CacheType {
        krun::CacheType::Writeback
    }
    fn nsectors(&self) -> u64 {
        self.0.nsectors()
    }
    fn image_id(&self) -> &[u8] {
        b"nitro-reset"
    }
    fn read_vectored_at(&self, b: &[krun::VolatileSlice], o: u64) -> std::io::Result<usize> {
        self.0.read_vectored_at(b, o)
    }
    fn write_vectored_at(&self, b: &[krun::VolatileSlice], o: u64) -> std::io::Result<usize> {
        self.0.write_vectored_at(b, o)
    }
    fn on_exit(&self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}
struct Observer(Arc<AtomicUsize>);
impl devices::virtio::VmmExitObserver for Observer {
    fn on_vmm_exit(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 4, "usage: nitro_reset IMAGE SOCKET deny|allow");
    let mut builder = Builder::new();
    builder.nitro_console_sink(Box::new(std::io::stdout()), 1024 * 1024, 256 * 1024);
    builder.nitro_backend(NitroBackendConfig {
        image: args[1].clone().into(),
        cid: 210,
        watchdog: std::time::Duration::from_secs(5),
    });
    builder.disable_implicit_console()?;
    builder.vm_config(2, 1024)?;
    builder.add_vsock_port(7000, args[2].clone().into(), false);
    if args[3] == "deny" {
        builder.outbound_policy(Arc::new(AllowlistPolicy::new([])));
    }
    let disk = Arc::new(Disk(
        backends::MemoryDisk(Mutex::new(vec![0; 1024 * 1024])),
        AtomicUsize::new(0),
    ));
    builder.add_block_backend("reset".into(), disk.clone(), true);
    let context = builder.build()?;
    assert_eq!(context.device_info().vcpu_count, 2);
    assert_eq!(context.device_info().ram_mib, 1024);
    let handle = context.vm_handle();
    assert!(handle
        .pause()
        .unwrap_err()
        .to_string()
        .contains("unsupported on this backend"));
    assert!(handle
        .resume()
        .unwrap_err()
        .to_string()
        .contains("unsupported on this backend"));
    assert!(handle
        .notify_vm_generation_change()
        .unwrap_err()
        .to_string()
        .contains("unsupported on this backend"));
    assert!(handle.balloon().is_none());
    let observed = Arc::new(AtomicUsize::new(0));
    context.register_exit_observer(Arc::new(Mutex::new(Observer(observed.clone()))));
    println!("CONTROLS_OK");
    println!("EXIT {:?}", context.run()?);
    assert_eq!(disk.1.load(Ordering::SeqCst), 1);
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    println!("EXIT_CALLBACKS_OK");
    Ok(())
}
