use super::{
    defs::{self, uapi},
    proxy::{ProxyRemoval, RecvPkt},
};

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::socket::SockaddrLike;
use nix::sys::socket::{
    accept, bind, connect, listen, recv, shutdown, socket, AddressFamily, Backlog, MsgFlags,
    Shutdown, SockFlag, SockType, UnixAddr,
};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::num::Wrapping;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::super::Queue as VirtQueue;
use super::muxer::{push_packet, MuxerRx};
use super::muxer_rxq::MuxerRxQ;
use super::packet::{TsiAcceptReq, TsiConnectReq, TsiListenReq, TsiSendtoAddr, VsockPacket};
use super::proxy::{NewProxyType, Proxy, ProxyError, ProxyStatus, ProxyUpdate};
use utils::epoll::EventSet;
use vm_memory::GuestMemoryMmap;

pub struct UnixProxy {
    pub(crate) socket_policy: super::policy::SocketPolicy,
    id: u64,
    cid: u64,
    fd: OwnedFd,
    pub status: ProxyStatus,
    mem: GuestMemoryMmap,
    queue: Arc<Mutex<VirtQueue>>,
    rxq: Arc<Mutex<MuxerRxQ>>,
    path: PathBuf,
    peer_port: u32,
    local_port: u32,
    control_port: u32,
    peer_fwd_cnt: Wrapping<u32>,
    peer_buf_alloc: u32,
    tx_cnt: Wrapping<u32>,
    last_tx_cnt_sent: Wrapping<u32>,
    push_cnt: Wrapping<u32>,
    rx_cnt: Wrapping<u32>,
    pending: super::stream_buffer::StreamBuffer,
    rx_paused: bool,
}

fn proxy_fd_create(id: u64) -> Result<OwnedFd, ProxyError> {
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(ProxyError::CreatingSocket)?;

    // Set non-blocking mode before using the socket.
    match fcntl(&fd, FcntlArg::F_GETFL) {
        Ok(flags) => match OFlag::from_bits(flags) {
            Some(flags) => {
                if let Err(e) = fcntl(&fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)) {
                    warn!("error switching to non-blocking: id={id}, err={e}");
                }
            }
            None => error!("invalid fd flags id={id}"),
        },
        Err(e) => error!("couldn't obtain fd flags id={id}, err={e}"),
    };

    Ok(fd)
}

