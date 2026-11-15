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
    /// Refused with `ParkedReceiverError::Closed` if the receiver has already
    /// taken an assignment, expired or ended (the same check `assign` makes).
    pub fn check_open(&self) -> io::Result<()> {
        let state = self.0.state.lock().unwrap();
        if state.1 || state.0.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                ParkedReceiverError::Closed,
            ));
        }
        Ok(())
    }
    /// True once the parked receiver has published READY (observed within
    /// the runtime's 50ms event bound).
    pub fn receiver_ready(&self) -> bool {
        self.0.ready.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// What the run loop observed when it ended. The embedder resolves these facts
/// into one typed ending; the runtime never ends a run on a stated reason alone.
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
    for ignored in [5, 255] {
        facts.observe_reason(ignored, Some(15));
        assert_eq!((facts.reason, facts.observed_micros), (2, Some(10)));
    }
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
    /// A final reason (power-off, reboot, halt, panic) has been stated.
    fn final_reason(&self) -> bool {
        (1..=4).contains(&self.reason)
    }
}

type Observer = Arc<Mutex<dyn VmmExitObserver>>;
/// Registered observers and whether the guest has already exited. An observer
/// registered after the exit is notified at once.
#[derive(Default)]
struct ExitObservers(Mutex<(Vec<Observer>, bool)>);
impl ExitObservers {
    fn register(&self, observer: Arc<Mutex<dyn VmmExitObserver>>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.1 {
            state.0.push(observer);
            return;
        }
        drop(state);
        Self::call(&observer);
    }
    fn call(observer: &Arc<Mutex<dyn VmmExitObserver>>) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .on_vmm_exit();
        }));
    }
    fn notify(&self) {
        let observers = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            state.1 = true;
            std::mem::take(&mut state.0)
        };
        for observer in &observers {
            Self::call(observer);
        }
    }
}
impl Drop for ExitObservers {
    fn drop(&mut self) {
        self.notify();
    }
}

/// Registers exit observers on a runtime that another thread owns (a parked
/// receiver runs from build). Registration after the exit notifies at once.
#[derive(Clone)]
pub struct ExitObserverRegistry(Arc<ExitObservers>);
impl ExitObserverRegistry {
    pub fn register(&self, observer: Arc<Mutex<dyn VmmExitObserver>>) {
        self.0.register(observer);
    }
}

