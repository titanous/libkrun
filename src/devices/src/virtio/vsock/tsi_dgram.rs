use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::num::Wrapping;
use std::os::fd::OwnedFd;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::socket::{
    bind, connect, getpeername, recvfrom, send, sendto, socket, AddressFamily, MsgFlags, SockFlag,
    SockType, SockaddrLike, SockaddrStorage, UnixAddr,
};

use super::super::Queue as VirtQueue;
use super::defs;
use super::defs::uapi;
use super::muxer::{push_packet, MuxerRx};
use super::muxer_rxq::MuxerRxQ;
use super::packet::{
    TsiAcceptReq, TsiConnectReq, TsiGetnameRsp, TsiListenReq, TsiSendtoAddr, VsockPacket,
};
use super::policy::SocketOperation;
use super::proxy::{Proxy, ProxyError, ProxyRemoval, ProxyStatus, ProxyUpdate, RecvPkt};
use utils::epoll::EventSet;
use vm_memory::GuestMemoryMmap;

fn wildcard_address(family: AddressFamily) -> Option<SockaddrStorage> {
    match family {
        AddressFamily::Inet => Some(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()),
        AddressFamily::Inet6 => {
            Some(std::net::SocketAddrV6::new(std::net::Ipv6Addr::UNSPECIFIED, 0, 0, 0).into())
        }
        AddressFamily::Unix => {
            let addr = UnixAddr::new_unnamed();
            // SAFETY: addr owns a valid unnamed sockaddr_un.
            unsafe { SockaddrStorage::from_raw(addr.as_ptr().cast(), Some(addr.len())) }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unix_datagram_autobind_can_send() {
        let file = utils::tempfile::TempFile::new().unwrap();
        std::fs::remove_file(file.as_path()).unwrap();
        let receiver = std::os::unix::net::UnixDatagram::bind(file.as_path()).unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let sender = socket(
            AddressFamily::Unix,
            SockType::Datagram,
            SockFlag::empty(),
            None,
        )
        .unwrap();
        bind(
            sender.as_raw_fd(),
            &wildcard_address(AddressFamily::Unix).unwrap(),
        )
        .unwrap();
        let destination = UnixAddr::new(file.as_path()).unwrap();
        assert_eq!(
            sendto(sender.as_raw_fd(), b"unix", &destination, MsgFlags::empty()).unwrap(),
            4
        );
        let mut data = [0; 4];
        assert_eq!(receiver.recv(&mut data).unwrap(), 4);
        assert_eq!(&data, b"unix");
    }
}

pub struct TsiDgramProxy {
    pub(crate) socket_policy: super::policy::SocketPolicy,
    family: AddressFamily,
    pub id: u64,
    cid: u64,
    local_port: u32,
    peer_port: u32,
    fd: OwnedFd,
    pub status: ProxyStatus,
    sendto_addr: Option<SockaddrStorage>,
    listening: bool,
    mem: GuestMemoryMmap,
    queue: Arc<Mutex<VirtQueue>>,
    rxq: Arc<Mutex<MuxerRxQ>>,
    rx_cnt: Wrapping<u32>,
    tx_cnt: Wrapping<u32>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: Wrapping<u32>,
}

impl TsiDgramProxy {
    pub fn new(
        id: u64,
        cid: u64,
        family: u16,
        peer_port: u32,
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

        let fd = socket(family, SockType::Datagram, SockFlag::empty(), None)
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

        Ok(TsiDgramProxy {
            socket_policy: Default::default(),
            family,
            id,
            cid,
            local_port: 0,
            peer_port,
            fd,
            status: ProxyStatus::Idle,
            sendto_addr: None,
            listening: false,
            mem,
            queue,
            rxq,
            rx_cnt: Wrapping(0),
            tx_cnt: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
        })
    }

    fn init_pkt(&self, pkt: &mut VsockPacket) {
        debug!(
            "init_pkt: id={}, src_port={}, dst_port={}",
            self.id, self.local_port, self.peer_port
        );
        pkt.set_op(uapi::VSOCK_OP_RW)
            .set_src_cid(self.cid)
            .set_dst_cid(uapi::VSOCK_HOST_CID)
            .set_dst_port(self.peer_port)
            .set_src_port(0)
            .set_type(uapi::VSOCK_TYPE_DGRAM)
            .set_buf_alloc(defs::CONN_TX_BUF_SIZE as u32)
            .set_fwd_cnt(self.tx_cnt.0);
    }

    fn push_pkt(&self, rx: MuxerRx) {
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }

    /*
    fn peer_avail_credit(&self) -> usize {
        (Wrapping(self.peer_buf_alloc) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
    }

    fn send_credit_request(&self) {
        // This response goes to the connection.
        let rx = MuxerRx::CreditRequest {
            local_port: self.local_port,
            peer_port: self.peer_port,
            fwd_cnt: self.tx_cnt.0,
        };
        self.push_pkt(rx);
    }
    */

    fn send_operation(&self) -> SocketOperation {
        if self.family == AddressFamily::Unix {
            SocketOperation::UnixDatagramSend
        } else {
            SocketOperation::UdpSend
        }
    }

    fn recv_to_pkt(&self, pkt: &mut VsockPacket) -> RecvPkt {
        if let Some(buf) = pkt.buf_mut() {
            // Disable UDP credit accounting until is fixed in the kernel
            //let peer_credit = self.peer_avail_credit();
            //let max_len = std::cmp::min(buf.len(), peer_credit);
            let max_len = buf.len();

            /*
            debug!(
                "recv_to_pkt: peer_avail_credit={}, buf.len={}, max_len={}",
                self.peer_avail_credit(),
                buf.len(),
                max_len,
            );

            if max_len == 0 {
                return RecvPkt::WaitForCredit;
            }
            */

            let mut data = vec![0; max_len];
            match recvfrom::<SockaddrStorage>(self.fd.as_raw_fd(), &mut data) {
                Ok((cnt, source)) => {
                    let source = source.or_else(|| getpeername(self.fd.as_raw_fd()).ok());
                    if source.is_none_or(|addr| {
                        self.socket_policy
                            .check(SocketOperation::ReceiveDatagram, &addr)
                            .is_err()
                    }) {
                        return RecvPkt::Error;
                    }
                    buf[..cnt].copy_from_slice(&data[..cnt]);
                    debug!("recv cnt={cnt}");
                    if cnt > 0 {
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

        while let Some(head) = queue.pop(&self.mem) {
            let len = match VsockPacket::from_rx_virtq_head(&head) {
                Ok(mut pkt) => match self.recv_to_pkt(&mut pkt) {
                    RecvPkt::WaitForCredit => {
                        wait_credit = true;
                        0
                    }
                    RecvPkt::Read(cnt) => {
                        self.rx_cnt += Wrapping(cnt as u32);
                        self.init_pkt(&mut pkt);
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
                debug!("recv_pkt: pushing packet with {len} bytes");
                if let Err(e) = queue.add_used(&self.mem, head.index, len as u32) {
                    error!("failed to add used elements to the queue: {e:?}");
                }
            }
        }

        debug!("recv_pkt: have_used={have_used}");
        (have_used, wait_credit)
    }
}

impl Proxy for TsiDgramProxy {
    fn id(&self) -> u64 {
        self.id
    }

    fn status(&self) -> ProxyStatus {
        self.status
    }

    fn connect(&mut self, pkt: &VsockPacket, req: TsiConnectReq) -> ProxyUpdate {
        debug!("connect: addr={}", req.addr);
        let operation = if self.family == AddressFamily::Unix {
            SocketOperation::UnixDatagramConnect
        } else {
            SocketOperation::UdpConnect
        };
        let res = match self
            .socket_policy
            .check(operation, &req.addr)
            .and_then(|()| connect(self.fd.as_raw_fd(), &req.addr))
        {
            Ok(()) => {
                debug!("connect: Connected");
                self.status = ProxyStatus::Connected;
                0
            }
            Err(e) => {
                debug!("Error connecting: {e}");
                #[cfg(target_os = "linux")]
                let errno = -(e as i32);
                errno
            }
        };

        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        // This response goes to the connection.
        let rx = MuxerRx::ConnResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
            result: res,
        };
        self.push_pkt(rx);

        let mut update = ProxyUpdate::default();
        if res == 0 && !self.listening {
            update.polling = Some((self.id, self.fd.as_raw_fd(), EventSet::IN));
        }
        update
    }

    fn getpeername(&mut self, pkt: &VsockPacket) {
        debug!("process_getpeername");

        let (result, addr): (i32, SockaddrStorage) = match getpeername(self.fd.as_raw_fd()) {
            Ok(name) => (0, name),
            Err(e) => {
                #[cfg(target_os = "linux")]
                let errno = -(e as i32);
                (
                    errno,
                    SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0).into(),
                )
            }
        };

        let data = TsiGetnameRsp {
            result,
            addr_len: addr.len(),
            addr,
        };

        // This response goes to the connection.
        let rx = MuxerRx::GetnameResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
            data,
        };
        self.push_pkt(rx);
    }

    fn sendmsg(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        debug!("sendmsg");

        let ret = if let Some(buf) = pkt.buf() {
            #[cfg(target_os = "linux")]
            let flags = MsgFlags::MSG_NOSIGNAL;

            let authorization = getpeername::<SockaddrStorage>(self.fd.as_raw_fd())
                .and_then(|addr| self.socket_policy.check(self.send_operation(), &addr));
            match authorization.and_then(|()| send(self.fd.as_raw_fd(), buf, flags)) {
                Ok(sent) => {
                    self.tx_cnt += Wrapping(sent as u32);
                    sent as i32
                }
                Err(err) => -(err as i32),
            }
        } else {
            -libc::EINVAL
        };

        debug!("sendmsg ret={ret}");
        if ret == -libc::EACCES {
            self.push_pkt(MuxerRx::Reset {
                local_port: pkt.dst_port(),
                peer_port: pkt.src_port(),
            });
        }

        ProxyUpdate::default()
    }

    fn sendto_addr(&mut self, req: TsiSendtoAddr) -> ProxyUpdate {
        debug!("sendto_addr: addr={}", req.addr);

        let mut update = ProxyUpdate::default();

        self.sendto_addr = Some(req.addr);
        if !self.listening {
            let Some(addr) = wildcard_address(self.family) else {
                return update;
            };
            match self
                .socket_policy
                .check(SocketOperation::Bind, &addr)
                .and_then(|()| bind(self.fd.as_raw_fd(), &addr))
            {
                Ok(_) => {
                    self.listening = true;
                    update.polling = Some((self.id, self.fd.as_raw_fd(), EventSet::IN));
                }
                Err(e) => {
                    self.sendto_addr = None;
                    self.push_pkt(MuxerRx::Reset {
                        local_port: self.local_port,
                        peer_port: self.peer_port,
                    });
                    debug!("couldn't bind socket: {e}");
                }
            }
        }

        update
    }

    fn sendto_data(&mut self, pkt: &VsockPacket) {
        debug!("sendto_data");

        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        if let Some(addr) = self.sendto_addr {
            if let Some(buf) = pkt.buf() {
                #[cfg(target_os = "linux")]
                let flags = MsgFlags::MSG_NOSIGNAL;

                match self
                    .socket_policy
                    .check(self.send_operation(), &addr)
                    .and_then(|()| sendto(self.fd.as_raw_fd(), buf, &addr, flags))
                {
                    Ok(sent) => {
                        self.tx_cnt += Wrapping(sent as u32);
                    }
                    Err(err) => {
                        debug!("error in sendto: {err}");
                        if err == nix::errno::Errno::EACCES {
                            self.push_pkt(MuxerRx::Reset {
                                local_port: pkt.dst_port(),
                                peer_port: pkt.src_port(),
                            });
                        }
                    }
                }
            } else {
                debug!("sendto_data pkt without buffer");
            }
        } else {
            debug!("sendto_data without sendto_addr");
        }
    }

    fn listen(
        &mut self,
        _pkt: &VsockPacket,
        _req: TsiListenReq,
        _host_port_map: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn accept(&mut self, _req: TsiAcceptReq) -> ProxyUpdate {
        ProxyUpdate::default()
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

        ProxyUpdate {
            polling: Some((self.id, self.fd.as_raw_fd(), EventSet::IN)),
            ..Default::default()
        }
    }

    fn process_op_response(&mut self, _pkt: &VsockPacket) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn release(&mut self) -> ProxyUpdate {
        debug!("release");
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

    fn process_event(&mut self, evset: EventSet) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        if evset.contains(EventSet::HANG_UP) {
            update.remove_proxy = if self.status == ProxyStatus::Listening {
                ProxyRemoval::Immediate
            } else {
                ProxyRemoval::Deferred
            };
            return update;
        }

        if evset.contains(EventSet::IN) {
            let (signal_queue, wait_credit) = self.recv_pkt();
            update.signal_queue = signal_queue || wait_credit;

            if wait_credit && self.status != ProxyStatus::WaitingCreditUpdate {
                self.status = ProxyStatus::WaitingCreditUpdate;
                let rx = MuxerRx::CreditRequest {
                    local_port: self.local_port,
                    peer_port: self.peer_port,
                    fwd_cnt: self.tx_cnt.0,
                };
                update.push_credit_req = Some(rx);
            }

            if self.status == ProxyStatus::WaitingCreditUpdate {
                debug!("process_event: WaitingCreditUpdate");
                update.polling = Some((self.id(), self.fd.as_raw_fd(), EventSet::empty()));
            }
        }

        if evset.contains(EventSet::OUT) {
            error!("EventSet::OUT unexpected");
        }

        update
    }
}

impl AsRawFd for TsiDgramProxy {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}
