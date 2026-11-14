//! One-guest runtime shared by the product context and acceptance binaries.
use crate::host::Sandbox;
use crate::transport::Transport;
use crate::{Action, Watchdog};
use devices::virtio::{VirtioDevice, VmmExitObserver};
use polly::event_manager::{EventManager, Subscriber};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Internal one-shot template import after a parked receiver publishes READY.
pub trait RestoreHandler: Send {
    /// Validate the sealed template and perform private copy/release. No guest
    /// instructions beyond the parked stub may run before validation succeeds.
    fn restore(self: Box<Self>, sandbox: &Sandbox) -> io::Result<()>;
}

/// Why a parked receiver ended without importing a template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkedReceiverError {
    /// No template was assigned and released within the park window.
    Expired,
    /// Every assignment handle was dropped while the receiver waited.
    Abandoned,
    /// The receiver already took an assignment, expired or ended.
    Closed,
}
impl std::fmt::Display for ParkedReceiverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Expired => "parked receiver was not given a template before its wait expired",
            Self::Abandoned => "parked receiver was dropped before a template was assigned",
            Self::Closed => "parked receiver no longer accepts a template",
        })
    }
}
impl std::error::Error for ParkedReceiverError {}

struct ParkSlot {
    state: Mutex<(Option<Box<dyn RestoreHandler>>, bool)>,
    wake: utils::eventfd::EventFd,
    ready: std::sync::atomic::AtomicBool,
}

