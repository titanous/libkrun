//! Backend-internal adapter for the shared template capture contract.
//! This is not an alternative embedding API: libkrun supplies the adapter that
//! owns its shared CaptureRequest and sealed Template.
use std::io;

/// Refusable preparation is complete. Acceptance races the shared request's
/// deadline; completion must run only after stopping devices and closing the VM.
pub trait PreparedCapture: Send {
    fn accept(&mut self) -> bool;
    fn complete(self: Box<Self>);
    /// The guest ended (or the host stopped it) after acceptance and before
    /// publication: no template is published; the waiter learns why.
    fn reject(self: Box<Self>, _stop: GuestStop) {}
}

/// Why a capture was refused because its source is ending: a final stated
/// reason (1 power-off, 2 reboot, 3 halt, 4 panic; 0 none) and/or a pending
/// host stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestStop {
    pub reason: u8,
    pub host_stop: bool,
}

pub trait CaptureHandler: Send {
    /// One boot notification, verified against the published control page.
    fn marker_confirmed(&mut self, _active: bool) {}
    /// Return None to refuse and recover the source. All memory must be copied
    /// into immutable owned storage before returning a pending acceptance.
    fn prepare(
        &mut self,
        memory: &vm_memory::GuestMemoryMmap,
        control: u64,
    ) -> io::Result<Option<Box<dyn PreparedCapture>>>;
    /// A preparation error must be delivered through the shared request, without
    /// terminating the source after it has been released in place.
    fn failed(&mut self, error: io::Error);
    /// Refused before acceptance because the source is ending.
    fn guest_stopped(&mut self, stop: GuestStop) {
        self.failed(io::Error::other(format!(
            "guest stopped during template capture: {stop:?}"
        )));
    }
}

/// Own the acceptance decision until the caller has synchronously closed source.
pub struct AcceptedCapture(Box<dyn PreparedCapture>);
impl AcceptedCapture {
    pub fn decide(mut prepared: Box<dyn PreparedCapture>) -> Option<Self> {
        if prepared.accept() {
            Some(Self(prepared))
        } else {
            None
        }
    }
    /// Called after source close, never merely after device reset.
    pub fn source_closed(self) {
        self.0.complete();
    }
    /// The source ended before publication; never publishes.
    pub fn reject(self, stop: GuestStop) {
        self.0.reject(stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    struct Pending {
        allow: bool,
        completed: Arc<AtomicBool>,
    }
    impl PreparedCapture for Pending {
        fn accept(&mut self) -> bool {
            self.allow
        }
        fn complete(self: Box<Self>) {
            self.completed.store(true, Ordering::SeqCst);
        }
    }
    #[test]
    fn acceptance_does_not_complete_before_source_close() {
        let completed = Arc::new(AtomicBool::new(false));
        let accepted = AcceptedCapture::decide(Box::new(Pending {
            allow: true,
            completed: completed.clone(),
        }))
        .unwrap();
        assert!(!completed.load(Ordering::SeqCst));
        accepted.source_closed();
        assert!(completed.load(Ordering::SeqCst));
    }
    #[test]
    fn refusal_never_delivers_template() {
        let completed = Arc::new(AtomicBool::new(false));
        assert!(AcceptedCapture::decide(Box::new(Pending {
            allow: false,
            completed: completed.clone()
        }))
        .is_none());
        assert!(!completed.load(Ordering::SeqCst));
    }
}
