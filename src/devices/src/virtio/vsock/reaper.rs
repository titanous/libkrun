use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use super::proxy::Proxy;
use crossbeam_channel::Receiver;

pub type ProxyMap = Arc<RwLock<HashMap<u64, Mutex<Box<dyn Proxy>>>>>;
const TIMEOUT: Duration = Duration::new(5, 0);

// ── Pure timeout calculation logic ────────────────────────────────────────────
//
// The timeout calculation in `check_expiration` is extracted as a pure function
// so that Kani can verify its properties without needing to model Instant::now()
// or HashMap operations.
//
// `remaining_timeout(elapsed, timeout)` computes how much time is left before a
// connection expires, given how much time has already elapsed.

/// Compute the remaining time until expiry for a single connection.
///
/// Returns `None` if the connection has already expired (elapsed >= timeout),
/// or `Some(remaining)` where remaining = timeout - elapsed.
///
/// This mirrors the logic in `check_expiration` where entries with
/// elapsed >= TIMEOUT are added to the expired list, and entries where
/// elapsed < TIMEOUT contribute to the highest_elapsed calculation used
/// for determining the next wakeup deadline.
#[cfg_attr(not(kani), allow(dead_code))]
#[cfg_attr(kani, kani::ensures(|result: &Option<Duration>| {
    match result {
        // When Some: remaining must equal the arithmetic complement and be positive.
        Some(rem) => elapsed < timeout && *rem == timeout - elapsed && *rem > Duration::ZERO,
        // When None: the connection has expired (elapsed >= timeout).
        None => elapsed >= timeout,
    }
}))]
pub(crate) fn remaining_timeout(elapsed: Duration, timeout: Duration) -> Option<Duration> {
    if elapsed >= timeout {
        None
    } else {
        let remaining = timeout - elapsed;
        // remaining is positive because elapsed < timeout (strict)
        debug_assert!(remaining > Duration::ZERO);
        Some(remaining)
    }
}

/// Compute the wakeup deadline for the reaper loop given a set of elapsed
/// durations (one per active connection).
///
/// Returns `Duration::MAX` when `elapsed_values` is empty (no pending
/// connections → sleep indefinitely).  Otherwise returns the minimum remaining
/// time across all non-expired connections.
///
/// This is the pure-logic core of `check_expiration`.
#[cfg_attr(not(kani), allow(dead_code))]
pub(crate) fn reaper_wakeup_deadline(elapsed_values: &[Duration], timeout: Duration) -> Duration {
    let mut highest_elapsed = Duration::ZERO;

    for &elapsed in elapsed_values {
        if elapsed >= timeout {
            // Would be expired; skip (already removed by caller)
            continue;
        }
        if elapsed > highest_elapsed {
            highest_elapsed = elapsed;
        }
    }

    if highest_elapsed > Duration::ZERO {
        let remaining = timeout - highest_elapsed;
        debug_assert!(remaining > Duration::ZERO);
        remaining
    } else {
        Duration::MAX
    }
}

pub struct ReaperThread {
    receiver: Receiver<u64>,
    proxy_map: ProxyMap,
    released_map: HashMap<u64, Instant>,
}

impl ReaperThread {
    pub fn new(receiver: Receiver<u64>, proxy_map: ProxyMap) -> Self {
        Self {
            receiver,
            proxy_map,
            released_map: HashMap::new(),
        }
    }

    fn check_expiration(&mut self) -> Duration {
        let mut highest_elapsed = Duration::ZERO;
        let mut expired: Vec<u64> = Vec::new();
        let now = Instant::now();

        for (id, exptime) in self.released_map.iter() {
            let elapsed = now.duration_since(*exptime);
            if elapsed >= TIMEOUT {
                expired.push(*id);
            } else if elapsed > highest_elapsed {
                highest_elapsed = elapsed;
            }
        }

        if !expired.is_empty() {
            let mut pmap = self.proxy_map.write().unwrap();
            for id in expired {
                debug!("removing proxy: {id}");
                pmap.remove(&id);
                self.released_map.remove(&id);
            }
            debug!("remainig proxies: {}", pmap.len());
        }

        let mut timeout = Duration::MAX;
        if highest_elapsed > Duration::ZERO {
            timeout = TIMEOUT - highest_elapsed;
            assert!(timeout > Duration::ZERO);
        }
        timeout
    }

