use std::collections::HashMap;
use std::fs;
use std::net::{Ipv4Addr, SocketAddrV4, SocketAddrV6};
use std::num::Wrapping;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use libc::EINVAL;
use nix::sys::socket::{
    accept, bind, connect, getpeername, listen, recv, setsockopt, shutdown, socket, sockopt,
    AddressFamily, Backlog, MsgFlags, Shutdown, SockFlag, SockType, SockaddrLike, SockaddrStorage,
};

use super::super::Queue as VirtQueue;
use super::defs;
use super::defs::uapi;
use super::muxer::{push_packet, MuxerRx};
use super::muxer_rxq::MuxerRxQ;
use super::packet::{
    TsiAcceptReq, TsiConnectReq, TsiGetnameRsp, TsiListenReq, TsiSendtoAddr, VsockPacket,
};
use super::proxy::{
    NewProxyType, Proxy, ProxyError, ProxyRemoval, ProxyStatus, ProxyUpdate, RecvPkt,
};
use utils::epoll::EventSet;
use vm_memory::GuestMemoryMmap;

pub struct TsiStreamProxy {
    pub(crate) socket_policy: super::policy::SocketPolicy,
    id: u64,
    cid: u64,
    parent_id: u64,
    family: AddressFamily,
    local_port: u32,
    peer_port: u32,
    control_port: u32,
    fd: Option<OwnedFd>,
    pub status: ProxyStatus,
    mem: GuestMemoryMmap,
    queue: Arc<Mutex<VirtQueue>>,
    rxq: Arc<Mutex<MuxerRxQ>>,
    rx_cnt: Wrapping<u32>,
    tx_cnt: Wrapping<u32>,
    last_tx_cnt_sent: Wrapping<u32>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: Wrapping<u32>,
    push_cnt: Wrapping<u32>,
    pending_accepts: u64,
    unixsock_path: Option<PathBuf>,
    pending: super::stream_buffer::StreamBuffer,
    rx_paused: bool,
}

