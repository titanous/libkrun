//! Runtime authorization and bounded completion for cooperative capture.
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct CaptureOutcome {
    pub template: crate::template::Template,
    /// None for descriptor-only capture; Some reports requested publication.
    pub export: Option<io::Result<std::path::PathBuf>>,
}

impl std::ops::Deref for CaptureOutcome {
    type Target = crate::template::Template;
    fn deref(&self) -> &Self::Target {
        &self.template
    }
}

pub fn refusal_errno(error: &io::Error) -> i32 {
    match error.kind() {
        io::ErrorKind::PermissionDenied => -libc::EPERM,
        io::ErrorKind::TimedOut => -libc::ETIMEDOUT,
        _ => -libc::EINVAL,
    }
}

#[derive(Default)]
struct State {
    deadline: Option<Instant>,
    accepted: bool,
    marker_confirmed: bool,
    result: Option<io::Result<CaptureOutcome>>,
}

#[derive(Clone, Default)]
pub struct CaptureRequest(Arc<(Mutex<State>, Condvar)>);

impl CaptureRequest {
    pub fn confirm_marker(&self, active: bool) {
        self.0 .0.lock().unwrap().marker_confirmed = active;
    }

    pub fn arm(&self, timeout: Duration) -> io::Result<()> {
        let mut state = self.0 .0.lock().unwrap();
        if !state.marker_confirmed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                crate::cooperative::TemplateMarkerMissing,
            ));
        }
        if state.deadline.is_some() || state.accepted {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "capture already armed",
            ));
        }
        state.deadline = Some(Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "capture timeout overflow")
        })?);
        state.result = None;
        Ok(())
    }

    pub fn authorize(&self) -> io::Result<()> {
        let state = self.0 .0.lock().unwrap();
        match state.deadline {
            Some(deadline) if Instant::now() < deadline => Ok(()),
            Some(_) => Err(io::Error::from(io::ErrorKind::TimedOut)),
            None => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        }
    }

    pub fn is_armed(&self) -> bool {
        self.0
             .0
            .lock()
            .unwrap()
            .deadline
            .is_some_and(|deadline| Instant::now() < deadline)
    }

    /// Atomically choose irreversible acceptance against deadline cancellation.
    pub fn accept(&self) -> bool {
        let mut state = self.0 .0.lock().unwrap();
        if state.accepted
            || state
                .deadline
                .is_none_or(|deadline| Instant::now() >= deadline)
        {
            return false;
        }
        state.accepted = true;
        true
    }

    pub fn complete(&self, result: io::Result<CaptureOutcome>) {
        let mut state = self.0 .0.lock().unwrap();
        state.result = Some(result);
        state.deadline = None;
        self.0 .1.notify_all();
    }

    pub fn wait(&self) -> io::Result<CaptureOutcome> {
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
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    if state.accepted {
                        "cooperative freeze timed out after acceptance; source must be discarded"
                    } else {
                        "cooperative freeze timed out before acceptance; source remains resumable"
                    },
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
    fn guest_refusals_have_distinct_errno() {
        assert_eq!(
            refusal_errno(&io::Error::from(io::ErrorKind::PermissionDenied)),
            -libc::EPERM
        );
        assert_eq!(
            refusal_errno(&io::Error::from(io::ErrorKind::TimedOut)),
            -libc::ETIMEDOUT
        );
        assert_eq!(
            refusal_errno(&io::Error::from(io::ErrorKind::InvalidData)),
            -libc::EINVAL
        );
    }

    #[test]
    fn acceptance_and_timeout_have_one_terminal_decision() {
        let expired = CaptureRequest::default();
        expired.confirm_marker(true);
        expired.arm(Duration::ZERO).unwrap();
        assert!(!expired.accept());
        assert!(expired
            .wait()
            .err()
            .unwrap()
            .to_string()
            .contains("before acceptance"));
        assert!(!expired.accept());

        let accepted = CaptureRequest::default();
        accepted.confirm_marker(true);
        accepted.arm(Duration::from_secs(30)).unwrap();
        assert!(accepted.accept());
        assert!(!accepted.accept());
        accepted.0 .0.lock().unwrap().deadline = Some(Instant::now());
        assert!(accepted
            .wait()
            .err()
            .unwrap()
            .to_string()
            .contains("after acceptance"));
        assert!(accepted.arm(Duration::from_secs(1)).is_err());
    }

    #[test]
    fn timeout_disarms_and_requests_are_consumed_once() {
        let request = CaptureRequest::default();
        assert!(!request.is_armed());
        let error = request.arm(Duration::from_secs(1)).unwrap_err();
        assert!(error
            .get_ref()
            .unwrap()
            .is::<crate::cooperative::TemplateMarkerMissing>());
        request.confirm_marker(true);
        request.arm(Duration::from_millis(1)).unwrap();
        assert!(request.arm(Duration::from_secs(1)).is_err());
        assert_eq!(
            request.wait().err().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(!request.is_armed());
    }
}