/// Hands one template import to a receiver started with `park_for_restore`.
/// Dropping every handle while the receiver waits ends it with `Abandoned`.
#[derive(Clone)]
pub struct RestoreAssignment(Arc<ParkSlot>);
impl RestoreAssignment {
    /// Accept at most one handler; refused with `ParkedReceiverError::Closed`
    /// once the receiver has taken an assignment, expired or ended.
    pub fn assign(&self, handler: Box<dyn RestoreHandler>) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        if state.1 || state.0.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                ParkedReceiverError::Closed,
            ));
        }
        state.0 = Some(handler);
        drop(state);
        self.0.wake.write(1)?;
        Ok(())
    }
    /// True once the parked receiver has published READY (observed within
    /// the runtime's 50ms event bound).
    pub fn receiver_ready(&self) -> bool {
        self.0.ready.load(std::sync::atomic::Ordering::Acquire)
    }
}

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
    marker_notified: bool,
    restore: Option<Box<dyn RestoreHandler>>,
    park: Option<(Arc<ParkSlot>, Duration)>,
    memory_size: usize,
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
        let memory_size = memory;
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
            capture: None,
            accepted_capture: None,
            marker_notified: false,
            restore: None,
            park: None,
            memory_size,
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
    pub fn set_restore_handler(&mut self, handler: Box<dyn RestoreHandler>) -> io::Result<()> {
        if self.memory_size < 1 << 30 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "parked receiver requires at least1GiB",
            ));
        }
        if self.park.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "receiver is parked for a deferred template",
            ));
        }
        self.restore = Some(handler);
        Ok(())
    }
    /// Start as a parked receiver with no template chosen. `wait`, counted from
    /// start, bounds both assignment and release; on expiry `run` returns
    /// `ParkedReceiverError::Expired` (TimedOut) after the normal teardown.
    /// The boot watchdog is suspended while parked and restarts at release.
    pub fn park_for_restore(&mut self, wait: Duration) -> io::Result<RestoreAssignment> {
        if self.memory_size < 1 << 30 || self.restore.is_some() || self.park.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "parking requires at least1GiB and no other restore",
            ));
        }
        if wait.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero park wait",
            ));
        }
        let slot = Arc::new(ParkSlot {
            state: Mutex::new((None, false)),
            wake: utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK)?,
            ready: Default::default(),
        });
        self.park = Some((slot.clone(), wait));
        Ok(RestoreAssignment(slot))
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
        if let Some((slot, _)) = &self.park {
            descriptors.push(event(slot.wake.as_raw_fd()));
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
            // Reservation publication precedes the final boot marker. A missing
            // reservation disables capture, not the running source guest.
            if discovery.marker.is_some() {
                self.capture.as_mut().unwrap().marker_confirmed(false);
                self.capture = None;
            }
            return Ok(false);
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
        if !self.marker_notified {
            if let Some(active) = discovery.marker {
                // SAFETY: the entire aligned control page was validated above.
                let confirmed = unsafe { pointer.add(32).read_volatile() } == 1;
                if confirmed != active {
                    return Err(io::Error::other(
                        "template marker notification disagrees with control page",
                    ));
                }
                self.capture.as_mut().unwrap().marker_confirmed(active);
                self.marker_notified = true;
            }
        }
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
        let receiver_start = Instant::now();
        self.sandbox.start(self.cid)?;
        let start = Instant::now();
        if let Some(heartbeat) = &mut self.heartbeat {
            heartbeat.start(start);
        }
        let mut watchdog = Watchdog::new(Duration::ZERO, self.timeout);
        let mut active = 0usize;
        let mut stated = None;
        let mut parked = self.park.is_some();
        loop {
            if let Some((slot, wait)) = self.park.take() {
                let expired = start.elapsed() >= wait;
                let abandoned = Arc::strong_count(&slot) == 1;
                let address = self.sandbox.ram.address() + crate::receiver::CONTROL_OFFSET as u64;
                // SAFETY: parking requires >=1GiB; the aligned control page lies in
                // the receiver reservation inside the live RAM mapping.
                if unsafe { (address as *const u64).read_volatile() } == crate::receiver::READY {
                    slot.ready.store(true, std::sync::atomic::Ordering::Release);
                }
                let mut state = slot.state.lock().unwrap();
                let _ = slot.wake.read();
                if self.restore.is_none() {
                    if let Some(handler) = state.0.take() {
                        state.1 = true;
                        self.restore = Some(handler);
                    }
                }
                if self.restore.is_none() && abandoned {
                    state.1 = true;
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        ParkedReceiverError::Abandoned,
                    ));
                }
                if expired {
                    state.1 = true;
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        ParkedReceiverError::Expired,
                    ));
                }
                drop(state);
                self.park = Some((slot, wait));
            }
            if self.restore.is_some() {
                let address = self.sandbox.ram.address() + crate::receiver::CONTROL_OFFSET as u64;
                // SAFETY: restore is installed only for >=1GiB parked receiver
                // shapes; the aligned control page is inside its reservation.
                if unsafe { (address as *const u64).read_volatile() } == crate::receiver::READY {
                    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                    if std::env::var_os("NITRO_COPY_TIMINGS").is_some() {
                        eprintln!(
                            "NITRO_RECEIVER_TIMING start_to_ready_us={}",
                            receiver_start.elapsed().as_micros()
                        );
                    }
                    self.restore.take().unwrap().restore(&self.sandbox)?;
                    if parked {
                        parked = false;
                        self.park = None;
                        watchdog = Watchdog::new(start.elapsed(), self.timeout);
                    }
                    if std::env::var_os("NITRO_COPY_TIMINGS").is_some() {
                        eprintln!(
                            "NITRO_RELEASE_TIMING host_ns={}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_err(io::Error::other)?
                                .as_nanos()
                        );
                    }
                }
            }
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
            if stated.is_none() && !parked && watchdog.expired(start.elapsed()) {
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

    struct Unused;
    impl RestoreHandler for Unused {
        fn restore(self: Box<Self>, _: &Sandbox) -> io::Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn parked_receiver_accepts_one_assignment_then_refuses_typed() {
        let slot = Arc::new(ParkSlot {
            state: Mutex::new((None, false)),
            wake: utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            ready: Default::default(),
        });
        let handle = RestoreAssignment(slot.clone());
        handle.assign(Box::new(Unused)).unwrap();
        assert_eq!(slot.wake.read().unwrap(), 1);
        let refused = handle.assign(Box::new(Unused)).unwrap_err();
        assert_eq!(
            refused
                .get_ref()
                .unwrap()
                .downcast_ref::<ParkedReceiverError>(),
            Some(&ParkedReceiverError::Closed)
        );
        // Once the runtime closes the slot (taken, expired or ended) nothing is accepted.
        let closed = RestoreAssignment(Arc::new(ParkSlot {
            state: Mutex::new((None, true)),
            wake: utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            ready: Default::default(),
        }));
        assert!(closed.assign(Box::new(Unused)).is_err());
    }

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