/// A parked receiver that is already running in its own thread. It started
/// at `Runtime::start_parked`, so its park wait, abandonment and stop are
/// enforced whether or not anyone waits for it. Dropping it unwaited stops
/// the receiver and waits for its teardown (under the configured lock).
pub struct ParkedRun {
    thread: Option<std::thread::JoinHandle<io::Result<TerminalFacts>>>,
    stop: Arc<utils::eventfd::EventFd>,
}
impl ParkedRun {
    /// Wait for the receiver to end; the same result `Runtime::run_facts` returns.
    pub fn wait(mut self) -> io::Result<TerminalFacts> {
        let thread = self.thread.take().expect("parked runtime thread");
        thread
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("parked receiver runtime panicked")))
    }
}
impl Drop for ParkedRun {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.stop.write(1);
            let _ = thread.join();
        }
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
    observers: Arc<ExitObservers>,
    /// Set when the VM was started before `run` (parked receivers start at build).
    started: Option<Instant>,
    stop: Arc<utils::eventfd::EventFd>,
    console: crate::console::Console,
    heartbeat: Option<crate::heartbeat::Heartbeat>,
    stop_reason: Option<crate::stop_reason::StopReason>,
    exit_code: Arc<std::sync::atomic::AtomicI32>,
    observation: Arc<Mutex<Option<u64>>>,
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
            observers: Default::default(),
            started: None,
            stop: Arc::new(utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK)?),
            console: Default::default(),
            heartbeat: None,
            stop_reason: None,
            exit_code: Arc::new(std::sync::atomic::AtomicI32::new(i32::MAX)),
            observation: Arc::new(Mutex::new(None)),
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
    /// Prepare a parked receiver with no template chosen. `wait`, counted from
    /// the VM start, bounds both assignment and release; on expiry the runtime
    /// returns `ParkedReceiverError::Expired` (TimedOut) after the normal
    /// teardown. The boot watchdog is suspended while parked and restarts at
    /// release. Start it with `start_parked` straight away: until it starts,
    /// the VM holds its slot and pool memory (and, with start-and-teardown
    /// serialization, the host-wide lock) and no deadline runs.
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
    /// Start a parked receiver now and keep running it in its own thread, so
    /// that it has a bounded life from this moment whatever its owner does:
    /// the VM start releases the creation lock, the park wait counts from
    /// here, and expiry, abandonment of every assignment handle or a stop
    /// request end it with teardown under the configured lock (releasing the
    /// slot and pool memory). A start failure tears the VM down here.
    pub fn start_parked(mut self) -> io::Result<ParkedRun> {
        if self.park.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "only a parked receiver starts before run",
            ));
        }
        self.sandbox.start(self.cid)?;
        self.started = Some(Instant::now());
        let stop = self.stop.clone();
        let thread = std::thread::Builder::new()
            .name(format!("nitro-parked-{}", self.cid))
            .spawn(move || self.run_facts())?;
        Ok(ParkedRun {
            thread: Some(thread),
            stop,
        })
    }
    pub fn exit_observer_registry(&self) -> ExitObserverRegistry {
        ExitObserverRegistry(self.observers.clone())
    }
    pub fn set_capture_handler(&mut self, handler: Box<dyn crate::capture::CaptureHandler>) {
        self.capture = Some(handler);
        self.console.enable_freeze_discovery();
    }
    pub fn console_dropped_counter(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.console.dropped_counter()
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

    /// Whether the host asked this run to stop, without consuming the request
    /// (the run loop consumes it and records `host_stop`).
    fn stop_pending(&self) -> io::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut descriptor = libc::pollfd {
            fd: self.stop.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(descriptor.revents & libc::POLLIN != 0)
    }
    fn observe_reason(&self, facts: &mut TerminalFacts) {
        if let Some(device) = &self.stop_reason {
            facts.observe_reason(device.reason(), device.observation_micros());
            *self.observation.lock().unwrap() = facts.observed_micros;
        }
    }
    /// The capture guard: a source whose guest stated a final reason, or whose
    /// run the host asked to stop, is never accepted or published as a template.
    fn guest_stop(
        &self,
        facts: &mut TerminalFacts,
    ) -> io::Result<Option<crate::capture::GuestStop>> {
        self.observe_reason(facts);
        let host_stop = self.stop_pending()?;
        Ok(
            (facts.final_reason() || host_stop).then_some(crate::capture::GuestStop {
                reason: facts.reason,
                host_stop,
            }),
        )
    }

    fn poll_capture(&mut self, facts: &mut TerminalFacts) -> io::Result<bool> {
        use vm_memory::{Bytes, GuestAddress, GuestMemory};
        if self.capture.is_none() {
            return Ok(false);
        }
        let Some(discovery) = self.console.freeze_discovery() else {
            return Ok(false);
        };
        if discovery.disabled {
            self.capture.as_mut().unwrap().marker_confirmed(false);
            self.capture = None;
            return Ok(false);
        }
        // Publications are provisional until the final pre-userspace marker.
        if discovery.marker.is_none() {
            return Ok(false);
        }
        let Some(control) = discovery.control else {
            self.capture.as_mut().unwrap().marker_confirmed(false);
            self.capture = None;
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
        let memory = match unsafe { self.sandbox.owned_guest_memory(&discovery.memory) } {
            Ok(memory) => memory,
            Err(_) => {
                self.capture.as_mut().unwrap().marker_confirmed(false);
                self.capture = None;
                return Ok(false);
            }
        };
        if !crate::cooperative::outside_receiver(control, 4096)
            || !memory.check_range(
                GuestAddress(control),
                4096,
                vm_memory::Permissions::ReadWrite,
            )
        {
            self.capture.as_mut().unwrap().marker_confirmed(false);
            self.capture = None;
            return Ok(false);
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
                    self.capture.as_mut().unwrap().marker_confirmed(false);
                    self.capture = None;
                    return Ok(false);
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
        let prepared = if let Some(stop) = self.guest_stop(facts)? {
            // Refused before copying anything; the source resumes in place and
            // ends through its stated reason or the host stop.
            self.capture.as_mut().unwrap().guest_stopped(stop);
            Ok(None)
        } else {
            (|| {
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
                        memory.check_range(
                            GuestAddress(gpa),
                            len,
                            vm_memory::Permissions::ReadWrite,
                        )
                    })
                })?;
                self.capture.as_mut().unwrap().prepare(&memory, control)
            })()
        };
        match prepared {
            Ok(Some(pending)) => {
                // Checked again after the (long) copy, immediately before the
                // irreversible acceptance.
                if let Some(stop) = self.guest_stop(facts)? {
                    drop(pending);
                    self.capture.as_mut().unwrap().guest_stopped(stop);
                } else if let Some(accepted) = crate::capture::AcceptedCapture::decide(pending) {
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

    pub fn observation_state(&self) -> Arc<Mutex<Option<u64>>> {
        self.observation.clone()
    }
    pub fn exit_code_state(&self) -> Arc<std::sync::atomic::AtomicI32> {
        self.exit_code.clone()
    }
    /// A capture that was armed but never accepted when the run ended: the waiter must learn,
    /// typed, that the source ended (a stated final reason or the host's stop), instead of
    /// waiting for its deadline and an untyped time-out.
    fn unanswered_capture_stop(facts: &TerminalFacts) -> Option<crate::capture::GuestStop> {
        (facts.final_reason() || facts.host_stop).then_some(crate::capture::GuestStop {
            reason: facts.reason,
            host_stop: facts.host_stop,
        })
    }
    pub fn run_facts(mut self) -> io::Result<TerminalFacts> {
        let mut result = self
            .run_inner()
            .or_else(|error| guest_error_facts(error, &self.stop))
            .map(|mut facts| {
                let code = self.exit_code.load(std::sync::atomic::Ordering::Acquire);
                facts.exit_code = (code != i32::MAX).then_some(code);
                facts
            });
        let cleanup = self.shutdown();
        let accepted = self.accepted_capture.take();
        if accepted.is_none() {
            if let (Some(capture), Ok(facts)) = (self.capture.as_mut(), &result) {
                if let Some(stop) = Self::unanswered_capture_stop(facts) {
                    capture.guest_stopped(stop);
                }
            }
        }
        // Recheck the capture guard after the source's devices stopped and
        // before the source is closed and the template published.
        let mut late = None;
        if accepted.is_some() {
            if let Ok(facts) = &mut result {
                match self.guest_stop(facts) {
                    Ok(stop) => late = stop,
                    Err(error) => result = Err(error),
                }
            }
        }
        drop(self);
        if let (Ok(()), Some(accepted)) = (&cleanup, accepted) {
            match (&mut result, late) {
                (Ok(facts), None) => {
                    accepted.source_closed();
                    facts.frozen = true;
                }
                (Ok(facts), Some(stop)) => {
                    facts.host_stop |= stop.host_stop;
                    accepted.reject(stop);
                }
                // An error after acceptance: dropping the decision leaves the
                // waiter to its "timed out after acceptance" result.
                (Err(_), _) => drop(accepted),
            }
        }
        match (result, cleanup) {
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(facts), Ok(())) => Ok(facts),
        }
    }
    fn run_inner(&mut self) -> io::Result<TerminalFacts> {
        let (receiver_start, start) = match self.started {
            Some(started) => (started, started),
            None => {
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
                let receiver_start = Instant::now();
                self.sandbox.start(self.cid)?;
                (receiver_start, Instant::now())
            }
        };
        if let Some(heartbeat) = &mut self.heartbeat {
            heartbeat.start(start);
        }
        let mut watchdog = Watchdog::new(Duration::ZERO, self.timeout);
        let mut active = 0usize;
        let mut facts = TerminalFacts::default();
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
                        if let Some(heartbeat) = &mut self.heartbeat {
                            heartbeat.start(Instant::now());
                        }
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
            // A stated reason is recorded but never ends the run by itself: the
            // run ends on power-off, a host stop, liveness or device expiry.
            self.observe_reason(&mut facts);
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
                self.observe_reason(&mut facts);
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
            // `frozen` is set by `run_facts` only after the source has closed.
            if self.poll_capture(&mut facts)? {
                return Ok(facts);
            }
            // Liveness and the device watchdog are suspended while parked and
            // restart with fresh deadlines at release.
            if !parked
                && self
                    .heartbeat
                    .as_mut()
                    .map(|heartbeat| heartbeat.refresh(Instant::now()))
                    .transpose()?
                    .unwrap_or(false)
            {
                facts.liveness_expired = true;
                return Ok(facts);
            }
            if !parked && watchdog.expired(start.elapsed()) {
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
        if let Some((slot, _)) = &self.park {
            let mut state = slot.state.lock().unwrap();
            state.1 = true;
            state.0 = None;
        }
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
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn an_unanswered_capture_learns_why_the_source_ended() {
        let host = TerminalFacts {
            host_stop: true,
            ..Default::default()
        };
        assert_eq!(
            Runtime::unanswered_capture_stop(&host),
            Some(crate::capture::GuestStop {
                reason: 0,
                host_stop: true
            })
        );
        let panic = TerminalFacts {
            reason: 4,
            ..Default::default()
        };
        assert_eq!(
            Runtime::unanswered_capture_stop(&panic),
            Some(crate::capture::GuestStop {
                reason: 4,
                host_stop: false
            })
        );
        // Not an ending the guard knows: the capture's own deadline answers.
        assert_eq!(
            Runtime::unanswered_capture_stop(&TerminalFacts::default()),
            None
        );
    }

    #[test]
    fn pending_host_stop_survives_guest_data_fault() {
        let stop = utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        stop.write(1).unwrap();
        let facts =
            super::guest_error_facts(crate::guest_data_error("bad guest data"), &stop).unwrap();
        assert!(facts.guest_fault);
        assert!(facts.host_stop);
    }

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
        assert_eq!(
            closed
                .check_open()
                .unwrap_err()
                .get_ref()
                .unwrap()
                .downcast_ref::<ParkedReceiverError>(),
            Some(&ParkedReceiverError::Closed)
        );
        let open = RestoreAssignment(Arc::new(ParkSlot {
            state: Mutex::new((None, false)),
            wake: utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            ready: Default::default(),
        }));
        assert!(open.check_open().is_ok());
        assert!(handle.check_open().is_err());
    }

    #[test]
    fn panicking_observer_does_not_skip_later_observers_or_double_panic() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let observers = ExitObservers::default();
        observers.register(Arc::new(Mutex::new(|| panic!("observer failure"))));
        observers.register(Arc::new(Mutex::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })));
        observers.notify();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn observer_registered_after_exit_is_notified_once_at_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = ExitObserverRegistry(Arc::new(ExitObservers::default()));
        let early = calls.clone();
        registry.register(Arc::new(Mutex::new(move || {
            early.fetch_add(1, Ordering::SeqCst);
        })));
        registry.0.notify();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let late = calls.clone();
        registry.register(Arc::new(Mutex::new(move || {
            late.fetch_add(1, Ordering::SeqCst);
        })));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // A second exit notification (Drop after shutdown) calls nobody again.
        registry.0.notify();
        drop(registry);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn runtime_can_move_to_a_parked_thread() {
        fn is_send<T: Send>() {}
        is_send::<Runtime>();
        is_send::<ParkedRun>();
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
