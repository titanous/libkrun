//! One-guest runtime shared by the product context and acceptance binaries.
use crate::host::Sandbox;
use crate::transport::Transport;
use crate::{Action, Watchdog};
use devices::virtio::{VirtioDevice, VmmExitObserver};
use polly::event_manager::{EventManager, Subscriber};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct TerminalFacts {
    /// Firmware power-off on a guest without the typed stop-reason device.
    pub legacy_poweroff: bool,
    /// Successful source capture, not a guest stop; set only after source close.
    pub frozen: bool,
    pub guest_fault: bool,
    pub host_stop: bool,
    pub liveness_expired: bool,
    pub device_expired: bool,
    pub reason: u8,
    pub observed_micros: Option<u64>,
    pub exit_code: Option<i32>,
}

#[cfg(test)]
#[test]
fn reason_observation_honors_clear_and_sticky_panic() {
    let mut facts = TerminalFacts::default();
    facts.observe_reason(2, Some(10));
    assert_eq!((facts.reason, facts.observed_micros), (2, Some(10)));
    facts.observe_reason(0, Some(20));
    assert_eq!((facts.reason, facts.observed_micros), (0, None));
    facts.observe_reason(4, Some(30));
    facts.observe_reason(0, Some(40));
    facts.observe_reason(1, Some(50));
    assert_eq!((facts.reason, facts.observed_micros), (4, Some(30)));
}

impl TerminalFacts {
    fn observe_reason(&mut self, reason: u8, time: Option<u64>) {
        if reason > 4 || self.reason == 4 {
            return;
        }
        if reason != self.reason {
            self.reason = reason;
            self.observed_micros = if reason == 0 { None } else { time };
        }
    }
}

