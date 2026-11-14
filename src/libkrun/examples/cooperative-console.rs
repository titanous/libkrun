//! Minimal separate-process cooperative freeze/clone proof launcher.
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: cooperative-console freeze|resume TEMPLATE CONTROL_GPA_HEX".into());
    }
    let control = u64::from_str_radix(args[3].trim_start_matches("0x"), 16)?;
    let mut builder = krun::Builder::new();
    builder.vm_config(1, 256)?;
    builder.set_kernel_cmdline(vec![
        "console=ttyS0",
        "earlycon=uart,io,0x3f8,115200",
        "rdinit=/init",
        "panic=-1",
        "reboot=k",
        "nokaslr",
        "initcall_debug",
        "pm_print_times",
    ]);
    if args[1] == "freeze" {
        builder.capture_cooperative_template(control, PathBuf::from(&args[2]));
    } else if args[1] == "resume" {
        builder.start_from_template_path(&args[2])?;
    } else {
        return Err("invalid mode".into());
    }
    let result = builder.build()?.run()?;
    eprintln!("COOPERATIVE HOST result={result:?}");
    match (args[1].as_str(), result) {
        ("freeze", krun::VmExit::Frozen { .. }) => Ok(()),
        ("resume", krun::VmExit::Shutdown { exit_code: 0 }) => Ok(()),
        _ => Err("unexpected VM exit".into()),
    }
}
