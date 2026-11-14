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
    Frozen,
    StatedReason {
        code: u8,
        observed_micros: Option<u64>,
    },
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
    capture: Option<Box<dyn crate::capture::CaptureHandler>>,
    accepted_capture: Option<crate::capture::AcceptedCapture>,
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
            stop_reason: None,
            capture: None,
            accepted_capture: None,
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
    pub fn set_capture_handler(&mut self, handler: Box<dyn crate::capture::CaptureHandler>) {
        self.capture = Some(handler);
        self.console.enable_freeze_discovery();
    }
    pub fn set_console(&mut self, mut console: crate::console::Console) {
        if self.capture.is_some() {
            console.enable_freeze_discovery();
        }
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

    fn poll_capture(&mut self) -> io::Result<bool> {
        use vm_memory::{Bytes, GuestAddress, GuestMemory};
        if self.capture.is_none() {
            return Ok(false);
        }
        let Some(discovery) = self.console.freeze_discovery() else {
            return Ok(false);
        };
        let Some(control) = discovery.control else {
            return Ok(false);
        };
        if !discovery.receiver_reserved {
            return Err(io::Error::other("clone receiver reservation missing"));
        }
        // SAFETY: used only within this call; queue workers must be reset before
        // capture preparation below, and only the control word is read first.
        let memory = unsafe { self.sandbox.owned_guest_memory(&discovery.memory)? };
        if !memory.check_range(
            GuestAddress(control),
            4096,
            vm_memory::Permissions::ReadWrite,
        ) {
            return Err(io::Error::other("freeze control outside owned RAM"));
        }
        let offset = if control < 0x80000000 {
            control - 0x40000000
        } else {
            control - 0x400000000 + 0x40000000
        };
        let pointer = (self.sandbox.ram.address() + offset) as *mut u64;
        if unsafe { pointer.read_volatile() } != crate::cooperative::FROZEN_MAGIC
            || unsafe { pointer.add(1).read_volatile() } != 0
        {
            return Ok(false);
        }
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        let prepared = (|| {
            for transport in &self.transports {
                if !transport.is_reset(&self.sandbox)? {
                    return Err(io::Error::other("capture requires every device reset"));
                }
            }
            let mut page = [0; 4096];
            memory
                .read_slice(&mut page, GuestAddress(control))
                .map_err(io::Error::other)?;
            crate::cooperative::ResumeEntry::parse(&page, |gpa, len| {
                usize::try_from(len).is_ok_and(|len| {
                    memory.check_range(GuestAddress(gpa), len, vm_memory::Permissions::ReadWrite)
                })
            })?;
            self.capture.as_mut().unwrap().prepare(&memory, control)
        })();
        match prepared {
            Ok(Some(pending)) => {
                if let Some(accepted) = crate::capture::AcceptedCapture::decide(pending) {
                    self.accepted_capture = Some(accepted);
                    return Ok(true);
                }
            }
            Ok(None) => {}
            Err(error) => self.capture.as_mut().unwrap().failed(error),
        }
        // Refusal remains reversible: existing arm64 release resumes in place.
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        unsafe {
            pointer.add(1).write_volatile(1);
        }
        Ok(false)
    }

    pub fn run(mut self) -> io::Result<Exit> {
        let result = self.run_inner();
        let cleanup = self.shutdown();
        let accepted = self.accepted_capture.take();
        drop(self);
        if cleanup.is_ok() {
            if let Some(accepted) = accepted {
                accepted.source_closed();
            }
        }
        match (result, cleanup) {
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(exit), Ok(())) => Ok(exit),
        }
    }
    fn run_inner(&mut self) -> io::Result<Exit> {
        match self.stop.read() {
            Ok(_) => return Ok(Exit::Stopped),
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
        let mut stated = None;
        loop {
            if stated.is_none() {
                if let Some(device) = &self.stop_reason {
                    if let Some(code) = device.reason() {
                        stated = Some((
                            Instant::now(),
                            Exit::StatedReason {
                                code,
                                observed_micros: device.observation_micros(),
                            },
                        ));
                    }
                }
            }
            // Intent is reported before the kernel flushes panic/shutdown output.
            // Continue servicing devices and draining until poweroff or a bounded
            // grace period; preserve the timestamp of the first observation.
            if let Some((observed, exit)) = stated {
                if self.sandbox.powered_off(0)? || observed.elapsed() >= Duration::from_secs(1) {
                    self.sandbox.drain_console(&mut self.console)?;
                    return Ok(exit);
                }
            }
            match self.stop.read() {
                Ok(_) => return Ok(Exit::Stopped),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            if self.sandbox.powered_off(0)? {
                self.sandbox.drain_console(&mut self.console)?;
                if let Some((_, exit)) = stated {
                    return Ok(exit);
                }
                if let Some(device) = &self.stop_reason {
                    if let Some(code) = device.reason() {
                        let observed_micros = device.observation_micros();
                        return Ok(Exit::StatedReason {
                            code,
                            observed_micros,
                        });
                    }
                    return Err(io::Error::other(
                        "Nitro guest stopped without stating a terminal reason",
                    ));
                }
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
            if self.poll_capture()? {
                return Ok(Exit::Frozen);
            }
            if self.stop_reason.is_none() && self.console.panic_seen() {
                return Ok(Exit::Panic);
            }
            if stated.is_none()
                && self
                    .heartbeat
                    .as_mut()
                    .map(|heartbeat| heartbeat.refresh(Instant::now()))
                    .transpose()?
                    .unwrap_or(false)
            {
                return Ok(Exit::LivenessExpired);
            }
            if stated.is_none() && watchdog.expired(start.elapsed()) {
                return Ok(Exit::Watchdog);
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

#[cfg(test)]
mod tests {
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
