//! Per-guest authorization of host socket operations.
use std::sync::Arc;

use nix::errno::Errno;
pub use nix::sys::socket::SockaddrStorage as SocketAddress;
use nix::sys::socket::SockaddrStorage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketOperation {
    TcpConnect,
    UdpConnect,
    UdpSend,
    Bind,
    Listen,
    UnixConnect,
    UnixBind,
    UnixRemove,
    Accept,
    ReceiveDatagram,
    UnixDatagramConnect,
    UnixDatagramSend,
}

/// Authorize host socket operations, including inbound accept/receive.
/// IPv4-mapped IPv6 is normalized; native scope IDs and ports are unchanged.
/// Unix names are untrusted raw strings, not canonicalized paths.
/// Callbacks run synchronously on vsock device threads while proxy locks are held.
/// They must be nonblocking and in-memory: a slow callback stalls every stream of
/// this guest, including mapped control ports. Never reenter VM controls here.
pub trait OutboundPolicy: Send + Sync {
    fn allow(&self, operation: SocketOperation, destination: &SocketAddress) -> bool;
}

/// Refuse every operation except exact operation/address pairs in this list.
/// Unix names are compared as supplied, not canonicalized or prefix-matched.
pub struct AllowlistPolicy {
    entries: Vec<(SocketOperation, SocketAddress)>,
}

impl AllowlistPolicy {
    pub fn new(entries: impl IntoIterator<Item = (SocketOperation, SocketAddress)>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(op, addr)| (op, normalize(&addr)))
                .collect(),
        }
    }
}

impl OutboundPolicy for AllowlistPolicy {
    fn allow(&self, operation: SocketOperation, destination: &SocketAddress) -> bool {
        self.entries
            .iter()
            .any(|(op, addr)| *op == operation && *addr == normalize(destination))
    }
}

fn normalize(destination: &SocketAddress) -> SocketAddress {
    destination
        .as_sockaddr_in6()
        .and_then(|addr| {
            addr.ip()
                .to_ipv4_mapped()
                .map(|ip| SocketAddress::from(std::net::SocketAddrV4::new(ip, addr.port())))
        })
        .unwrap_or(*destination)
}

#[derive(Clone)]
pub struct SocketPolicy {
    pub policy: Option<Arc<dyn OutboundPolicy>>,
    pub internet_enabled: bool,
}

impl Default for SocketPolicy {
    fn default() -> Self {
        Self {
            policy: None,
            internet_enabled: false,
        }
    }
}

impl SocketPolicy {
    pub fn check(
        &self,
        operation: SocketOperation,
        destination: &SockaddrStorage,
    ) -> Result<(), Errno> {
        let normalized = normalize(destination);
        let destination = &normalized;
        let internet =
            destination.as_sockaddr_in().is_some() || destination.as_sockaddr_in6().is_some();
        if (internet && !self.internet_enabled)
            || self
                .policy
                .as_ref()
                .is_some_and(|policy| !policy.allow(operation, destination))
        {
            Err(Errno::EACCES)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn mapped_addresses_normalize_and_allowlist_is_closed() {
        let ipv4: SocketAddress = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 443).into();
        let mapped: SocketAddress =
            std::net::SocketAddrV6::new(Ipv4Addr::LOCALHOST.to_ipv6_mapped(), 443, 0, 9).into();
        let policy = SocketPolicy {
            policy: Some(Arc::new(DenyPort)),
            internet_enabled: true,
        };
        assert_eq!(
            policy.check(SocketOperation::TcpConnect, &mapped),
            Err(Errno::EACCES)
        );
        let allow = AllowlistPolicy::new([(SocketOperation::TcpConnect, ipv4)]);
        assert!(allow.allow(SocketOperation::TcpConnect, &mapped));
        assert!(!allow.allow(SocketOperation::Accept, &mapped));
        assert!(!AllowlistPolicy::new([]).allow(SocketOperation::TcpConnect, &ipv4));
        let native: SocketAddress =
            std::net::SocketAddrV6::new(std::net::Ipv6Addr::LOCALHOST, 123, 4, 9).into();
        assert_eq!(normalize(&native).as_sockaddr_in6().unwrap().scope_id(), 9);
        assert_eq!(normalize(&native).as_sockaddr_in6().unwrap().port(), 123);
    }

    #[test]
    fn slow_callback_resumes_without_state_change() {
        use std::sync::{Condvar, Mutex};
        struct Slow(Arc<(Mutex<bool>, Condvar)>, std::sync::mpsc::Sender<()>);
        impl OutboundPolicy for Slow {
            fn allow(&self, _: SocketOperation, _: &SocketAddress) -> bool {
                let _ = self.1.send(());
                let (lock, ready) = &*self.0;
                let mut done = lock.lock().unwrap();
                while !*done {
                    done = ready.wait(done).unwrap();
                }
                false
            }
        }
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (entered, waiting) = std::sync::mpsc::channel();
        let policy = SocketPolicy {
            policy: Some(Arc::new(Slow(gate.clone(), entered))),
            internet_enabled: true,
        };
        let other = policy.clone();
        let addr: SocketAddress = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80).into();
        let thread = std::thread::spawn(move || other.check(SocketOperation::TcpConnect, &addr));
        waiting
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(!thread.is_finished());
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert_eq!(thread.join().unwrap(), Err(Errno::EACCES));
        assert_eq!(
            policy.check(SocketOperation::TcpConnect, &addr),
            Err(Errno::EACCES)
        );
    }

    struct DenyPort;
    impl OutboundPolicy for DenyPort {
        fn allow(&self, _: SocketOperation, destination: &SockaddrStorage) -> bool {
            destination
                .as_sockaddr_in()
                .is_none_or(|addr| addr.port() != 443)
        }
    }

    #[test]
    fn default_denies_internet_and_enabled_policy_checks_destination() {
        let denied: SockaddrStorage = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 443).into();
        let allowed: SockaddrStorage = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80).into();
        let operations = [
            SocketOperation::TcpConnect,
            SocketOperation::UdpConnect,
            SocketOperation::UdpSend,
            SocketOperation::Bind,
            SocketOperation::Listen,
        ];
        let policy = SocketPolicy {
            policy: Some(Arc::new(DenyPort)),
            internet_enabled: true,
        };
        for op in operations {
            assert_eq!(SocketPolicy::default().check(op, &denied), Err(Errno::EACCES));
            assert_eq!(policy.check(op, &denied), Err(Errno::EACCES));
            assert!(policy.check(op, &allowed).is_ok());
        }
    }

    #[test]
    fn disabled_internet_preserves_unix() {
        use nix::sys::socket::{SockaddrLike, UnixAddr};
        let policy = SocketPolicy {
            internet_enabled: false,
            ..Default::default()
        };
        let internet: SockaddrStorage = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80).into();
        assert_eq!(
            policy.check(SocketOperation::TcpConnect, &internet),
            Err(Errno::EACCES)
        );
        let ipv6: SockaddrStorage =
            std::net::SocketAddrV6::new(std::net::Ipv6Addr::LOCALHOST, 80, 0, 0).into();
        assert_eq!(
            policy.check(SocketOperation::UdpSend, &ipv6),
            Err(Errno::EACCES)
        );
        let unix = UnixAddr::new("/tmp/mapped-vsock").unwrap();
        // SAFETY: unix owns a valid sockaddr_un for the reported length.
        let unix =
            unsafe { SockaddrStorage::from_raw(unix.as_ptr().cast(), Some(unix.len())) }.unwrap();
        assert!(policy.check(SocketOperation::UnixConnect, &unix).is_ok());
    }
}
