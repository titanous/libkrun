//! One-guest runtime shared by the product context and acceptance binaries.
use crate::host::Sandbox;
use crate::transport::Transport;
use crate::{Action, Watchdog};
use devices::virtio::{VirtioDevice, VmmExitObserver};
use polly::event_manager::{EventManager, Subscriber};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    PowerOff,
    Watchdog,
    Stopped,
    Panic,
    LivenessExpired,
}

#[derive(Default)]
struct ExitObservers(Mutex<Vec<Arc<Mutex<dyn VmmExitObserver>>>>);
impl ExitObservers {
    fn register(&self, observer: Arc<Mutex<dyn VmmExitObserver>>) {
        self.0.lock().unwrap().push(observer);
    }
    fn notify(&mut self) {
        for observer in self.0.get_mut().unwrap().drain(..) {
            observer.lock().unwrap().on_vmm_exit();
        }
    }
}
impl Drop for ExitObservers {
    fn drop(&mut self) {
        self.notify();
    }
}

pub struct Runtime {
    // Explicit cleanup in run/Drop stops workers before mappings disappear.
    transports: Vec<Transport>,
    manager: EventManager,
    memory: Option<vm_memory::GuestMemoryMmap>,
    sandbox: Sandbox,
    timeout: Duration,
    cid: u64,
    observers: ExitObservers,
    stop: Arc<utils::eventfd::EventFd>,
    console: crate::console::Console,
    heartbeat: Option<crate::heartbeat::Heartbeat>,
}
impl Runtime {
    pub fn new(
        image: &str,
        memory: usize,
        vcpus: u32,
        cid: u64,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::with_lifecycle_lock(image, memory, vcpus, cid, timeout, None)
    }
    pub fn with_lifecycle_lock(
        image: &str,
        memory: usize,
        vcpus: u32,
        cid: u64,
        timeout: Duration,
        lock: Option<crate::lifecycle::LifecycleLock>,
    ) -> io::Result<Self> {
        let mut sandbox = Sandbox::with_lifecycle_lock(memory, vcpus, None, lock)?;
        sandbox.load(image)?;
        let memory = Some(unsafe { sandbox.guest_memory()? });
        let manager =
            EventManager::new().map_err(|e| io::Error::other(format!("event manager: {e:?}")))?;
        Ok(Self {
            transports: Vec::new(),
            manager,
            memory,
            sandbox,
            timeout,
            cid,
            observers: ExitObservers::default(),
            stop: Arc::new(utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK)?),
            console: Default::default(),
            heartbeat: None,
        })
    }
    pub fn set_heartbeat(&mut self, heartbeat: crate::heartbeat::Heartbeat) {
        self.heartbeat = Some(heartbeat);
    }
    pub fn set_console(&mut self, console: crate::console::Console) {
        self.console = console;
    }
    pub fn stop_event(&self) -> Arc<utils::eventfd::EventFd> {
        self.stop.clone()
    }
    pub fn register_exit_observer(&self, observer: Arc<Mutex<dyn VmmExitObserver>>) {
        self.observers.register(observer);
    }
    pub fn add_device<D: VirtioDevice + Subscriber + 'static>(
        &mut self,
        device: Arc<Mutex<D>>,
    ) -> io::Result<()> {
        self.manager
            .add_subscriber(device.clone())
            .map_err(|e| io::Error::other(format!("subscriber: {e:?}")))?;
        self.transports
            .push(Transport::register(&self.sandbox, device)?);
        Ok(())
    }
    pub fn add_worker_device<D: VirtioDevice + 'static>(
        &mut self,
        device: Arc<Mutex<D>>,
    ) -> io::Result<()> {
        self.transports
            .push(Transport::register(&self.sandbox, device)?);
        Ok(())
    }
    pub fn run(mut self) -> io::Result<Exit> {
        self.sandbox.start(self.cid)?;
        let start = Instant::now();
        if let Some(heartbeat) = &mut self.heartbeat {
            heartbeat.start(start);
        }
        let mut watchdog = Watchdog::new(Duration::ZERO, self.timeout);
        let mut active = 0usize;
        loop {
            match self.stop.read() {
                Ok(_) => return Ok(Exit::Stopped),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            if self.sandbox.powered_off(0)? {
                self.sandbox.drain_console(&mut self.console)?;
                return Ok(if self.console.panic_seen() {
                    Exit::Panic
                } else {
                    Exit::PowerOff
                });
            }
            for transport in &mut self.transports {
                match transport.refresh(&self.sandbox, self.memory.as_ref().unwrap())? {
                    Action::Start => {
                        active += 1;
                    }
                    Action::Stop => {
                        active = active.saturating_sub(1);
                        watchdog.devices_reset(start.elapsed());
                    }
                    Action::None => {}
                }
            }
            if !self.transports.is_empty() && active == self.transports.len() {
                watchdog.devices_ready();
            }
            self.sandbox.drain_console(&mut self.console)?;
            if self.console.panic_seen() {
                return Ok(Exit::Panic);
            }
            if self
                .heartbeat
                .as_mut()
                .map(|heartbeat| heartbeat.refresh(Instant::now()))
                .transpose()?
                .unwrap_or(false)
            {
                return Ok(Exit::LivenessExpired);
            }
            if watchdog.expired(start.elapsed()) {
                return Ok(Exit::Watchdog);
            }
            self.manager
                .run_with_timeout(1)
                .map_err(|e| io::Error::other(format!("dispatch: {e:?}")))?;
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        for t in &mut self.transports {
            t.prepare_exit();
            if t.stop().is_err() {
                std::process::abort();
            }
        }
        self.transports.clear();
        // Workers no longer touch guest queues or backend state. Notify on every
        // exit path, including failed start and device-dispatch errors.
        self.observers.notify();
        self.memory.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn observers_notify_exactly_once_for_all_exit_results() {
        let outcomes = [
            Ok(Exit::PowerOff),
            Ok(Exit::Watchdog),
            Ok(Exit::Stopped),
            Err(io::Error::other("device error")),
        ];
        for outcome in outcomes {
            let calls = Arc::new(AtomicUsize::new(0));
            let counted = calls.clone();
            let run = || {
                let observers = ExitObservers::default();
                observers.register(Arc::new(Mutex::new(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                })));
                outcome
            };
            let _ = run();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}