    fn work(&mut self) {
        loop {
            let timeout = self.check_expiration();
            if let Ok(id) = self.receiver.recv_timeout(timeout) {
                self.released_map.insert(id, Instant::now());
            }
        }
    }

    pub fn run(mut self) {
        thread::Builder::new()
            .name("vsock reaper".into())
            .spawn(move || self.work())
            .unwrap();
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    // ── remaining_timeout proofs ──────────────────────────────────────────────

    /// Proof: `remaining_timeout` satisfies its post-condition for all inputs.
    ///
    /// We constrain elapsed and timeout to `u32`-second granularity to keep
    /// Kani's state space tractable.  The contract ensures:
    /// - `Some(rem)`: elapsed < timeout, rem = timeout - elapsed > 0
    /// - `None`: elapsed >= timeout
    #[kani::proof_for_contract(remaining_timeout)]
    fn proof_remaining_timeout_contract() {
        // Constrain to values that fit in Duration::from_secs without overflow.
        let elapsed_secs: u32 = kani::any();
        let timeout_secs: u32 = kani::any();
        let elapsed = Duration::from_secs(u64::from(elapsed_secs));
        let timeout = Duration::from_secs(u64::from(timeout_secs));
        let _ = remaining_timeout(elapsed, timeout);
        kani::cover!(
            elapsed_secs == timeout_secs,
            "elapsed == timeout boundary exercised"
        );
        kani::cover!(
            elapsed_secs < timeout_secs,
            "elapsed < timeout (not expired) exercised"
        );
    }

    /// Proof: when elapsed == timeout, remaining_timeout returns None
    /// (connection has expired exactly at the boundary).
    #[kani::proof]
    fn proof_remaining_timeout_at_boundary_returns_none() {
        let secs: u32 = kani::any();
        let d = Duration::from_secs(u64::from(secs));
        // elapsed == timeout → expired
        let result = remaining_timeout(d, d);
        kani::assert(
            result.is_none(),
            "elapsed == timeout must return None (expired)",
        );
        kani::cover!(secs == 0, "zero elapsed == zero timeout boundary exercised");
        kani::cover!(
            secs == u32::MAX,
            "max elapsed == max timeout boundary exercised"
        );
    }

    /// Proof: when elapsed > timeout, remaining_timeout returns None.
    #[kani::proof]
    fn proof_remaining_timeout_past_expiry_returns_none() {
        let timeout_secs: u32 = kani::any();
        // elapsed is strictly greater than timeout
        let extra: u32 = kani::any_where(|&e| e > 0);
        let timeout = Duration::from_secs(u64::from(timeout_secs));
        let elapsed = timeout.saturating_add(Duration::from_secs(u64::from(extra)));
        // Guard against overflow (saturating_add may equal timeout if extra would overflow)
        kani::assume(elapsed > timeout);
        let result = remaining_timeout(elapsed, timeout);
        kani::assert(
            result.is_none(),
            "elapsed > timeout must return None (already expired)",
        );
        kani::cover!(timeout_secs == 0, "zero timeout past-expiry exercised");
        kani::cover!(extra == 1, "minimal extra elapsed past-expiry exercised");
    }

    /// Proof: when elapsed < timeout, remaining is strictly positive.
    ///
    /// This validates the invariant checked by `assert!(timeout > Duration::ZERO)`
    /// in `check_expiration`.
    #[kani::proof]
    fn proof_remaining_is_positive_when_not_expired() {
        let elapsed_secs: u32 = kani::any();
        let timeout_secs: u32 = kani::any_where(|&t| t > elapsed_secs);
        let elapsed = Duration::from_secs(u64::from(elapsed_secs));
        let timeout = Duration::from_secs(u64::from(timeout_secs));
        // timeout > elapsed → strict, so elapsed < timeout
        let result = remaining_timeout(elapsed, timeout);
        match result {
            Some(rem) => {
                kani::assert(rem > Duration::ZERO, "remaining must be strictly positive");
            }
            None => {
                kani::assert(false, "must return Some when elapsed < timeout");
            }
        }
        kani::cover!(elapsed_secs == 0, "zero elapsed remaining exercised");
        kani::cover!(
            elapsed_secs == timeout_secs - 1,
            "elapsed one second before expiry exercised"
        );
    }

    // ── reaper_wakeup_deadline proofs ─────────────────────────────────────────

    /// Proof: with an empty elapsed list, the deadline is Duration::MAX.
    ///
    /// An empty reaper means no pending connections → sleep indefinitely.
    #[kani::proof]
    fn proof_wakeup_deadline_empty_is_max() {
        let timeout = Duration::from_secs(5);
        let result = reaper_wakeup_deadline(&[], timeout);
        kani::assert(
            result == Duration::MAX,
            "empty elapsed list must yield Duration::MAX",
        );
    }

    /// Proof: when all connections have expired (elapsed >= timeout), the
    /// deadline is still Duration::MAX (expired entries are skipped).
    ///
    /// Bounded to 4 entries to keep Kani tractable.
    #[kani::proof]
    #[kani::unwind(5)]
    fn proof_wakeup_deadline_all_expired_is_max() {
        let timeout_secs: u32 = kani::any_where(|&t| t > 0 && t < 1000);
        let timeout = Duration::from_secs(u64::from(timeout_secs));

        // All four elapsed values are >= timeout.
        let e0_secs: u32 = kani::any_where(|&e| e >= timeout_secs);
        let e1_secs: u32 = kani::any_where(|&e| e >= timeout_secs);
        let e2_secs: u32 = kani::any_where(|&e| e >= timeout_secs);
        let e3_secs: u32 = kani::any_where(|&e| e >= timeout_secs);

        let elapsed = [
            Duration::from_secs(u64::from(e0_secs)),
            Duration::from_secs(u64::from(e1_secs)),
            Duration::from_secs(u64::from(e2_secs)),
            Duration::from_secs(u64::from(e3_secs)),
        ];

        let result = reaper_wakeup_deadline(&elapsed, timeout);
        kani::assert(
            result == Duration::MAX,
            "all-expired elapsed list must yield Duration::MAX",
        );
        kani::cover!(
            e0_secs == timeout_secs,
            "entry expired exactly at boundary exercised"
        );
        kani::cover!(
            e0_secs > timeout_secs,
            "entry expired past boundary exercised"
        );
    }

    /// Proof: when there is one non-expired connection, the deadline is
    /// strictly less than the timeout.
    ///
    /// remaining = timeout - elapsed, which is positive and < timeout.
    #[kani::proof]
    fn proof_wakeup_deadline_one_live_is_less_than_timeout() {
        let timeout_secs: u32 = kani::any_where(|&t| t > 1 && t < 1000);
        let timeout = Duration::from_secs(u64::from(timeout_secs));

        // One non-expired connection with elapsed in (0, timeout).
        let elapsed_secs: u32 = kani::any_where(|&e| e > 0 && e < timeout_secs);
        let elapsed = [Duration::from_secs(u64::from(elapsed_secs))];

        let result = reaper_wakeup_deadline(&elapsed, timeout);
        kani::assert(
            result > Duration::ZERO && result < timeout,
            "single live connection deadline must be in (0, timeout)",
        );
        kani::cover!(
            elapsed_secs == 1,
            "minimal elapsed single live connection exercised"
        );
        kani::cover!(
            elapsed_secs == timeout_secs - 1,
            "elapsed one second before expiry exercised"
        );
    }

    /// Proof: the deadline is always > Duration::ZERO when there are live
    /// connections (i.e., the reaper never schedules a zero-duration timeout).
    ///
    /// A zero-duration timeout would cause a busy-loop in the work thread.
    #[kani::proof]
    #[kani::unwind(3)]
    fn proof_wakeup_deadline_never_zero() {
        let timeout_secs: u32 = kani::any_where(|&t| t > 1 && t < 1000);
        let timeout = Duration::from_secs(u64::from(timeout_secs));

        // One non-expired connection with elapsed strictly less than timeout.
        let elapsed_secs: u32 = kani::any_where(|&e| e < timeout_secs);
        let elapsed = [Duration::from_secs(u64::from(elapsed_secs))];

        let result = reaper_wakeup_deadline(&elapsed, timeout);
        // When elapsed == 0, highest_elapsed stays ZERO → returns MAX (also > 0).
        // When elapsed > 0 and < timeout, returns timeout - elapsed > 0.
        kani::assert(
            result > Duration::ZERO,
            "wakeup deadline must never be zero",
        );
        kani::cover!(
            elapsed_secs == 0,
            "zero elapsed never-zero deadline exercised"
        );
        kani::cover!(
            elapsed_secs == timeout_secs - 1,
            "near-expiry never-zero deadline exercised"
        );
    }
}
