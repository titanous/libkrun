//! Runtime authorization and bounded completion for cooperative capture.
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    deadline: Option<Instant>,
    requested: bool,
    result: Option<io::Result<crate::template::Template>>,
}

#[derive(Clone, Default)]
pub struct CaptureRequest(Arc<(Mutex<State>, Condvar)>);

impl CaptureRequest {
    pub fn arm(&self, timeout: Duration) -> io::Result<()> {
        let mut state = self.0 .0.lock().unwrap();
        if state.deadline.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "capture already armed",
            ));
        }
        state.deadline = Some(Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "capture timeout overflow")
        })?);
        state.requested = true;
        state.result = None;
        Ok(())
    }

    pub fn take_request(&self) -> bool {
        let mut state = self.0 .0.lock().unwrap();
        if state
            .deadline
            .is_none_or(|deadline| Instant::now() >= deadline)
        {
            return false;
        }
        std::mem::take(&mut state.requested)
    }

    pub fn is_armed(&self) -> bool {
        self.0
             .0
            .lock()
            .unwrap()
            .deadline
            .is_some_and(|deadline| Instant::now() < deadline)
    }

    pub fn complete(&self, result: io::Result<crate::template::Template>) {
        let mut state = self.0 .0.lock().unwrap();
        state.result = Some(result);
        state.deadline = None;
        state.requested = false;
        self.0 .1.notify_all();
    }

    pub fn wait(&self) -> io::Result<crate::template::Template> {
        let mut state = self.0 .0.lock().unwrap();
        loop {
            if let Some(result) = state.result.take() {
                return result;
            }
            let Some(deadline) = state.deadline else {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "capture not armed",
                ));
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.deadline = None;
                state.requested = false;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "cooperative freeze timed out",
                ));
            }
            state = self.0 .1.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timeout_disarms_and_requests_are_consumed_once() {
        let request = CaptureRequest::default();
        assert!(!request.is_armed());
        assert!(!request.take_request());
        request.arm(Duration::from_millis(1)).unwrap();
        assert!(request.arm(Duration::from_secs(1)).is_err());
        assert!(request.take_request());
        assert!(!request.take_request());
        assert_eq!(
            request.wait().err().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(!request.is_armed());
    }
}
