//! Per-guest ceiling on the host sockets the VMM holds for a guest's forwarded sockets.
//!
//! Every host socket the VMM opens for a guest (a stream proxy's socket once it connects or
//! listens, a datagram proxy's socket, a connection accepted on a mapped listener) holds a
//! permit until the proxy is dropped, including while its removal is deferred after the
//! guest closed it. At the ceiling the guest gets a definite error (ENOBUFS) instead of the
//! VMM opening sockets until its descriptor limit or the guest's memory runs out.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The ceiling a guest gets when its VMM is not configured with another one.
pub const DEFAULT_MAX_HOST_SOCKETS: usize = 4096;

#[derive(Debug)]
pub struct HostSocketBudget {
    used: AtomicUsize,
    limit: usize,
}

impl HostSocketBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            limit,
        })
    }

    /// A permit for one more host socket, or None at the ceiling.
    pub fn try_acquire(self: &Arc<Self>) -> Option<HostSocketPermit> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < self.limit).then_some(used + 1)
            })
            .ok()
            .map(|_| HostSocketPermit(self.clone()))
    }

    pub fn in_use(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
}

/// One host socket counted against its guest's ceiling; returned when dropped.
#[derive(Debug)]
pub struct HostSocketPermit(Arc<HostSocketBudget>);

impl Drop for HostSocketPermit {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A permit from the policy's budget; a policy without a budget does not limit.
pub(crate) fn acquire(
    budget: &Option<Arc<HostSocketBudget>>,
) -> Result<Option<HostSocketPermit>, nix::errno::Errno> {
    match budget {
        None => Ok(None),
        Some(budget) => budget
            .try_acquire()
            .map(Some)
            .ok_or(nix::errno::Errno::ENOBUFS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_stop_at_the_ceiling_and_return_on_drop() {
        let budget = HostSocketBudget::new(2);
        let first = budget.try_acquire().unwrap();
        let _second = budget.try_acquire().unwrap();
        assert!(budget.try_acquire().is_none());
        assert_eq!(budget.in_use(), 2);
        drop(first);
        assert_eq!(budget.in_use(), 1);
        assert!(budget.try_acquire().is_some());
    }
}