#[derive(Default)]
struct ExitObservers(Mutex<Vec<Arc<Mutex<dyn VmmExitObserver>>>>);
impl ExitObservers {
    fn register(&self, observer: Arc<Mutex<dyn VmmExitObserver>>) {
        self.0.lock().unwrap().push(observer);
    }
    fn notify(&mut self) {
        for observer in self
            .0
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .on_vmm_exit();
            }));
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
    stop_reason: Option<crate::stop_reason::StopReason>,
    exit_code: Arc<std::sync::atomic::AtomicI32>,
    observation: Arc<Mutex<Option<u64>>>,
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
        Self::create(image, memory, vcpus, cid, timeout, lock, None)
    }
    /// As `with_lifecycle_lock`, sending the duplicate VM descriptor over `handoff` right
    /// after the create ioctl (see `Sandbox::create`).
    pub fn create(
        image: &str,
        memory: usize,
        vcpus: u32,
        cid: u64,
        timeout: Duration,
        lock: Option<crate::lifecycle::LifecycleLock>,
        handoff: Option<&std::os::unix::net::UnixStream>,
    ) -> io::Result<Self> {
        let mut sandbox = Sandbox::create(memory, vcpus, None, lock, handoff)?;
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
            stop_reason: None,
            exit_code: Arc::new(std::sync::atomic::AtomicI32::new(i32::MAX)),
            observation: Arc::new(Mutex::new(None)),
        })
    }
    pub fn duplicate_vm_descriptor(&self) -> io::Result<std::os::fd::OwnedFd> {
        self.sandbox.duplicate_vm_descriptor()
    }
    pub fn enable_stop_reason(&mut self) -> io::Result<()> {
        self.stop_reason = Some(crate::stop_reason::StopReason::register(&mut self.sandbox)?);
        Ok(())
    }
    pub fn heartbeat_control(&self) -> Option<crate::heartbeat::DeadlineControl> {
        self.heartbeat.as_ref().map(|heartbeat| heartbeat.control())
    }
    pub fn set_heartbeat(&mut self, heartbeat: crate::heartbeat::Heartbeat) {
        self.heartbeat = Some(heartbeat);
    }
    pub fn set_console(&mut self, console: crate::console::Console) {
        self.console = console;
    }
    pub fn set_stop_event(&mut self, stop: Arc<utils::eventfd::EventFd>) {
        self.stop = stop;
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
    pub fn add_filesystem(
        &mut self,
        device: devices::virtio::Fs,
        dax_size: Option<usize>,
    ) -> io::Result<()> {
        if dax_size.is_some_and(|size| size != 0) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "virtio-fs DAX windows are not supported on Nitro",
            ));
        }
        self.transports.push(Transport::register(
            &self.sandbox,
            Arc::new(Mutex::new(device)),
        )?);
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
    fn wait_events(&self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let event = |fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut descriptors = vec![
            event(self.sandbox.vm_fd()),
            event(self.stop.as_raw_fd()),
            event(self.manager.as_raw_fd()),
        ];
        for transport in &self.transports {
            descriptors.push(event(transport.info_fd()));
        }
        if let Some(reason) = &self.stop_reason {
            descriptors.push(event(reason.fd()));
        }
        if let Some(heartbeat) = &self.heartbeat {
            descriptors.extend(heartbeat.poll_fds().into_iter().map(event));
        }
        // Console has no notification descriptor. Bound diagnostic/panic and
        // deadline observation to 50ms without polling devices every millisecond.
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 50) };
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
        for (index, transport) in self.transports.iter().enumerate() {
            if descriptors[3 + index].revents & libc::POLLIN != 0 {
                transport.consume_info_event()?;
            }
        }
        if let Some(reason) = &self.stop_reason {
            if descriptors[3 + self.transports.len()].revents & libc::POLLIN != 0 {
                reason.consume_notification()?;
            }
        }
        Ok(())
    }

    pub fn observation_state(&self) -> Arc<Mutex<Option<u64>>> {
        self.observation.clone()
    }
    pub fn exit_code_state(&self) -> Arc<std::sync::atomic::AtomicI32> {
        self.exit_code.clone()
    }
    pub fn run_facts(mut self) -> io::Result<TerminalFacts> {
        let result = self
            .run_inner()
            .or_else(|error| guest_error_facts(error, &self.stop))
            .map(|mut facts| {
                let code = self.exit_code.load(std::sync::atomic::Ordering::Acquire);
                facts.exit_code = (code != i32::MAX).then_some(code);
                facts
            });
        let cleanup = self.shutdown();
        match (result, cleanup) {
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(exit), Ok(())) => Ok(exit),
        }
    }
    fn run_inner(&mut self) -> io::Result<TerminalFacts> {
        match self.stop.read() {
            Ok(_) => {
                return Ok(TerminalFacts {
                    host_stop: true,
                    ..Default::default()
                })
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        self.sandbox.start(self.cid)?;
        let start = Instant::now();
        if let Some(heartbeat) = &mut self.heartbeat {
            heartbeat.start(start);
        }
        let mut watchdog = Watchdog::new(Duration::ZERO, self.timeout);
        let mut active = 0usize;
        let mut facts = TerminalFacts::default();
        loop {
            if let Some(device) = &self.stop_reason {
                facts.observe_reason(device.reason().unwrap_or(0), device.observation_micros());
                *self.observation.lock().unwrap() = facts.observed_micros;
            }
            match self.stop.read() {
                Ok(_) => {
                    facts.host_stop = true;
                    return Ok(facts);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            if self.sandbox.powered_off(0)? {
                facts.legacy_poweroff = self.stop_reason.is_none();
                // The terminal status can become visible just after the loop's
                // first sample. Read the final ordered byte, including a clear.
                if let Some(device) = &self.stop_reason {
                    facts.observe_reason(device.reason().unwrap_or(0), device.observation_micros());
                    *self.observation.lock().unwrap() = facts.observed_micros;
                }
                self.sandbox.drain_console(&mut self.console)?;
                match self.stop.read() {
                    Ok(_) => facts.host_stop = true,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error),
                }
                return Ok(facts);
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
            if self
                .heartbeat
                .as_mut()
                .map(|heartbeat| heartbeat.refresh(Instant::now()))
                .transpose()?
                .unwrap_or(false)
            {
                facts.liveness_expired = true;
                return Ok(facts);
            }
            if watchdog.expired(start.elapsed()) {
                facts.device_expired = true;
                return Ok(facts);
            }
            self.wait_events()?;
            self.manager
                .run_with_timeout(0)
                .map_err(|e| io::Error::other(format!("dispatch: {e:?}")))?;
        }
    }
    fn shutdown(&mut self) -> io::Result<()> {
        let mut failure = None;
        for transport in &mut self.transports {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                transport.prepare_exit();
                transport.stop()
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failure = Some(error),
                Err(_) => failure = Some(io::Error::other("device panicked during teardown")),
            }
        }
        if let Some(error) = failure {
            self.sandbox.close_retaining_worker_mappings();
            std::mem::forget(std::mem::take(&mut self.transports));
            std::mem::forget(self.memory.take());
            return Err(error);
        }
        self.transports.clear();
        self.observers.notify();
        self.memory.take();
        Ok(())
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn guest_error_facts(
    error: io::Error,
    stop: &utils::eventfd::EventFd,
) -> io::Result<TerminalFacts> {
    if !crate::is_guest_data_error(&error) {
        return Err(error);
    }
    let host_stop = match stop.read() {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
        Err(error) => return Err(error),
    };
    Ok(TerminalFacts {
        guest_fault: true,
        host_stop,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn pending_host_stop_survives_guest_data_fault() {
        let stop = utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        stop.write(1).unwrap();
        let facts =
            super::guest_error_facts(crate::guest_data_error("bad guest data"), &stop).unwrap();
        assert!(facts.guest_fault);
        assert!(facts.host_stop);
    }
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn panicking_observer_does_not_skip_later_observers_or_double_panic() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let mut observers = ExitObservers::default();
        observers.register(Arc::new(Mutex::new(|| panic!("observer failure"))));
        observers.register(Arc::new(Mutex::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })));
        observers.notify();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn observers_notify_exactly_once_for_all_exit_results() {
        let outcomes = [
            Ok(TerminalFacts::default()),
            Ok(TerminalFacts {
                device_expired: true,
                ..Default::default()
            }),
            Ok(TerminalFacts {
                host_stop: true,
                ..Default::default()
            }),
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