impl UnixProxy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        cid: u64,
        local_port: u32,
        control_port: u32,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
        path: PathBuf,
    ) -> Result<Self, ProxyError> {
        let fd = proxy_fd_create(id)?;

        Ok(UnixProxy {
            socket_policy: Default::default(),
            id,
            cid,
            local_port,
            peer_port: 0,
            control_port,
            fd,
            status: ProxyStatus::Idle,
            mem,
            queue,
            rxq,
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            path,
            tx_cnt: Wrapping(0),
            last_tx_cnt_sent: Wrapping(0),
            pending: Default::default(),
            rx_paused: false,
            push_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_reverse(
        id: u64,
        cid: u64,
        local_port: u32,
        peer_port: u32,
        fd: OwnedFd,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
    ) -> Self {
        debug!("new_reverse: id={id} local_port={local_port} peer_port={peer_port}");
        UnixProxy {
            socket_policy: Default::default(),
            id,
            cid,
            local_port,
            peer_port,
            control_port: 0,
            fd,
            status: ProxyStatus::ReverseInit,
            mem,
            queue,
            rxq,
            rx_cnt: Wrapping(0),
            tx_cnt: Wrapping(0),
            last_tx_cnt_sent: Wrapping(0),
            pending: Default::default(),
            rx_paused: false,
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            push_cnt: Wrapping(0),
            path: Default::default(),
        }
    }

    fn push_pkt(&self, rx: MuxerRx) {
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
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
        match self.pending.flush(self.fd.as_raw_fd()) {
            Ok(count) => self.tx_cnt += Wrapping(count as u32),
            Err(_) => {
                self.push_reset();
                self.close_host_socket();
                self.status = ProxyStatus::Closed;
                update.signal_queue = true;
                update.remove_proxy = ProxyRemoval::Deferred;
                update.polling = Some((self.id, self.fd.as_raw_fd(), EventSet::empty()));
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
        update.polling = Some((self.id, self.fd.as_raw_fd(), self.stream_events()));
        update
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

    /// The connection is over (the guest sent RST, or this proxy reset the guest after the host
    /// side ended or failed): shut the host socket down in both directions now, so the host peer
    /// reads end of file at once. Only the proxy's own removal is deferred (to the reaper, about
    /// 5 s later); before this, the host socket stayed open until then, and a host service that
    /// counts open connections (the holodeck daemon's relay limit) still counted it.
    fn close_host_socket(&self) {
        if let Err(e) = shutdown(self.fd.as_raw_fd(), Shutdown::Both) {
            debug!("close_host_socket: id={}: {e}", self.id);
        }
    }

    fn push_reset(&self) {
        debug!(
            "push_reset: id: {}, peer_port: {}, local_port: {}",
            self.id, self.peer_port, self.local_port
        );

        let rx = MuxerRx::Reset {
            local_port: self.local_port,
            peer_port: self.peer_port,
        };

        self.push_pkt(rx);
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
                max_len
            );

            if max_len == 0 {
                return RecvPkt::WaitForCredit;
            }

            match recv(
                self.fd.as_raw_fd(),
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
}

impl Proxy for UnixProxy {
    fn id(&self) -> u64 {
        self.id
    }

    fn status(&self) -> ProxyStatus {
        self.status
    }

    fn connect(&mut self, _pkt: &VsockPacket, _req: TsiConnectReq) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        let addr = UnixAddr::new(&self.path).unwrap();

        // SAFETY: addr owns a valid sockaddr_un for its reported length.
        let storage = unsafe {
            nix::sys::socket::SockaddrStorage::from_raw(addr.as_ptr().cast(), Some(addr.len()))
        }
        .unwrap();
        let result = match self
            .socket_policy
            .check(super::policy::SocketOperation::UnixConnect, &storage)
            .and_then(|()| connect(self.fd.as_raw_fd(), &addr))
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
                debug!("Error connecting: {e}");
                #[cfg(target_os = "linux")]
                let errno = -(e as i32);
                errno
            }
        };

        if self.status == ProxyStatus::Connecting {
            update.polling = Some((self.id, self.fd.as_raw_fd(), EventSet::IN | EventSet::OUT));
        } else {
            if self.status == ProxyStatus::Connected {
                update.polling = Some((self.id, self.fd.as_raw_fd(), EventSet::IN));
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

        None
    }

    fn getpeername(&mut self, _pkt: &VsockPacket) {
        todo!();
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
                polling: Some((self.id, self.fd.as_raw_fd(), EventSet::empty())),
                ..Default::default()
            };
        }
        self.flush_pending()
    }

    fn sendto_addr(&mut self, _req: TsiSendtoAddr) -> ProxyUpdate {
        todo!();
    }

    fn listen(
        &mut self,
        _pkt: &VsockPacket,
        _req: TsiListenReq,
        _host_port_map: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate {
        todo!();
    }

    fn accept(&mut self, _req: TsiAcceptReq) -> ProxyUpdate {
        todo!();
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
            polling: Some((self.id, self.fd.as_raw_fd(), self.stream_events())),
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
            polling: Some((self.id, self.fd.as_raw_fd(), EventSet::IN)),
            ..Default::default()
        }
    }

    fn enqueue_accept(&mut self) {
        todo!();
    }

    fn shutdown(&mut self, pkt: &VsockPacket) {
        let recv_off = pkt.flags() & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0;
        let send_off = pkt.flags() & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0;

        if send_off {
            self.pending.shutdown_write(self.fd.as_raw_fd());
        }
        if recv_off {
            if let Err(e) = shutdown(self.fd.as_raw_fd(), Shutdown::Read) {
                debug!("error sending shutdown to socket: {e}");
            }
        }
    }

    fn release(&mut self) -> ProxyUpdate {
        debug!(
            "release: id={}, tx_cnt={}, last_tx_cnt={}",
            self.id, self.tx_cnt, self.last_tx_cnt_sent
        );
        // The guest reset the connection: nothing more crosses it in either direction.
        self.status = ProxyStatus::Closed;
        self.close_host_socket();
        let remove_proxy = ProxyRemoval::Deferred;

        ProxyUpdate {
            remove_proxy,
            polling: Some((self.id, self.fd.as_raw_fd(), EventSet::empty())),
            ..Default::default()
        }
    }

    fn resume_rx(&mut self) -> Option<ProxyUpdate> {
        if !self.rx_paused {
            return None;
        }
        self.rx_paused = false;
        Some(ProxyUpdate {
            polling: Some((self.id, self.fd.as_raw_fd(), self.stream_events())),
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
            self.close_host_socket();

            self.status = ProxyStatus::Closed;
            update.polling = Some((self.id, self.fd.as_raw_fd(), EventSet::empty()));
            update.signal_queue = true;
            update.remove_proxy = ProxyRemoval::Deferred;

            return update;
        }

        if evset.contains(EventSet::IN) {
            debug!("process_event: IN");
            if self.status == ProxyStatus::Connected {
                let (signal_queue, wait_credit) = self.recv_pkt();
                update.signal_queue = signal_queue;
                update.polling = Some((self.id, self.fd.as_raw_fd(), self.stream_events()));

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
                    self.close_host_socket();
                    update.signal_queue = true;
                    update.polling = Some((self.id(), self.fd.as_raw_fd(), EventSet::empty()));
                    return update;
                } else if self.status == ProxyStatus::WaitingCreditUpdate {
                    debug!("process_event: WaitingCreditUpdate");
                    update.polling = Some((self.id(), self.fd.as_raw_fd(), self.stream_events()));
                }
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
                update.polling = Some((self.id(), self.fd.as_raw_fd(), EventSet::IN));
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

impl AsRawFd for UnixProxy {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

pub struct UnixAcceptorProxy {
    pub(crate) socket_policy: super::policy::SocketPolicy,
    id: u64,
    fd: OwnedFd,
    peer_port: u32,
}

impl UnixAcceptorProxy {
    pub fn new(id: u64, path: &PathBuf, peer_port: u32) -> Result<Self, ProxyError> {
        match std::fs::remove_file(path) {
            Ok(()) => {
                debug!("removed stale unix vsock listener path: {:?}", path);
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => {
                warn!(
                    "failed to remove unix vsock listener path {:?}: {err}",
                    path
                );
            }
        }

        let fd = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .map_err(ProxyError::CreatingSocket)?;
        bind(
            fd.as_raw_fd(),
            &UnixAddr::new(path).map_err(ProxyError::CreatingSocket)?,
        )
        .map_err(ProxyError::CreatingSocket)?;
        listen(&fd, Backlog::new(5).map_err(ProxyError::CreatingSocket)?)
            .map_err(ProxyError::CreatingSocket)?;
        Ok(UnixAcceptorProxy {
            id,
            fd,
            peer_port,
            socket_policy: Default::default(),
        })
    }
}

impl Proxy for UnixAcceptorProxy {
    fn id(&self) -> u64 {
        self.id
    }
    fn status(&self) -> ProxyStatus {
        ProxyStatus::WaitingOnAccept
    }
    fn connect(&mut self, _: &VsockPacket, _: TsiConnectReq) -> ProxyUpdate {
        unreachable!()
    }
    fn getpeername(&mut self, _: &VsockPacket) {
        unreachable!()
    }
    fn sendmsg(&mut self, _: &VsockPacket) -> ProxyUpdate {
        unreachable!()
    }
    fn sendto_addr(&mut self, _: TsiSendtoAddr) -> ProxyUpdate {
        unreachable!()
    }
    fn listen(
        &mut self,
        _: &VsockPacket,
        _: TsiListenReq,
        _: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate {
        unreachable!()
    }
    fn accept(&mut self, _: TsiAcceptReq) -> ProxyUpdate {
        unreachable!()
    }
    fn update_peer_credit(&mut self, _: &VsockPacket) -> ProxyUpdate {
        unreachable!()
    }
    fn process_op_response(&mut self, _: &VsockPacket) -> ProxyUpdate {
        unreachable!()
    }
    fn release(&mut self) -> ProxyUpdate {
        unreachable!()
    }
    fn process_event(&mut self, evset: EventSet) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        if evset.contains(EventSet::HANG_UP) {
            debug!("process_event: HANG_UP");
            update.polling = Some((self.id, self.fd.as_raw_fd(), EventSet::empty()));
            update.signal_queue = true;
            update.remove_proxy = ProxyRemoval::Deferred;
            return update;
        }
        if evset.contains(EventSet::IN) {
            match accept(self.fd.as_raw_fd()) {
                Ok(accept_fd) => {
                    // Safe because we've just obtained the FD from the `accept` call above.
                    let new_fd = unsafe { OwnedFd::from_raw_fd(accept_fd) };
                    if nix::sys::socket::getpeername::<nix::sys::socket::SockaddrStorage>(accept_fd)
                        .map_or(true, |addr| {
                            self.socket_policy
                                .check(super::policy::SocketOperation::Accept, &addr)
                                .is_err()
                        })
                    {
                        return update;
                    }
                    update.new_proxy = Some((
                        self.peer_port,
                        new_fd,
                        AddressFamily::Unix,
                        NewProxyType::Unix,
                    ));
                }
                Err(e) => warn!("error accepting connection: id={}, err={}", self.id, e),
            };
            update.signal_queue = true;
        }
        update
    }
}

impl AsRawFd for UnixAcceptorProxy {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use vm_memory::GuestAddress;

    #[test]
    fn guest_reset_gives_the_host_peer_end_of_file_at_once() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let (proxy_side, mut host) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut proxy = UnixProxy::new_reverse(
            1,
            3,
            1024,
            7004,
            OwnedFd::from(proxy_side),
            mem,
            Arc::new(Mutex::new(VirtQueue::new(16))),
            Arc::new(Mutex::new(MuxerRxQ::new())),
        );
        let update = proxy.release();
        assert!(matches!(update.remove_proxy, ProxyRemoval::Deferred));
        // The proxy (and its descriptor) still exists until the reaper removes it, but the host
        // peer already reads end of file: no wait for the reaper.
        host.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        assert_eq!(host.read(&mut [0u8; 8]).unwrap(), 0);
        drop(proxy);
    }
}
