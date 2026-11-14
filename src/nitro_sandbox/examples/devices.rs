use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use devices::virtio::{Block, CacheType, Console, PortDescription, Vsock};
use devices::virtio::block::{BlockDeviceType, ImageType, SyncMode};
use devices::virtio::vsock::TsiFlags;
use nitro_sandbox::{host::Sandbox, transport::Transport, Action};
use polly::event_manager::EventManager;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 { return Err("usage: devices EIF DISK UNIX_SOCKET".into()); }
    let mut sandbox = Sandbox::new(1024 * 1024 * 1024, 2)?;
    sandbox.load(&args[1])?;
    let memory = unsafe { sandbox.guest_memory()? };
    let mut manager = EventManager::new().map_err(|e| format!("event manager: {e:?}"))?;
    let block = Arc::new(Mutex::new(Block::new("disk".into(), None, CacheType::Writeback,
        BlockDeviceType::Image { path: args[2].clone(), format: ImageType::Raw, sync_mode: SyncMode::Full }, false, false)?));
    let ports = HashMap::from([(7000u32, (args[3].clone().into(), false))]);
    let vsock = Arc::new(Mutex::new(Vsock::new(200, None, Some(ports), TsiFlags::empty()).map_err(|e| format!("vsock: {e:?}"))?));
    let console = Arc::new(Mutex::new(Console::new(vec![PortDescription::output_pipe("smoke", devices::virtio::port_io::stdout()?)]).map_err(|e| format!("console: {e:?}"))?));
    manager.add_subscriber(vsock.clone()).map_err(|e| format!("vsock subscriber: {e:?}"))?;
    manager.add_subscriber(console.clone()).map_err(|e| format!("console subscriber: {e:?}"))?;
    let mut transports = vec![Transport::register(&sandbox, block)?, Transport::register(&sandbox, vsock)?, Transport::register(&sandbox, console)?];
    sandbox.start(200)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let result = loop {
        if sandbox.powered_off(0)? { break Ok(()); }
        for t in &mut transports {
            match t.refresh(&sandbox, &memory)? {
                Action::None => (),
                action => eprintln!("DEVICE {action:?}"),
            }
        }
        sandbox.drain_console(&mut std::io::stdout())?;
        manager.run_with_timeout(1).map_err(|e| format!("event dispatch: {e:?}"))?;
        if Instant::now() >= deadline { break Err("guest watchdog expired"); }
    };
    for t in &mut transports { t.stop()?; }
    drop(transports);
    drop(manager);
    drop(memory);
    result.map_err(Into::into)
}