impl TsiStreamProxy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        cid: u64,
        family: u16,
        local_port: u32,
        peer_port: u32,
        control_port: u32,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
    ) -> Result<Self, ProxyError> {
        let family = match family {
            defs::LINUX_AF_INET => AddressFamily::Inet,
            defs::LINUX_AF_INET6 => AddressFamily::Inet6,
            #[cfg(target_os = "linux")]
            defs::LINUX_AF_UNIX => AddressFamily::Unix,
            _ => return Err(ProxyError::InvalidFamily),
        };
        Ok(TsiStreamProxy {
            socket_policy: Default::default(),
            id,
            cid,
            parent_id: 0,
            family,
            local_port,
            peer_port,
            control_port,
            fd: None,
            status: ProxyStatus::Idle,
            mem,
            queue,
            rxq,
            rx_cnt: Wrapping(0),
            tx_cnt: Wrapping(0),
            last_tx_cnt_sent: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            push_cnt: Wrapping(0),
            pending_accepts: 0,
            unixsock_path: None,
            pending: Default::default(),
            rx_paused: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_reverse(
        id: u64,
        cid: u64,
        parent_id: u64,
        family: AddressFamily,
        local_port: u32,
        peer_port: u32,
        fd: OwnedFd,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
    ) -> Self {
        debug!("new_reverse: id={id} local_port={local_port} peer_port={peer_port}");
        TsiStreamProxy {
            socket_policy: Default::default(),
            id,
            cid,
            parent_id,
            family,
            local_port,
            peer_port,
            control_port: 0,
            fd: Some(fd),
            status: ProxyStatus::ReverseInit,
            mem,
            queue,
            rxq,
            rx_cnt: Wrapping(0),
            tx_cnt: Wrapping(0),
            last_tx_cnt_sent: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            push_cnt: Wrapping(0),
            pending_accepts: 0,
            unixsock_path: None,
            pending: Default::default(),
            rx_paused: false,
        }
    }

    // A proxy can exist solely to reject a guest-local listen. Do not allocate a
    // host socket until an authorized operation actually needs one.
    fn ensure_socket(&mut self) -> nix::Result<()> {
        if self.fd.is_none() {
            let fd = socket(self.family, SockType::Stream, SockFlag::SOCK_NONBLOCK, None)?;
            if self.family == AddressFamily::Unix {
                setsockopt(&fd, sockopt::ReuseAddr, &true)?;
            } else {
                setsockopt(&fd, sockopt::ReusePort, &true)?;
            }
            self.fd = Some(fd);
        }
        Ok(())
    }

    fn init_data_pkt(&self, pkt: &mut VsockPacket) {
        debug!(
            "init_data_pkt: id={}, local_port={}, peer_port={}",
            self.id, self.local_port, self.peer_port
        );
        pkt.set_op(uapi::VSOCK_OP_RW)
            .set_src_cid(uapi::VSOCK_HOST_CID)
            .set_dst_cid(self.cid)
            .set_src_port(self.local_port)
            .set_dst_port(self.peer_port)
            .set_type(uapi::VSOCK_TYPE_STREAM)
            .set_buf_alloc(defs::CONN_TX_BUF_SIZE as u32)
            .set_fwd_cnt(self.tx_cnt.0);
    }

    fn try_listen(&mut self, req: &TsiListenReq, host_port_map: &Option<HashMap<u16, u16>>) -> i32 {
        if self.status == ProxyStatus::Listening || self.status == ProxyStatus::WaitingOnAccept {
            return 0;
        }

        let guest_port = req
            .addr
            .as_sockaddr_in()
            .map(|a| a.port())
            .or_else(|| req.addr.as_sockaddr_in6().map(|a| a.port()));
        if let Some(port) = guest_port {
            if !host_port_map
                .as_ref()
                .is_some_and(|map| map.contains_key(&port))
            {
                // EPERM selects the guest's native listener without a host socket.
                return -libc::EPERM;
            }
        }

        let addr: SockaddrStorage = if let Some(port_map) = host_port_map {
            if let Some(sin) = req.addr.as_sockaddr_in() {
                debug!("sockaddr is ipv4");
                if let Some(port) = port_map.get(&sin.port()) {
                    SocketAddrV4::new(sin.ip(), *port).into()
                } else {
                    req.addr
                }
            } else if let Some(sin6) = req.addr.as_sockaddr_in6() {
                debug!("sockaddr is ipv6");
                if let Some(port) = port_map.get(&sin6.port()) {
                    SocketAddrV6::new(sin6.ip(), *port, sin6.flowinfo(), sin6.scope_id()).into()
                } else {
                    req.addr
                }
            } else if req.addr.as_unix_addr().is_some() {
                debug!("sockaddr is unix");
                req.addr
            } else {
                return -libc::EINVAL;
            }
        } else {
            req.addr
        };

        use super::policy::SocketOperation;
        let bind_op = if addr.as_unix_addr().is_some() {
            SocketOperation::UnixBind
        } else {
            SocketOperation::Bind
        };
        if self.socket_policy.check(bind_op, &addr).is_err()
            || self
                .socket_policy
                .check(SocketOperation::Listen, &addr)
                .is_err()
            || (addr.as_unix_addr().is_some()
                && self
                    .socket_policy
                    .check(SocketOperation::UnixRemove, &addr)
                    .is_err())
        {
            return -libc::EPERM;
        }
        if let Err(error) = self.ensure_socket() {
            return -(error as i32);
        }
        let unixsock_path = self.get_unixsock_path(&addr);
        // If the userspace process in the guest has already created the socket,
        // we need to unlink it to take ownership of the node in the filesystem.
        if let Some(path) = &unixsock_path {
            if let Err(e) = fs::remove_file(path) {
                debug!("error removing socket: {e}");
            }
        }

        match bind(self.as_raw_fd(), &addr) {
            Ok(_) => {
                debug!("tcp bind: id={}", self.id);

                // For unix sockets we need to unlink the path on Drop, since
                // it's possible the userspace application can't do it itself.
                self.unixsock_path = unixsock_path;

                // Clamp backlog to SOMAXCONN, mirroring Linux kernel's __sys_listen behavior.
                // The nix crate's Backlog::new() rejects values above SOMAXCONN with EINVAL.
                let clamped_backlog = req.backlog.clamp(0, libc::SOMAXCONN);
                match Backlog::new(clamped_backlog) {
                    Ok(backlog) => match listen(
                        self.fd.as_ref().expect("authorized listener has a socket"),
                        backlog,
                    ) {
                        Ok(_) => {
                            debug!("proxy: id={}", self.id);
                            0
                        }
                        Err(e) => {
                            warn!("proxy: id={} err={}", self.id, e);
                            #[cfg(target_os = "linux")]
                            let errno = -(e as i32);
                            errno
                        }
                    },
                    Err(e) => {
                        warn!("proxy: id={} err={}", self.id, e);
                        #[cfg(target_os = "linux")]
                        let errno = -(e as i32);
                        errno
                    }
                }
            }
            Err(e) => {
                warn!("tcp bind: id={} err={}", self.id, e);
                #[cfg(target_os = "linux")]
                let errno = -(e as i32);
                errno
            }
        }
    }

    fn peer_avail_credit(&self) -> usize {
        self.peer_buf_alloc
            .saturating_sub((self.rx_cnt - self.peer_fwd_cnt).0) as usize
    }

    fn recv_to_pkt(&self, pkt: &mut VsockPacket) -> RecvPkt {
        if let Some(buf) = pkt.buf_mut() {
            let peer_credit = self.peer_avail_credit();
            let max_len = std::cmp::min(buf.len(), peer_credit);

            debug!(
                "recv_to_pkt: peer_avail_credit={}, buf.len={}, max_len={}",
                self.peer_avail_credit(),
                buf.len(),
                max_len,
            );

            if max_len == 0 {
                return RecvPkt::WaitForCredit;
            }

            match recv(
                self.as_raw_fd(),
                &mut buf[..max_len],
                MsgFlags::MSG_DONTWAIT,
            ) {
                Ok(cnt) => {
                    debug!("recv cnt={cnt}");
                    if cnt > 0 {
                        debug!("recv rx_cnt={}", self.rx_cnt);
                        RecvPkt::Read(cnt)
                    } else {
                        RecvPkt::Close
                    }
                }
                Err(e) => {
                    debug!("recv_pkt: recv error: {e:?}");
                    RecvPkt::Error
                }
            }
        } else {
            debug!("recv_pkt: pkt without buf");
            RecvPkt::Error
        }
    }

    fn recv_pkt(&mut self) -> (bool, bool) {
        let mut have_used = false;
        let mut wait_credit = false;
        let mut queue = self.queue.lock().unwrap();

        loop {
            let Some(head) = queue.pop(&self.mem) else {
                self.rx_paused = true;
                break;
            };
            let len = match VsockPacket::from_rx_virtq_head(&head) {
                Ok(mut pkt) => match self.recv_to_pkt(&mut pkt) {
                    RecvPkt::WaitForCredit => {
                        wait_credit = true;
                        0
                    }
                    RecvPkt::Read(cnt) => {
                        self.rx_cnt += Wrapping(cnt as u32);
                        self.init_data_pkt(&mut pkt);
                        pkt.set_len(cnt as u32);
                        pkt.hdr().len() + cnt
                    }
                    RecvPkt::Close => {
                        self.status = ProxyStatus::Closed;
                        0
                    }
                    RecvPkt::Error => 0,
                },
                Err(e) => {
                    debug!("recv_pkt: RX queue error: {e:?}");
                    0
                }
            };

            if len == 0 {
                queue.undo_pop();
                break;
            } else {
                have_used = true;
                self.push_cnt += Wrapping(len as u32);
                debug!(
                    "recv_pkt: pushing packet with {} bytes, push_cnt={}",
                    len, self.push_cnt
                );
                if let Err(e) = queue.add_used(&self.mem, head.index, len as u32) {
                    error!("failed to add used elements to the queue: {e:?}");
                }
            }
        }

        debug!("recv_pkt: have_used={have_used}");
        (have_used, wait_credit)
    }

    fn push_connect_rsp(&self, result: i32) {
        debug!(
            "push_connect_rsp: id: {}, control_port: {}, result: {}",
            self.id, self.control_port, result
        );

        // This response goes to the control port (DGRAM).
        let rx = MuxerRx::ConnResponse {
            local_port: 1025,
            peer_port: self.control_port,
            result,
        };
        self.push_pkt(rx);
    }

    fn push_reset(&self) {
        debug!(
            "push_reset: id: {}, peer_port: {}, local_port: {}",
            self.id, self.peer_port, self.local_port
        );

        // This response goes to the connection.
        let rx = MuxerRx::Reset {
            local_port: self.local_port,
            peer_port: self.peer_port,
        };
        self.push_pkt(rx);
    }

    fn switch_to_connected(&mut self) {
        self.status = ProxyStatus::Connected;
    }

    fn stream_events(&self) -> EventSet {
        let mut events = EventSet::empty();
        if self.status == ProxyStatus::Connected && !self.rx_paused {
            events |= EventSet::IN;
        }
        if !self.pending.is_empty() {
            events |= EventSet::OUT;
        }
        events
    }

    fn flush_pending(&mut self) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();
        match self.pending.flush(self.as_raw_fd()) {
            Ok(count) => self.tx_cnt += Wrapping(count as u32),
            Err(_) => {
                self.push_reset();
                self.status = ProxyStatus::Closed;
                update.signal_queue = true;
                update.remove_proxy = ProxyRemoval::Deferred;
                update.polling = Some((self.id, self.as_raw_fd(), EventSet::empty()));
                return update;
            }
        }
        if self.tx_cnt != self.last_tx_cnt_sent {
            self.last_tx_cnt_sent = self.tx_cnt;
            self.push_pkt(MuxerRx::CreditUpdate {
                local_port: self.local_port,
                peer_port: self.peer_port,
                fwd_cnt: self.tx_cnt.0,
            });
            update.signal_queue = true;
        }
        update.polling = Some((self.id, self.as_raw_fd(), self.stream_events()));
        update
    }

    fn get_addr_len(&self, addr: &SockaddrStorage) -> Option<u32> {
        let addr_len = match self.family {
            AddressFamily::Inet => addr.as_sockaddr_in()?.len(),
            AddressFamily::Inet6 => addr.as_sockaddr_in6()?.len(),
            AddressFamily::Unix => addr.as_unix_addr()?.len(),
            _ => 0,
        };

        Some(addr_len)
    }

    fn get_unixsock_path(&self, addr: &SockaddrStorage) -> Option<PathBuf> {
        if let Some(addr) = addr.as_unix_addr() {
            if let Some(path) = addr.path() {
                // SockaddrStorage doesn't clean up NULLs. This is fine when
                // using addr with other nix methods, but we need to clean them
                // up to be able to treat it as a path with other Rust crates.
                let path_str = path.to_str()?.replace("\0", "");
                debug!("unix socket path_str={path_str}");

                match fs::metadata(&path_str) {
                    Ok(metadata) => {
                        if metadata.file_type().is_socket() {
                            debug!("unix socket path is socket");
                            return PathBuf::from_str(&path_str).ok();
                        } else {
                            debug!("unix socket path is NOT a socket");
                        }
                    }
                    Err(e) => debug!("metadata failed with {e}"),
                }
            }
        }

        None
    }

    fn push_pkt(&self, rx: MuxerRx) {
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }
}

impl Proxy for TsiStreamProxy {
    fn id(&self) -> u64 {
        self.id
    }

    fn status(&self) -> ProxyStatus {
        self.status
    }

    fn connect(&mut self, _pkt: &VsockPacket, req: TsiConnectReq) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        let operation = if req.addr.as_unix_addr().is_some() {
            super::policy::SocketOperation::UnixConnect
        } else {
            super::policy::SocketOperation::TcpConnect
        };
        let permitted = self.socket_policy.check(operation, &req.addr);
        let result = match permitted
            .and_then(|()| self.ensure_socket())
            .and_then(|()| connect(self.as_raw_fd(), &req.addr))
        {
            Ok(()) => {
                debug!("connect: Connected");
                self.switch_to_connected();
                0
            }
            Err(nix::errno::Errno::EINPROGRESS) => {
                debug!("connect: Connecting");
                self.status = ProxyStatus::Connecting;
                0
            }
            Err(e) => {
                debug!("TcpProxy: Error connecting: {e}");
                #[cfg(target_os = "linux")]
                let errno = -(e as i32);
                errno
            }
        };

        if self.status == ProxyStatus::Connecting {
            update.polling = Some((
                self.id,
                self.as_raw_fd(),
                EventSet::OUT | EventSet::EDGE_TRIGGERED,
            ));
        } else {
            if self.status == ProxyStatus::Connected {
                update.polling = Some((self.id, self.as_raw_fd(), EventSet::IN));
            }
            self.push_connect_rsp(result);
        }

        update
    }

    fn confirm_connect(&mut self, pkt: &VsockPacket) -> Option<ProxyUpdate> {
        debug!(
            "confirm_connect: local_port={} peer_port={}, src_port={}, dst_port={}",
            pkt.dst_port(),
            pkt.src_port(),
            self.local_port,
            self.peer_port,
        );

        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        self.local_port = pkt.dst_port();
        self.peer_port = pkt.src_port();

        // This response goes to the connection.
        let rx = MuxerRx::OpResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
        };
        self.push_pkt(rx);

        // Now that the vsock transport is fully established, start listening
        // for events in the TCP socket again.
        Some(ProxyUpdate {
            polling: Some((self.id, self.as_raw_fd(), EventSet::IN)),
            ..Default::default()
        })
    }

    fn getpeername(&mut self, pkt: &VsockPacket) {
        debug!("getpeername: id={}", self.id);

        let (result, addr_len, addr): (i32, u32, SockaddrStorage) =
            match getpeername(self.as_raw_fd()) {
                Ok(addr) => {
                    if let Some(addr_len) = self.get_addr_len(&addr) {
                        (0, addr_len, addr)
                    } else {
                        #[cfg(target_os = "linux")]
                        let errno = -EINVAL;
                        (errno, 0, addr)
                    }
                }
                Err(e) => {
                    #[cfg(target_os = "linux")]
                    let errno = -(e as i32);
                    (
                        errno,
                        0,
                        SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0).into(),
                    )
                }
            };

        let data = TsiGetnameRsp {
            result,
            addr_len,
            addr,
        };

        debug!("getpeername: reply={data:?}");

        // This response goes to the control port (DGRAM).
        let rx = MuxerRx::GetnameResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
            data,
        };
        self.push_pkt(rx);
    }

    fn sendmsg(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        if pkt
            .buf()
            .and_then(|buf| buf.get(..pkt.len() as usize))
            .is_none_or(|buf| self.pending.enqueue(buf).is_err())
        {
            self.push_reset();
            self.status = ProxyStatus::Closed;
            return ProxyUpdate {
                signal_queue: true,
                remove_proxy: ProxyRemoval::Deferred,
                polling: Some((self.id, self.as_raw_fd(), EventSet::empty())),
                ..Default::default()
            };
        }
        self.flush_pending()
    }

    fn sendto_addr(&mut self, _req: TsiSendtoAddr) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn listen(
        &mut self,
        pkt: &VsockPacket,
        req: TsiListenReq,
        host_port_map: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate {
        debug!(
            "listen: id={} addr={}, vm_port={} backlog={}",
            self.id, req.addr, req.vm_port, req.backlog
        );
        let mut update = ProxyUpdate::default();

        let result = self.try_listen(&req, host_port_map);

        // This packet goes to the control port (DGRAM).
        let rx = MuxerRx::ListenResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
            result,
        };
        self.push_pkt(rx);
        update.signal_queue = true;

        if result == 0 {
            self.peer_port = req.vm_port;
            self.status = ProxyStatus::Listening;
            update.polling = Some((self.id, self.as_raw_fd(), EventSet::IN));
        }

        update
    }

    fn accept(&mut self, req: TsiAcceptReq) -> ProxyUpdate {
        debug!("accept: id={} flags={}", req.peer_port, req.flags);

        let mut update = ProxyUpdate::default();

        if self.pending_accepts > 0 {
            self.pending_accepts -= 1;
            self.push_accept_rsp(0);
            update.signal_queue = true;
        } else if (req.flags & libc::O_NONBLOCK as u32) != 0 {
            self.push_accept_rsp(-libc::EWOULDBLOCK);
            update.signal_queue = true;
        } else {
            self.status = ProxyStatus::WaitingOnAccept;
        }

        update
    }

    fn update_peer_credit(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        debug!(
            "update_credit: buf_alloc={} rx_cnt={} fwd_cnt={}",
            pkt.buf_alloc(),
            self.rx_cnt,
            pkt.fwd_cnt()
        );
        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        if self.status == ProxyStatus::WaitingCreditUpdate && self.peer_avail_credit() > 0 {
            self.status = ProxyStatus::Connected;
        }
        let request = pkt.op() == uapi::VSOCK_OP_CREDIT_REQUEST;
        if request {
            self.push_pkt(MuxerRx::CreditUpdate {
                local_port: self.local_port,
                peer_port: self.peer_port,
                fwd_cnt: self.tx_cnt.0,
            });
            self.last_tx_cnt_sent = self.tx_cnt;
        }
        ProxyUpdate {
            signal_queue: request,
            polling: Some((self.id, self.as_raw_fd(), self.stream_events())),
            ..Default::default()
        }
    }

    fn push_op_request(&self) {
        debug!(
            "push_op_request: id={}, local_port={} peer_port={}",
            self.id, self.local_port, self.peer_port
        );

        // This packet goes to the connection.
        let rx = MuxerRx::OpRequest {
            local_port: self.local_port,
            peer_port: self.peer_port,
        };
        self.push_pkt(rx);
    }

    fn process_op_response(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        debug!(
            "process_op_response: id={} src_port={} dst_port={}",
            self.id,
            pkt.src_port(),
            pkt.dst_port()
        );

        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        self.switch_to_connected();

        ProxyUpdate {
            polling: Some((self.id, self.as_raw_fd(), EventSet::IN)),
            push_accept: Some((self.id, self.parent_id)),
            ..Default::default()
        }
    }

    fn enqueue_accept(&mut self) {
        debug!("enqueue_accept: control_port: {}", self.control_port);

        if self.status == ProxyStatus::WaitingOnAccept {
            self.status = ProxyStatus::Listening;
            self.push_accept_rsp(0);
        } else {
            self.pending_accepts += 1;
        }
    }

    fn push_accept_rsp(&self, result: i32) {
        debug!(
            "push_accept_rsp: control_port: {}, result: {}",
            self.control_port, result
        );

        // This packet goes to the control port (DGRAM).
        let rx = MuxerRx::AcceptResponse {
            local_port: 1030,
            peer_port: self.control_port,
            result,
        };
        self.push_pkt(rx);
    }

    fn shutdown(&mut self, pkt: &VsockPacket) {
        let recv_off = pkt.flags() & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0;
        let send_off = pkt.flags() & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0;

        if send_off {
            self.pending.shutdown_write(self.as_raw_fd());
        }
        if recv_off {
            if let Err(e) = shutdown(self.as_raw_fd(), Shutdown::Read) {
                debug!("error sending shutdown to socket: {e}");
            }
        }
    }

    fn release(&mut self) -> ProxyUpdate {
        debug!(
            "release: id={}, tx_cnt={}, last_tx_cnt={}",
            self.id, self.tx_cnt, self.last_tx_cnt_sent
        );
        let remove_proxy = if self.status == ProxyStatus::Listening {
            ProxyRemoval::Immediate
        } else {
            ProxyRemoval::Deferred
        };
        ProxyUpdate {
            remove_proxy,
            ..Default::default()
        }
    }

    fn resume_rx(&mut self) -> Option<ProxyUpdate> {
        if !self.rx_paused {
            return None;
        }
        self.rx_paused = false;
        Some(ProxyUpdate {
            polling: Some((self.id, self.as_raw_fd(), self.stream_events())),
            ..Default::default()
        })
    }

    fn process_event(&mut self, evset: EventSet) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        if evset.contains(EventSet::HANG_UP) {
            debug!("process_event: HANG_UP");
            if self.status == ProxyStatus::Connecting {
                self.push_connect_rsp(-libc::ECONNREFUSED);
            } else {
                self.push_reset();
            }

            self.status = ProxyStatus::Closed;
            update.polling = Some((self.id, self.as_raw_fd(), EventSet::empty()));
            update.signal_queue = true;
            update.remove_proxy = if self.status == ProxyStatus::Listening {
                ProxyRemoval::Immediate
            } else {
                ProxyRemoval::Deferred
            };
            return update;
        }

        if evset.contains(EventSet::IN) {
            debug!("process_event: IN");
            if self.status == ProxyStatus::Connected {
                let (signal_queue, wait_credit) = self.recv_pkt();
                update.signal_queue = signal_queue;
                update.polling = Some((self.id, self.as_raw_fd(), self.stream_events()));

                if wait_credit && self.status != ProxyStatus::WaitingCreditUpdate {
                    self.status = ProxyStatus::WaitingCreditUpdate;
                    let rx = MuxerRx::CreditRequest {
                        local_port: self.local_port,
                        peer_port: self.peer_port,
                        fwd_cnt: self.tx_cnt.0,
                    };
                    update.push_credit_req = Some(rx);
                }

                if self.status == ProxyStatus::Closed {
                    debug!(
                        "process_event: endpoint closed, sending reset: id={}",
                        self.id
                    );
                    self.push_reset();
                    update.signal_queue = true;
                    update.polling = Some((self.id(), self.as_raw_fd(), EventSet::empty()));
                    return update;
                } else if self.status == ProxyStatus::WaitingCreditUpdate {
                    debug!("process_event: WaitingCreditUpdate");
                    update.polling = Some((self.id(), self.as_raw_fd(), self.stream_events()));
                }
            } else if self.status == ProxyStatus::Listening
                || self.status == ProxyStatus::WaitingOnAccept
            {
                match accept(self.as_raw_fd()) {
                    Ok(accept_fd) => {
                        // Safe because we've just obtained the FD from the `accept` call above.
                        let new_fd = unsafe { OwnedFd::from_raw_fd(accept_fd) };
                        if nix::sys::socket::getsockname::<SockaddrStorage>(accept_fd).map_or(true, |addr| {
                            self.socket_policy
                                .check(super::policy::SocketOperation::Accept, &addr)
                                .is_err()
                        }) {
                            return update;
                        }
                        update.new_proxy =
                            Some((self.peer_port, new_fd, self.family, NewProxyType::Tcp));
                    }
                    Err(e) => warn!("error accepting connection: id={}, err={}", self.id, e),
                };
                update.signal_queue = true;
                return update;
            } else {
                debug!("EventSet::IN while not connected: {:?}", self.status);
            }
        }

        if evset.contains(EventSet::OUT) {
            debug!("process_event: OUT");
            if self.status == ProxyStatus::Connecting {
                self.switch_to_connected();
                self.push_connect_rsp(0);
                update.signal_queue = true;
                // Stop listening for events in the TCP socket until we receive
                // OP_REQUEST and the vsock transport is fully established.
                update.polling = Some((self.id(), self.as_raw_fd(), EventSet::empty()));
            } else if self.status == ProxyStatus::Connected
                || self.status == ProxyStatus::WaitingCreditUpdate
            {
                let flushed = self.flush_pending();
                update.signal_queue |= flushed.signal_queue;
                update.polling = flushed.polling;
                update.remove_proxy = flushed.remove_proxy;
            }
        }

        update
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestAddress;

    #[test]
    fn refused_listen_returns_eperm_and_never_opens_host_socket() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut proxy = TsiStreamProxy::new(
            1, 42, defs::LINUX_AF_INET, 1234, 5678, 9012, mem,
            Arc::new(Mutex::new(VirtQueue::new(256))),
            Arc::new(Mutex::new(MuxerRxQ::new())),
        ).unwrap();
        let req = TsiListenReq {
            peer_port: 5678,
            vm_port: 1234,
            backlog: 8,
            addr: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 34567).into(),
        };
        // Unmapped listen must be rejected even when Internet forwarding is enabled.
        proxy.socket_policy.internet_enabled = true;
        assert_eq!(proxy.try_listen(&req, &None), -libc::EPERM);
        assert!(proxy.fd.is_none());
        assert_eq!(proxy.status, ProxyStatus::Idle);
        // A mapping does not bypass the independent policy refusal.
        proxy.socket_policy.internet_enabled = false;
        let mapping = Some(HashMap::from([(34567, 34568)]));
        assert_eq!(proxy.try_listen(&req, &mapping), -libc::EPERM);
        assert!(proxy.fd.is_none());
    }
}

impl AsRawFd for TsiStreamProxy {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }
}

impl Drop for TsiStreamProxy {
    fn drop(&mut self) {
        if let Some(path) = &self.unixsock_path {
            if let Ok(addr) = nix::sys::socket::UnixAddr::new(path) {
                // SAFETY: addr owns a valid sockaddr_un for its reported length.
                let storage =
                    unsafe { SockaddrStorage::from_raw(addr.as_ptr().cast(), Some(addr.len())) }
                        .unwrap();
                if self
                    .socket_policy
                    .check(super::policy::SocketOperation::UnixRemove, &storage)
                    .is_ok()
                {
                    _ = fs::remove_file(path);
                }
            }
        }
    }
}
