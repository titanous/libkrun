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
}

/// The address retains IPv4/IPv6 ports and Unix paths (including abstract names).
/// Callbacks run on device threads and must not block indefinitely.
pub trait OutboundPolicy: Send + Sync {
    fn allow(&self, operation: SocketOperation, destination: &SocketAddress) -> bool;
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
            internet_enabled: true,
        }
    }
}

impl SocketPolicy {
    pub fn check(
        &self,
        operation: SocketOperation,
        destination: &SockaddrStorage,
    ) -> Result<(), Errno> {
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

    struct DenyPort;
    impl OutboundPolicy for DenyPort {
        fn allow(&self, _: SocketOperation, destination: &SockaddrStorage) -> bool {
            destination
                .as_sockaddr_in()
                .is_none_or(|addr| addr.port() != 443)
        }
    }

    #[test]
    fn default_allows_and_policy_checks_destination() {
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
            ..Default::default()
        };
        for op in operations {
            assert!(SocketPolicy::default().check(op, &denied).is_ok());
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
