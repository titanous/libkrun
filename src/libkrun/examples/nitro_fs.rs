//! External-kernel packaging timings and caller-supplied filesystem smoke.
#[allow(dead_code)]
#[path = "../tests/embed_backends.rs"]
mod backends;
#[allow(dead_code)]
mod nitro_memory_fs;
use krun::{Builder, NitroBackendConfig};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 4, "usage: nitro_fs Image INITRAMFS none|dax");
    let trials: usize = std::env::var("NITRO_FS_TRIALS")
        .unwrap_or_else(|_| "2".into())
        .parse()?;
    for trial in 0..trials {
        let mut builder = Builder::new();
        builder.nitro_backend(NitroBackendConfig {
            image: "unused.eif".into(),
            cid: 214,
            watchdog: std::time::Duration::from_secs(5),
        });
        builder.disable_implicit_console()?;
        builder.vm_config(1, 1024)?;
        builder.external_kernel(
            args[1].clone().into(),
            krun::KernelFormat::Raw,
            Some(args[2].clone().into()),
        )?;
        builder.set_kernel_cmdline(vec!["console=hvc0", "quiet", "panic=-1", "rdinit=/init"]);
        let fs: Box<dyn krun::FileSystem + Send + Sync> = if args[3].starts_with("extended") {
            Box::new(nitro_memory_fs::Tree::default())
        } else {
            Box::new(backends::ReadOnlyTree)
        };
        builder.add_virtiofs(
            "testfs",
            fs,
            args[3].contains("dax").then_some(64 * 1024 * 1024),
        );
        let captured = Arc::new(Mutex::new(Vec::new()));
        builder.nitro_console_sink(Box::new(Sink(captured.clone())), 1024 * 1024, 256 * 1024);
        let start = std::time::Instant::now();
        let context = builder.build()?;
        let build_ms = start.elapsed().as_secs_f64() * 1000.;
        let exit = context.run()?;
        let text = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
        assert!(
            text.contains("NITRO_FS_OK") && !text.contains("FS_FAIL"),
            "guest failed: {text}"
        );
        if args[3].contains("dax") {
            assert!(text.contains("NITRO_DAX_OK"));
        }
        if args[3].starts_with("extended") {
            assert!(text.contains("NITRO_FS_EXTENDED_OK"));
        }
        println!(
            "NITRO_FS_PASS trial={trial} build_ms={build_ms:.3} total_ms={:.3} exit={exit:?}",
            start.elapsed().as_secs_f64() * 1000.
        );
    }
    Ok(())
}
