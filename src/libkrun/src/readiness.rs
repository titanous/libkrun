//! Embedder-confirmed readiness, independent of kernel continuation.
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Waiting,
    Ready,
    TimedOut,
}

/// A start deadline expired before the embedder validated the new guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestReadyTimeout;
impl std::fmt::Display for GuestReadyTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("new guest did not become ready before its deadline; destroy the guest")
    }
}
impl std::error::Error for GuestReadyTimeout {}

/// Per-guest readiness confirmation. Mark ready only after validating the
/// expected agent identity and generation, never on device I/O or resume alone.
#[derive(Clone)]
pub struct GuestReadiness {
    inner: Arc<(Mutex<State>, Condvar)>,
    deadline: Instant,
}
impl GuestReadiness {
    pub fn new(timeout: Duration) -> std::io::Result<Self> {
        Ok(Self {
            inner: Arc::new((Mutex::new(State::Waiting), Condvar::new())),
            deadline: Instant::now().checked_add(timeout).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "readiness timeout overflow",
                )
            })?,
        })
    }

    /// Return false if this guest has already missed its start deadline.
    pub fn mark_ready(&self) -> bool {
        let mut state = self.inner.0.lock().unwrap();
        if *state == State::Ready {
            return true;
        }
        if *state == State::TimedOut || Instant::now() >= self.deadline {
            *state = State::TimedOut;
            self.inner.1.notify_all();
            return false;
        }
        *state = State::Ready;
        self.inner.1.notify_all();
        true
    }

    /// Wait without blocking the VMM event loop. On timeout the process
    /// supervisor must destroy the guest; a late confirmation is refused.
    pub fn wait(&self) -> Result<(), GuestReadyTimeout> {
        let mut state = self.inner.0.lock().unwrap();
        loop {
            match *state {
                State::Ready => return Ok(()),
                State::TimedOut => return Err(GuestReadyTimeout),
                State::Waiting => {}
            }
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                *state = State::TimedOut;
                return Err(GuestReadyTimeout);
            }
            state = self.inner.1.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readiness_and_deadline_are_terminal() {
        let ready = GuestReadiness::new(Duration::from_secs(10)).unwrap();
        assert!(ready.mark_ready());
        assert_eq!(ready.wait(), Ok(()));
        let mut duplicate = ready.clone();
        duplicate.deadline = Instant::now();
        assert!(duplicate.mark_ready());
        assert_eq!(duplicate.wait(), Ok(()));
        let late = GuestReadiness::new(Duration::ZERO).unwrap();
        assert_eq!(late.wait(), Err(GuestReadyTimeout));
        assert!(!late.mark_ready());
    }
}
