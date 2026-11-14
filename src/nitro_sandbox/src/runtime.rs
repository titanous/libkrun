//! One-guest runtime shared by the product context and acceptance binaries.
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use devices::virtio::VirtioDevice;
use polly::event_manager::{EventManager, Subscriber};
use crate::{Action, Watchdog};
use crate::host::Sandbox;
use crate::transport::Transport;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit { PowerOff, Watchdog }

pub struct Runtime {
    // Explicit cleanup in run/Drop stops workers before mappings disappear.
    transports: Vec<Transport>,
    manager: EventManager,
    memory: Option<vm_memory::GuestMemoryMmap>,
    sandbox: Sandbox,
    timeout: Duration,
    cid: u64,
}
impl Runtime {
    pub fn new(image: &str, memory: usize, vcpus: u32, cid: u64, timeout: Duration) -> io::Result<Self> {
        let mut sandbox = Sandbox::new(memory, vcpus)?;
        sandbox.load(image)?;
        let memory = Some(unsafe { sandbox.guest_memory()? });
        let manager = EventManager::new().map_err(|e| io::Error::other(format!("event manager: {e:?}")))?;
        Ok(Self { transports: Vec::new(), manager, memory, sandbox, timeout, cid })
    }
    pub fn add_device<D: VirtioDevice + Subscriber + 'static>(&mut self, device: Arc<Mutex<D>>) -> io::Result<()> {
        self.manager.add_subscriber(device.clone()).map_err(|e| io::Error::other(format!("subscriber: {e:?}")))?;
        self.transports.push(Transport::register(&self.sandbox, device)?);
        Ok(())
    }
    pub fn add_worker_device<D: VirtioDevice + 'static>(&mut self, device: Arc<Mutex<D>>) -> io::Result<()> {
        self.transports.push(Transport::register(&self.sandbox, device)?);
        Ok(())
    }
    pub fn run(mut self) -> io::Result<Exit> {
        self.sandbox.start(self.cid)?;
        let start = Instant::now();
        let mut watchdog = Watchdog::new(Duration::ZERO, self.timeout);
        let mut active = 0usize;
        loop {
            if self.sandbox.powered_off(0)? { return Ok(Exit::PowerOff); }
            for transport in &mut self.transports {
                match transport.refresh(&self.sandbox, self.memory.as_ref().unwrap())? {
                    Action::Start => { active += 1; watchdog.devices_ready(); }
                    Action::Stop => { active = active.saturating_sub(1); if active == 0 { watchdog.devices_reset(start.elapsed()); } }
                    Action::None => {}
                }
            }
            self.sandbox.drain_console(&mut io::stdout())?;
            if watchdog.expired(start.elapsed()) { return Ok(Exit::Watchdog); }
            self.manager.run_with_timeout(1).map_err(|e| io::Error::other(format!("dispatch: {e:?}")))?;
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        for t in &mut self.transports { if t.stop().is_err() { std::process::abort(); } }
        self.transports.clear();
        self.memory.take();
    }
}
