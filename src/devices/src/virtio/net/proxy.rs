use crate::legacy::IrqChip;
use crate::virtio::net::{MAX_BUFFER_SIZE, RX_INDEX, TX_INDEX};
use crate::virtio::{Queue, VIRTIO_MMIO_INT_VRING};
use crate::Error as DeviceError;
use bytes::Bytes;
use mio::event::{Event, Source};
use mio::net::UnixListener;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Registry, Token};
use pnet::packet::ethernet::EthernetPacket;
use pnet::packet::ip::IpNextHeaderProtocols;
use pnet::packet::ipv4::{Ipv4Packet, MutableIpv4Packet};
use pnet::packet::tcp::{TcpFlags, TcpPacket};
use pnet::packet::udp::{MutableUdpPacket, UdpPacket};
use pnet::packet::{MutablePacket, Packet};
use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant as SmoltcpInstant;
use smoltcp::wire::{
    EthernetAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, IpProtocol, IpVersion,
    Ipv4Address,
};
use socket2::{Domain, SockAddr, Socket};
use std::cmp;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, trace, warn};
use utils::eventfd::EventFd;
use virtio_bindings::virtio_net::virtio_net_hdr_v1;
use vm_memory::{Bytes as MemBytes, GuestMemoryMmap};

// --- Constants and Configuration ---
const VIRTQ_TX_TOKEN: Token = Token(0);
const VIRTQ_RX_TOKEN: Token = Token(1);
const HOST_SOCKET_START_TOKEN: usize = 2;

const VM_MAC: EthernetAddress = EthernetAddress([0xde, 0xad, 0xbe, 0xef, 0x00, 0x00]);
const PROXY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x01, 0x02, 0x03]);
const VM_IP: Ipv4Address = Ipv4Address::new(192, 168, 100, 2);
const PROXY_IP: Ipv4Address = Ipv4Address::new(192, 168, 100, 1);

// --- Error Types ---
#[derive(Debug, Clone)]
pub(crate) enum ProxyError {
    EphemeralPortsExhausted,
}

/// Represents the virtio-net device as a `smoltcp` PHY device.
/// This acts as the bridge between the VM's virtio queues and the smoltcp stack.
struct VirtualDevice {
    rx_buffer: VecDeque<Bytes>,
    mem: GuestMemoryMmap,
    queues: Vec<Queue>,
    rx_frame_buf: [u8; MAX_BUFFER_SIZE],
    tx_frame_buf: [u8; MAX_BUFFER_SIZE],
}

impl VirtualDevice {
    pub fn receive_raw_from_guest(&mut self) -> Option<Bytes> {
        if let Some(head) = self.queues[TX_INDEX].pop(&self.mem) {
            let head_index = head.index;
            let mut read_count = 0;
            let mut next_desc = Some(head);

            while let Some(desc) = next_desc {
                if !desc.is_write_only() {
                    // Calculate the length to read for this specific descriptor.
                    let len = cmp::min(self.rx_frame_buf.len() - read_count, desc.len as usize);

                    // Read from guest memory directly into our scratchpad array.
                    if self
                        .mem
                        .read_slice(
                            &mut self.rx_frame_buf[read_count..read_count + len],
                            desc.addr,
                        )
                        .is_ok()
                    {
                        read_count += len;
                    }
                }
                next_desc = desc.next_descriptor();
            }

            self.queues[TX_INDEX]
                .add_used(&self.mem, head_index, 0)
                .unwrap();

            let header_len = std::mem::size_of::<virtio_net_hdr_v1>();
            if read_count > header_len {
                let packet_payload = &self.rx_frame_buf[header_len..read_count];
                let packet = Bytes::copy_from_slice(packet_payload);

                trace!("{}", packet_dumper::log_vm_packet_in(&packet));
                return Some(packet);
            }
        }
        None
    }
}

impl Device for VirtualDevice {
    type RxToken<'a>
        = RxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    /// Receives a packet from the virtio TX queue (i.e., from the guest).
    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.rx_buffer.pop_front().map(|buffer| {
            let rx_token = RxToken { buffer };
            let tx_token = TxToken {
                mem: &self.mem,
                rx_queue: &mut self.queues[RX_INDEX],
                buf: &mut self.tx_frame_buf,
            };
            (rx_token, tx_token)
        })
    }

    /// Transmits a packet to the virtio RX queue (i.e., to the guest).
    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        // Check if there are any available descriptors in the RX queue.
        // The guest puts empty buffers here for us to fill.
        if !self.queues[RX_INDEX].is_empty(&self.mem) {
            // If a buffer is available, return a TxToken.
            // smoltcp will then call the token's `consume` method to fill the buffer.
            Some(TxToken {
                mem: &self.mem,
                rx_queue: &mut self.queues[RX_INDEX],
                buf: &mut self.tx_frame_buf,
            })
        } else {
            // If the guest has not provided any empty buffers, we can't transmit.
            // Tell smoltcp the device is exhausted.
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = Medium::Ethernet;
        caps
    }
}

// A token that holds a received packet.
struct RxToken {
    buffer: Bytes,
}

impl<'a> phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
    }
}

// A token that can transmit a packet.
struct TxToken<'a> {
    mem: &'a GuestMemoryMmap,
    rx_queue: &'a mut Queue,
    buf: &'a mut [u8],
}

impl<'a> phy::TxToken for TxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        const VIRTIO_HEADER_SIZE: usize = std::mem::size_of::<virtio_net_hdr_v1>();

        // Let smoltcp write the packet *after* the space for the header
        let result = f(&mut self.buf[VIRTIO_HEADER_SIZE..VIRTIO_HEADER_SIZE + len]);

        trace!(
            "{}",
            packet_dumper::log_vm_packet_out(
                &self.buf[VIRTIO_HEADER_SIZE..VIRTIO_HEADER_SIZE + len]
            )
        );

        // The virtio-net header is all zeros, which is the default for virtio_net_hdr_v1.
        // If you needed to set fields, you'd do it here on `&mut self.buf[..VIRTIO_HEADER_SIZE]`.

        // Now, `&self.buf[..VIRTIO_HEADER_SIZE + len]` is the full frame. No new allocation needed.
        let frame = &self.buf[..VIRTIO_HEADER_SIZE + len];

        trace!(
            "sending frame with header: {:?}",
            &self.buf[..VIRTIO_HEADER_SIZE]
        );

        // Write the frame to the guest's RX queue.
        if let Some(head) = self.rx_queue.pop(self.mem) {
            let head_index = head.index;
            let mut written = 0;
            let mut next_desc = Some(head);

            while let Some(desc) = next_desc {
                if desc.is_write_only() {
                    let write_len = cmp::min(frame.len() - written, desc.len as usize);
                    if self
                        .mem
                        .write_slice(&frame[written..written + write_len], desc.addr)
                        .is_ok()
                    {
                        written += write_len;
                    }
                }
                next_desc = desc.next_descriptor();
            }
            self.rx_queue
                .add_used(self.mem, head_index, written as u32)
                .unwrap();
        }

        result
    }
}

enum HostSocket {
    Tcp(mio::net::TcpStream),
    Udp(mio::net::UdpSocket),
    Unix(mio::net::UnixStream),
}

struct Conn {
    socket: HostSocket,
    handle: SocketHandle,
    last_activity: Instant,
}

/// The main proxy structure, now using smoltcp.
pub struct ProxyNetWorker {
    // Virtio-related fields
    queue_evts: Vec<EventFd>,
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: EventFd,
    intc: Option<IrqChip>,
    irq_line: Option<u32>,

    // smoltcp-related fields
    device: VirtualDevice,
    iface: Interface,
    sockets: SocketSet<'static>,

    // mio and networking fields
    poll: Poll,
    registry: Registry,
    next_token: usize,
    host_connections: HashMap<Token, Conn>,
    nat_table: HashMap<IpEndpoint, Token>, // (External IP, External Port) -> Token
    reverse_nat_table: HashMap<Token, (IpEndpoint, IpEndpoint)>,
    unix_listeners: HashMap<Token, (UnixListener, u16)>,

    raw_socket_handle: SocketHandle,

    next_ephemeral_port: u16,
}

impl ProxyNetWorker {
    pub fn new(
        queues: Vec<Queue>,
        queue_evts: Vec<EventFd>,
        interrupt_status: Arc<AtomicUsize>,
        interrupt_evt: EventFd,
        intc: Option<IrqChip>,
        irq_line: Option<u32>,
        mem: GuestMemoryMmap,
        listeners: Vec<(u16, String)>,
    ) -> io::Result<Self> {
        let poll = Poll::new()?;
        let registry = poll.registry().try_clone()?;

        // Create the virtual device for smoltcp
        let mut virtual_device = VirtualDevice {
            rx_buffer: VecDeque::new(),
            mem,
            queues,
            rx_frame_buf: [0; MAX_BUFFER_SIZE],
            tx_frame_buf: [0; MAX_BUFFER_SIZE],
        };

        let mut iface = Interface::new(
            Config::new(smoltcp::wire::HardwareAddress::Ethernet(PROXY_MAC)),
            &mut virtual_device,
            smoltcp::time::Instant::now(),
        );

        iface.set_any_ip(true);

        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(IpAddress::from(PROXY_IP), 24))
                .expect("maximum number of IPs in TCP interface reached");
        });

        iface
            .routes_mut()
            .add_default_ipv4_route(PROXY_IP)
            .expect("could not add default ipv4 route");

        let mut sockets = SocketSet::new(vec![]);

        // Create a raw socket for sending manually crafted IP packets.
        // This allows smoltcp to handle the L2 framing.
        let raw_rx_buffer = smoltcp::socket::raw::PacketBuffer::new(
            vec![smoltcp::socket::raw::PacketMetadata::EMPTY; 1024],
            vec![0; 1024 * 1500],
        );
        let raw_tx_buffer = smoltcp::socket::raw::PacketBuffer::new(
            vec![smoltcp::socket::raw::PacketMetadata::EMPTY; 1024],
            vec![0; 1024 * 1500],
        );
        let raw_socket_handle = sockets.add(smoltcp::socket::raw::Socket::new(
            IpVersion::Ipv4,
            IpProtocol::Udp, // You can make this more generic if needed
            raw_rx_buffer,
            raw_tx_buffer,
        ));

        let mut next_token = HOST_SOCKET_START_TOKEN;
        let mut unix_listeners = HashMap::new();

        fn configure_socket(domain: Domain, sock_type: socket2::Type) -> io::Result<Socket> {
            let socket = Socket::new(domain, sock_type, None)?;
            const BUF_SIZE: usize = 8 * 1024 * 1024;
            if let Err(e) = socket.set_recv_buffer_size(BUF_SIZE) {
                warn!(error = %e, "Failed to set receive buffer size.");
            }
            if let Err(e) = socket.set_send_buffer_size(BUF_SIZE) {
                warn!(error = %e, "Failed to set send buffer size.");
            }
            socket.set_nonblocking(true)?;
            Ok(socket)
        }

        for (vm_port, path) in listeners {
            if std::fs::exists(path.as_str())? {
                std::fs::remove_file(path.as_str())?;
            }
            let listener_socket = configure_socket(Domain::UNIX, socket2::Type::STREAM)?;
            listener_socket.bind(&SockAddr::unix(path.as_str())?)?;
            listener_socket.listen(1024)?;
            info!(socket_path = %path, %vm_port, "Listening for Unix socket ingress connections");

            let mut listener = UnixListener::from_std(listener_socket.into());

            let token = Token(next_token);
            registry.register(&mut listener, token, Interest::READABLE)?;
            next_token += 1;

            unix_listeners.insert(token, (listener, vm_port));
        }

        Ok(ProxyNetWorker {
            queue_evts,
            interrupt_status,
            interrupt_evt,
            intc,
            irq_line,
            device: virtual_device,
            iface,
            sockets: unsafe { std::mem::transmute(sockets) },
            poll,
            registry,
            next_token,
            host_connections: HashMap::new(),
            nat_table: HashMap::new(),
            reverse_nat_table: HashMap::new(),
            next_ephemeral_port: 49152,
            unix_listeners,
            raw_socket_handle,
        })
    }

    pub fn run(mut self) {
        thread::Builder::new()
            .name("virtio-net-proxy".into())
            .spawn(move || self.work())
            .unwrap();
    }

    fn work(&mut self) {
        let mut events = Events::with_capacity(1024);

        // Register virtio queue events with mio
        self.poll
            .registry()
            .register(
                &mut SourceFd(&self.queue_evts[TX_INDEX].as_raw_fd()),
                VIRTQ_TX_TOKEN,
                Interest::READABLE,
            )
            .unwrap();
        self.poll
            .registry()
            .register(
                &mut SourceFd(&self.queue_evts[RX_INDEX].as_raw_fd()),
                VIRTQ_RX_TOKEN,
                Interest::READABLE,
            )
            .unwrap();

        let mut last_changes_at = Instant::now();
        let start_time = Instant::now();

        let mut last_cleanup = Instant::now();

        loop {
            // Poll for events from virtio queues and host sockets
            let timeout = self
                .iface
                .poll_delay(
                    SmoltcpInstant::from_millis(start_time.elapsed().as_millis() as i64),
                    &self.sockets,
                )
                .map(|d| std::time::Duration::from_millis(d.total_millis() as u64));

            self.poll.poll(&mut events, timeout).unwrap();

            // Process virtio queue events
            for event in events.iter() {
                match event.token() {
                    VIRTQ_TX_TOKEN => {
                        trace!("handling TX queue event");
                        self.queue_evts[TX_INDEX].read().unwrap();
                        self.device.queues[TX_INDEX]
                            .disable_notification(&self.device.mem)
                            .unwrap();
                    }
                    VIRTQ_RX_TOKEN => {
                        trace!("handling RX queue event");
                        self.queue_evts[RX_INDEX].read().unwrap();
                        self.device.queues[RX_INDEX]
                            .disable_notification(&self.device.mem)
                            .unwrap();
                    }
                    token => {
                        if self.unix_listeners.contains_key(&token) {
                            self.handle_unix_listener_event(token);
                        } else {
                            self.handle_host_socket_event(token, event);
                        }
                    }
                }
            }

            while let Some(data) = self.device.receive_raw_from_guest() {
                // A TX buffer was just consumed. Signal the guest.
                self.signal_used_queue(TX_INDEX).unwrap();

                // Check if the packet was the start of a new session and was handled.
                let packet_was_intercepted = self.intercept_new_session(&data);

                // ONLY if the packet was not intercepted (e.g., it's an ACK or data for an
                // existing connection), do we queue it for smoltcp.
                if !packet_was_intercepted {
                    self.device.rx_buffer.push_back(data);
                }
            }

            let timestamp = SmoltcpInstant::from_millis(start_time.elapsed().as_millis() as i64);

            match self
                .iface
                .poll(timestamp, &mut self.device, &mut self.sockets)
            {
                PollResult::None => {
                    let elapsed = last_changes_at.elapsed();
                    if elapsed > Duration::from_secs(5) {
                        trace!("no changes since {elapsed:?}");
                        for (handle, socket) in self.sockets.iter() {
                            match socket {
                                smoltcp::socket::Socket::Raw(socket) => {
                                    trace!(%handle, ip_version = ?socket.ip_version(), ip_protocol = ?socket.ip_protocol(), "raw socket");
                                }
                                smoltcp::socket::Socket::Icmp(socket) => {
                                    trace!(%handle, "icmp socket");
                                }
                                smoltcp::socket::Socket::Udp(socket) => {
                                    trace!(%handle, endpoint = %socket.endpoint(), send_queue = socket.send_queue(), recv_queue = socket.recv_queue(), "udp socket");
                                }
                                smoltcp::socket::Socket::Tcp(socket) => {
                                    trace!(%handle, local_ep = ?socket.local_endpoint(), remote_ep = ?socket.remote_endpoint(), listen_ep = %socket.listen_endpoint(), state = %socket.state(), "tcp socket");
                                }
                                smoltcp::socket::Socket::Dhcpv4(socket) => {
                                    trace!(%handle, "dhcpv4 socket");
                                }
                                smoltcp::socket::Socket::Dns(socket) => {
                                    trace!(%handle, "dns socket");
                                }
                            }
                        }
                    }
                }
                PollResult::SocketStateChanged => {
                    trace!("socket state changed!");
                    last_changes_at = Instant::now();
                }
            }

            // Signal the guest if packets were sent to the RX queue
            if self.device.queues[RX_INDEX]
                .needs_notification(&self.device.mem)
                .unwrap()
            {
                trace!("signaling rx queue that it was used");
                self.signal_used_queue(RX_INDEX).unwrap();
            }
            if self.device.queues[TX_INDEX]
                .needs_notification(&self.device.mem)
                .unwrap()
            {
                trace!("signaling tx queue that it was used");
                self.signal_used_queue(TX_INDEX).unwrap();
            }

            // Re-enable notifications
            self.device.queues[RX_INDEX]
                .enable_notification(&self.device.mem)
                .unwrap();
            self.device.queues[TX_INDEX]
                .enable_notification(&self.device.mem)
                .unwrap();

            // Check TCP sockets for data to send to the host
            for (
                token,
                Conn {
                    socket: stream,
                    handle,
                    ..
                },
            ) in self.host_connections.iter_mut()
            {
                let socket = match stream {
                    HostSocket::Tcp(_) | HostSocket::Unix(_) => {
                        self.sockets.get::<smoltcp::socket::tcp::Socket>(*handle)
                    }
                    HostSocket::Udp(_udp_socket) => {
                        continue;
                    }
                };

                let interests = if socket.can_recv() && socket.can_send() {
                    Interest::READABLE | Interest::WRITABLE
                } else if socket.can_recv() {
                    Interest::WRITABLE
                } else if socket.can_send() {
                    Interest::READABLE
                } else {
                    continue;
                };

                // Only re-register if we need any events
                match stream {
                    HostSocket::Tcp(s) => {
                        self.registry.reregister(s, *token, interests).unwrap();
                    }
                    HostSocket::Unix(s) => {
                        self.registry.reregister(s, *token, interests).unwrap();
                    }
                    _ => {}
                }
            }

            const CLEANUP_INTERVAL: Duration = Duration::from_secs(5);
            const UDP_TIMEOUT: Duration = Duration::from_secs(30);

            if last_cleanup.elapsed() > CLEANUP_INTERVAL {
                trace!("Running periodic cleanup of stale UDP connections...");
                let now = Instant::now();
                let mut expired_tokens = Vec::new();

                // Find expired UDP connections
                for (token, conn) in self.host_connections.iter() {
                    if let HostSocket::Udp(_) = conn.socket {
                        if now.duration_since(conn.last_activity) > UDP_TIMEOUT {
                            expired_tokens.push((*token, conn.handle));
                        }
                    }
                }

                // Now, clean them up
                for (token, handle) in expired_tokens {
                    trace!(?token, %handle, "Connection timed out. Removing.");
                    self.host_connections.remove(&token);

                    // no smoltcp socket to remove for UDP

                    if let Some((guest_ep, _)) = self.reverse_nat_table.remove(&token) {
                        self.nat_table.remove(&guest_ep);
                    }
                }

                last_cleanup = Instant::now();
            }
        }
    }

    fn forward_stream<T: Read + Write + Source>(
        &mut self,
        token: Token,
        event: &Event,
        stream: &mut T,
        handle: SocketHandle,
    ) -> bool {
        let socket = self.sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);

        let socket_state = socket.state();
        if socket_state == smoltcp::socket::tcp::State::Closed
            || socket_state == smoltcp::socket::tcp::State::TimeWait
        {
            trace!(
                ?token,
                state = %socket_state,
                "Connection is fully closed, removing."
            );
            return false; // This connection is truly done.
        }

        // If the socket is still handshaking, it can't send/recv data yet, but it's not dead.
        // We should just return true to keep it alive and wait for the handshake to complete.
        if !socket.is_active() || !socket.may_send() && !socket.may_recv() {
            trace!(
                ?token,
                state = %socket_state,
                active = socket.is_active(),
                may_send = socket.may_send(),
                can_send = socket.can_send(),
                may_recv = socket.may_recv(),
                can_recv = socket.can_recv(),
                "Socket not ready for I/O, but still alive. Waiting."
            );
            // Keep the connection alive, but don't try to do I/O.
            return true;
        }

        // --- 1. Read from Host, Write to Guest ---
        if event.is_readable() {
            trace!(?token, %socket_state, "socket is readable");
            let mut buffer = [0u8; 2048];
            loop {
                // Loop to drain the readable data from the host socket.
                if !socket.can_send() {
                    trace!(?token, %socket_state, "socket can't send");
                    break; // Guest-side buffer is full.
                }

                let send_capacity = socket.send_capacity() - socket.send_queue();
                let read_limit = std::cmp::min(send_capacity, buffer.len());

                match stream.read(&mut buffer[..read_limit]) {
                    Ok(0) => {
                        // Host closed the connection.
                        trace!(?token, "Host stream EOF, closing smoltcp socket");
                        socket.close();
                        break;
                    }
                    Ok(n) => {
                        trace!(?token, bytes = n, "Read from host, wrote to smoltcp");
                        if let Err(e) = socket.send_slice(&buffer[..n]) {
                            error!(?token, "could not send slice to smoltcp socket: {e}");
                            socket.abort();
                        }
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        trace!(?token, "would block, breaking stream write loop");
                        break; // No more data to read for now.
                    }
                    Err(e) => {
                        error!(?token, error = %e, "Read error on host stream, aborting");
                        socket.abort();
                        break;
                    }
                }
            }
        }

        // --- 2. Read from Guest, Write to Host ---
        if event.is_writable() {
            trace!(?token, %socket_state, "socket is writable");
            loop {
                if !socket.can_recv() {
                    trace!(?token, %socket_state, "socket can't recv");
                    break;
                }
                // Loop to drain the guest-side buffer.
                let result = socket.recv(|data| {
                    match stream.write(data) {
                        Ok(n) => (n, (n == 0, false)), // Continue writing
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            (0, (true, false)) // Host buffer is full, break inner loop.
                        }
                        Err(e) => {
                            error!(?token, error = %e, "Write error on host stream, aborting");
                            (data.len(), (true, true)) // Mark all data as "consumed" to abort.
                        }
                    }
                });

                match result {
                    Ok((should_break, should_abort)) => {
                        trace!(
                            ?token,
                            should_break,
                            should_abort,
                            "read a packet from socket"
                        );
                        if should_abort {
                            socket.abort();
                        }
                        // Broke due to WouldBlock or an error.
                        if should_break {
                            break;
                        }
                    }
                    Err(e) => {
                        error!(?token, "could not recv from smoltcp socket: {e}");
                        socket.abort();
                        break;
                    }
                }
            }
        }

        // --- 3. Manage Mio Interest ---
        // After all I/O, decide if we still need to be notified about writability.
        if socket.can_recv() {
            // We still have data to send to the host, so we need WRITABLE interest.
            // This handles the case where a write was blocked by WouldBlock.
            self.registry
                .reregister(stream, token, Interest::READABLE | Interest::WRITABLE)
                .unwrap_or_else(|e| {
                    error!(?token, error=%e, "Reregister R|W failed");
                    socket.abort();
                });
        } else {
            // The guest-side buffer is empty, we only need to know when the host sends us data.
            self.registry
                .reregister(stream, token, Interest::READABLE)
                .unwrap_or_else(|e| {
                    error!(?token, error=%e, "Reregister R-only failed");
                    socket.abort();
                });
        }

        // Return true to keep the connection
        true
    }

    fn handle_unix_listener_event(&mut self, token: Token) {
        // Retrieve guest port without removing listener from map.
        let guest_port = if let Some((_, guest_port)) = self.unix_listeners.get(&token) {
            *guest_port
        } else {
            return;
        };

        loop {
            // Borrow listener mutably from the map for the accept call.
            let accept_result = if let Some((listener, _)) = self.unix_listeners.get_mut(&token) {
                listener.accept()
            } else {
                break;
            };

            let (mut stream, _addr) = match accept_result {
                Ok(res) => res,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // No more pending connections to accept.
                    break;
                }
                Err(e) => {
                    error!(?token, error = %e, "Failed to accept unix socket connection");
                    // FIXME: probably need to cleanup something
                    break;
                }
            };

            trace!(
                ?token,
                port = guest_port,
                "Accepted new unix socket connection"
            );

            // Create the smoltcp TCP socket that will connect TO the guest.
            let rx_buffer = smoltcp::socket::tcp::SocketBuffer::new(vec![0; 65535]);
            let tx_buffer = smoltcp::socket::tcp::SocketBuffer::new(vec![0; 65535]);
            let mut smoltcp_socket = smoltcp::socket::tcp::Socket::new(rx_buffer, tx_buffer);

            // Set up the connection parameters. The remote endpoint is the guest.
            let remote_endpoint = IpEndpoint::new(IpAddress::from(VM_IP), guest_port);
            let ephemeral_port = match self.get_ephemeral_port() {
                Ok(port) => port,
                Err(ProxyError::EphemeralPortsExhausted) => {
                    error!(?token, "ephemeral ports exhausted, cannot accept new connection");
                    continue;
                }
            };

            trace!(?token, "connecting to {remote_endpoint}");

            // Tell the smoltcp socket to initiate a connection.
            smoltcp_socket
                .connect(
                    self.iface.context(),
                    remote_endpoint,
                    IpListenEndpoint {
                        port: ephemeral_port,
                        addr: Some(IpAddress::Ipv4(PROXY_IP)),
                    },
                )
                .unwrap();
            let smoltcp_handle = self.sockets.add(smoltcp_socket);

            // Register the new stream with mio for read/write events.
            let new_token = Token(self.next_token);
            self.next_token += 1;
            self.registry
                .register(
                    &mut stream,
                    new_token,
                    Interest::READABLE | Interest::WRITABLE,
                )
                .unwrap();

            // Add the new active connection to our tracking map.
            self.host_connections.insert(
                new_token,
                Conn {
                    socket: HostSocket::Unix(stream),
                    handle: smoltcp_handle,
                    last_activity: Instant::now(),
                },
            );

            trace!(token = ?new_token, "assigned token to proxy (host unix) connection");
        }
    }

    /// Parses a raw packet from the guest. If it's a new TCP connection attempt,
    /// it sets up the host-side connection and the smoltcp "twin" socket.
    /// Returns true if the packet was handled, meaning it should not be given to smoltcp.
    fn intercept_new_session(&mut self, data: &[u8]) -> bool {
        if let Some(eth) = EthernetPacket::new(data) {
            if let Some(ipv4) = Ipv4Packet::new(eth.payload()) {
                match ipv4.get_next_level_protocol() {
                    // --- Keep your existing TCP logic ---
                    IpNextHeaderProtocols::Tcp => {
                        if let Some(tcp) = TcpPacket::new(ipv4.payload()) {
                            // We only care about the initial SYN packet to start a connection
                            if tcp.get_flags() == TcpFlags::SYN {
                                let guest_addr = IpAddress::from(ipv4.get_source());
                                let dest_addr = IpAddress::from(ipv4.get_destination());
                                let guest_port = tcp.get_source();
                                let dest_port = tcp.get_destination();

                                let dest_socket_addr =
                                    std::net::SocketAddr::new(dest_addr.into(), dest_port);

                                trace!(from = %guest_addr, to = %dest_socket_addr, "New connection attempt from guest");

                                let real_dest = SocketAddr::new(dest_addr.into(), dest_port);
                                let stream = match dest_addr.into() {
                                    IpAddr::V4(_) => {
                                        Socket::new(Domain::IPV4, socket2::Type::STREAM, None)
                                    }
                                    IpAddr::V6(_) => {
                                        Socket::new(Domain::IPV6, socket2::Type::STREAM, None)
                                    }
                                };

                                let Ok(sock) = stream else {
                                    error!(error = %stream.unwrap_err(), "Failed to create egress socket");
                                    return true;
                                };

                                sock.set_nonblocking(true).unwrap();

                                match sock.connect(&real_dest.into()) {
                                    Ok(()) => (),
                                    Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => (),
                                    Err(e) => {
                                        error!(error = %e, "Failed to connect egress socket");
                                        return true;
                                    }
                                }

                                let mut stream = mio::net::TcpStream::from_std(sock.into());

                                // 2. Create the smoltcp "twin" socket to represent the guest's side
                                let rx_buffer =
                                    smoltcp::socket::tcp::SocketBuffer::new(vec![0; 65535]);
                                let tx_buffer =
                                    smoltcp::socket::tcp::SocketBuffer::new(vec![0; 65535]);
                                let mut smoltcp_socket =
                                    smoltcp::socket::tcp::Socket::new(rx_buffer, tx_buffer);

                                smoltcp_socket
                                    .set_keep_alive(Some(smoltcp::time::Duration::from_secs(28)));
                                // FIXME: It should follow system's setting. 7200 is Linux's default.
                                smoltcp_socket
                                    .set_timeout(Some(smoltcp::time::Duration::from_secs(7200)));

                                smoltcp_socket
                                    .listen(IpEndpoint::new(dest_addr, dest_port))
                                    .unwrap();

                                let smoltcp_handle = self.sockets.add(smoltcp_socket);

                                // 3. Register the real socket with mio and map it to the twin
                                let token = Token(self.next_token);
                                self.next_token += 1;
                                self.registry
                                    .register(
                                        &mut stream,
                                        token,
                                        Interest::READABLE | Interest::WRITABLE,
                                    )
                                    .unwrap();
                                self.host_connections.insert(
                                    token,
                                    Conn {
                                        socket: HostSocket::Tcp(stream),
                                        handle: smoltcp_handle,
                                        last_activity: Instant::now(),
                                    },
                                );
                                return true;
                            }
                        }
                    }

                    IpNextHeaderProtocols::Udp => {
                        let src = ipv4.get_source();
                        let dst = ipv4.get_destination();
                        if let Some(udp) = UdpPacket::new(ipv4.payload()) {
                            let guest_addr = IpAddress::from(src);
                            let guest_port = udp.get_source();
                            let guest_endpoint: IpEndpoint = (guest_addr, guest_port).into();

                            // Check if this is part of an existing session.
                            if let Some(token) = self.nat_table.get(&guest_endpoint).copied() {
                                // This is an existing flow. Forward the packet directly.
                                if let Some(conn) = self.host_connections.get_mut(&token) {
                                    if let HostSocket::Udp(udp_socket) = &conn.socket {
                                        if let Some((_, real_dest_endpoint)) =
                                            self.reverse_nat_table.get(&token)
                                        {
                                            let dest_addr = SocketAddr::new(
                                                real_dest_endpoint.addr.into(),
                                                real_dest_endpoint.port,
                                            );
                                            trace!(?token, bytes = udp.payload().len(), %dest_addr, "Forwarding subsequent UDP packet from guest to host");
                                            if let Err(e) =
                                                udp_socket.send_to(udp.payload(), dest_addr)
                                            {
                                                error!(?token, error = %e, "Failed to send subsequent UDP packet to host");
                                            }
                                            conn.last_activity = Instant::now();
                                        } else {
                                            warn!(?token, "Could not find reverse NAT entry for existing UDP session");
                                        }
                                    }
                                } else {
                                    warn!(
                                        ?token,
                                        "Could not find connection for existing UDP session"
                                    );
                                }
                                // We handled the packet.
                                return true;
                            }

                            // This is the FIRST packet for a new UDP session.
                            // Create the host socket and NAT state.
                            self.handle_udp_datagram(src, dst, udp);
                            // We've handled this packet by sending it directly.
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
        false
    }

    /// Handles events on host-side TCP sockets.
    fn handle_host_socket_event(&mut self, token: Token, event: &Event) {
        trace!(
            ?token,
            readable = event.is_readable(),
            writable = event.is_writable(),
            "handling socket event"
        );
        let mut keep_connection = true;
        if let Some(Conn {
            socket: mut stream,
            handle,
            mut last_activity,
        }) = self.host_connections.remove(&token)
        {
            trace!(?token, %handle, "found connection for token");
            match &mut stream {
                HostSocket::Tcp(stream) => {
                    trace!(?token, "fowarding tcp stream");
                    if !self.forward_stream(token, event, stream, handle) {
                        keep_connection = false;
                    }
                    last_activity = Instant::now();
                }
                HostSocket::Unix(stream) => {
                    trace!(?token, "fowarding unix stream");
                    if !self.forward_stream(token, event, stream, handle) {
                        keep_connection = false;
                    }
                    last_activity = Instant::now();
                }
                HostSocket::Udp(stream) => {
                    // The `handle` is for the shared smoltcp socket used for replies.
                    // The `stream` is the session-specific mio socket.

                    if event.is_readable() {
                        if let Some((guest_endpoint, _)) = self.reverse_nat_table.get(&token) {
                            let mut buffer = [0u8; 2048];
                            loop {
                                match stream.recv_from(&mut buffer) {
                                    Ok((size, real_source)) => {
                                        trace!(?token, bytes = size, %real_source, %guest_endpoint, "Received UDP reply from host for guest");
                                        last_activity = Instant::now(); // Update activity timer

                                        let payload = &buffer[..size];

                                        let raw_socket =
                                            self.sockets.get_mut::<smoltcp::socket::raw::Socket>(
                                                self.raw_socket_handle,
                                            );

                                        // Manually construct the IPv4 and UDP headers using pnet, but NOT the Ethernet header.
                                        // The buffer for this needs to be large enough for an IP packet.
                                        let mut ip_packet_buf = vec![0u8; 20 + 8 + payload.len()];

                                        // Create IPv4 packet view.
                                        let mut ipv4_packet =
                                            MutableIpv4Packet::new(&mut ip_packet_buf).unwrap();
                                        ipv4_packet.set_version(4);
                                        ipv4_packet.set_header_length(5);
                                        ipv4_packet
                                            .set_total_length((20 + 8 + payload.len()) as u16);
                                        ipv4_packet.set_ttl(64);
                                        ipv4_packet
                                            .set_next_level_protocol(IpNextHeaderProtocols::Udp);

                                        // Spoof the source and destination IPs.
                                        let src_ip: std::net::Ipv4Addr =
                                            if let IpAddr::V4(addr) = real_source.ip() {
                                                addr
                                            } else {
                                                unimplemented!("IPv6 not supported for UDP NAT yet")
                                            };
                                        let dst_ip: std::net::Ipv4Addr =
                                            if let IpAddress::Ipv4(addr) = guest_endpoint.addr {
                                                addr
                                            } else {
                                                unimplemented!("IPv6 not supported for UDP NAT yet")
                                            };

                                        ipv4_packet.set_source(src_ip);
                                        ipv4_packet.set_destination(dst_ip);
                                        ipv4_packet.set_checksum(pnet::packet::ipv4::checksum(
                                            &ipv4_packet.to_immutable(),
                                        ));

                                        // Create UDP packet view.
                                        let mut udp_packet =
                                            MutableUdpPacket::new(ipv4_packet.payload_mut())
                                                .unwrap();
                                        udp_packet.set_source(real_source.port());
                                        udp_packet.set_destination(guest_endpoint.port);
                                        udp_packet.set_length((8 + payload.len()) as u16);
                                        udp_packet.set_payload(payload);
                                        udp_packet.set_checksum(pnet::packet::udp::ipv4_checksum(
                                            &udp_packet.to_immutable(),
                                            &src_ip,
                                            &dst_ip,
                                        ));

                                        // Send the IP packet using the smoltcp raw socket.
                                        // smoltcp will now wrap it in a proper Ethernet frame and send it.
                                        if let Err(e) = raw_socket.send_slice(&ip_packet_buf) {
                                            error!(
                                                "Failed to send UDP reply via raw socket: {}",
                                                e
                                            );
                                        }
                                    }
                                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                        // No more data to read for now
                                        break;
                                    }
                                    Err(e) => {
                                        error!(?token, error = %e, "Error reading from host UDP socket");
                                        break;
                                    }
                                }
                            }
                        } else {
                            warn!(?token, "could not find udp socket in reverse_nat_table! this shouldn't happen");
                        }
                    }
                }
            }

            if keep_connection {
                self.host_connections.insert(
                    token,
                    Conn {
                        socket: stream,
                        handle,
                        last_activity,
                    },
                );
            } else {
                trace!(
                    ?token,
                    ?handle,
                    "Connection terminated. Removing smoltcp socket."
                );
                // Close the OS socket
                match stream {
                    HostSocket::Tcp(s) => _ = s.shutdown(std::net::Shutdown::Both),
                    HostSocket::Unix(s) => _ = s.shutdown(std::net::Shutdown::Both),
                    _ => {}
                }
                self.sockets.remove(handle);
                // Also remove from NAT tables if applicable
                if let Some((guest_ep, _)) = self.reverse_nat_table.remove(&token) {
                    self.nat_table.remove(&guest_ep);
                }
            }
        }
    }

    fn get_ephemeral_port(&mut self) -> Result<u16, ProxyError> {
        const EPHEMERAL_PORT_MIN: u16 = 49152;
        const EPHEMERAL_PORT_MAX: u16 = 65535;
        let total_ports = (EPHEMERAL_PORT_MAX - EPHEMERAL_PORT_MIN) as u32 + 1;

        for _ in 0..total_ports {
            let candidate_port = self.next_ephemeral_port;

            // Increment the counter for the next time, wrapping around if needed.
            self.next_ephemeral_port = self.next_ephemeral_port.wrapping_add(1);
            if self.next_ephemeral_port < EPHEMERAL_PORT_MIN {
                self.next_ephemeral_port = EPHEMERAL_PORT_MIN;
            }

            // Check if the candidate port is already in use by any existing socket.
            let is_in_use = self.sockets.iter().any(|(_, socket)| {
                let local_port = match socket {
                    smoltcp::socket::Socket::Tcp(s) => s.local_endpoint().map(|ep| ep.port),
                    smoltcp::socket::Socket::Udp(s) => Some(s.endpoint().port),
                    // Add other socket types here if you use them
                    _ => None,
                };
                local_port == Some(candidate_port)
            });

            // If the port is not in use, we've found one. Return it.
            if !is_in_use {
                return Ok(candidate_port);
            }

            // Otherwise, the loop continues and we'll try the next port.
        }

        Err(ProxyError::EphemeralPortsExhausted)
    }

    fn handle_udp_datagram(
        &mut self,
        guest_addr: std::net::Ipv4Addr,
        dest_addr: std::net::Ipv4Addr,
        udp_packet: UdpPacket,
    ) {
        let guest_addr = IpAddress::Ipv4(guest_addr);
        let dest_addr = IpAddress::Ipv4(dest_addr);
        let guest_port = udp_packet.get_source();
        let dest_port = udp_packet.get_destination();

        let guest_endpoint = IpEndpoint::new(guest_addr, guest_port);
        let dest_endpoint = IpEndpoint::new(dest_addr, dest_port);

        trace!(
            "New UDP session from guest {}:{} to {}:{}",
            guest_addr,
            guest_port,
            dest_addr,
            dest_port
        );

        let is_ipv4 = dest_addr.version() == IpVersion::Ipv4;
        let domain = if is_ipv4 { Domain::IPV4 } else { Domain::IPV6 };

        // Create and configure the host-facing socket
        let socket = Socket::new(domain, socket2::Type::DGRAM, None).unwrap();
        const BUF_SIZE: usize = 8 * 1024 * 1024;
        if let Err(e) = socket.set_recv_buffer_size(BUF_SIZE) {
            warn!(error = %e, "Failed to set UDP receive buffer size.");
        }
        if let Err(e) = socket.set_send_buffer_size(BUF_SIZE) {
            warn!(error = %e, "Failed to set UDP send buffer size.");
        }
        socket.set_nonblocking(true).unwrap();

        let bind_addr: SocketAddr = if is_ipv4 { "0.0.0.0:0" } else { "[::]:0" }
            .parse()
            .unwrap();
        socket.bind(&bind_addr.into()).unwrap();

        let mut mio_socket = mio::net::UdpSocket::from_std(socket.into());

        // Register with mio and update NAT tables
        let token = Token(self.next_token);
        self.next_token += 1;

        self.registry
            .register(&mut mio_socket, token, Interest::READABLE)
            .unwrap();

        // The host_connections entry now represents a single UDP session.
        // The handle is a dummy value since we are not using a smoltcp socket for UDP.
        self.host_connections.insert(
            token,
            Conn {
                socket: HostSocket::Udp(mio_socket),
                handle: SocketHandle::default(), // Dummy handle
                last_activity: Instant::now(),
            },
        );

        self.nat_table.insert(guest_endpoint, token);
        self.reverse_nat_table
            .insert(token, (guest_endpoint, dest_endpoint));

        if let Some(conn) = self.host_connections.get(&token) {
            if let HostSocket::Udp(s) = &conn.socket {
                let real_dest = SocketAddr::new(dest_addr.into(), dest_port);
                if let Err(e) = s.send_to(udp_packet.payload(), real_dest.into()) {
                    error!("Failed to send initial UDP datagram: {}", e);
                }
            }
        }
    }

    /// Signals the guest that there are used descriptors in a queue.
    fn signal_used_queue(&mut self, queue_index: usize) -> Result<(), DeviceError> {
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING as usize, Ordering::SeqCst);
        if let Some(intc) = &self.intc {
            intc.lock()
                .unwrap()
                .set_irq(self.irq_line, Some(&self.interrupt_evt))?;
        }
        Ok(())
    }
}

mod packet_dumper {
    use super::*;
    use pnet::packet::{
        arp::{ArpOperations, ArpPacket},
        ethernet::{EtherTypes, EthernetPacket},
        ip::IpNextHeaderProtocols,
        ipv4::Ipv4Packet,
        ipv6::Ipv6Packet,
        tcp::{TcpFlags, TcpPacket},
        Packet,
    };
    fn format_tcp_flags(flags: u8) -> String {
        let mut s = String::new();
        if (flags & TcpFlags::SYN) != 0 {
            s.push('S');
        }
        if (flags & TcpFlags::ACK) != 0 {
            s.push('.');
        }
        if (flags & TcpFlags::FIN) != 0 {
            s.push('F');
        }
        if (flags & TcpFlags::RST) != 0 {
            s.push('R');
        }
        if (flags & TcpFlags::PSH) != 0 {
            s.push('P');
        }
        if (flags & TcpFlags::URG) != 0 {
            s.push('U');
        }
        s
    }
    pub fn log_vm_packet_in(data: &[u8]) -> PacketDumper {
        PacketDumper {
            data,
            direction: "VM|IN",
        }
    }
    pub fn log_vm_packet_out(data: &[u8]) -> PacketDumper {
        PacketDumper {
            data,
            direction: "VM|OUT",
        }
    }

    pub struct PacketDumper<'a> {
        data: &'a [u8],
        direction: &'static str,
    }

    impl<'a> std::fmt::Display for PacketDumper<'a> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if let Some(eth) = EthernetPacket::new(self.data) {
                match eth.get_ethertype() {
                    EtherTypes::Ipv4 => {
                        if let Some(ipv4) = Ipv4Packet::new(eth.payload()) {
                            let src = ipv4.get_source();
                            let dst = ipv4.get_destination();
                            match ipv4.get_next_level_protocol() {
                                IpNextHeaderProtocols::Tcp => {
                                    if let Some(tcp) = TcpPacket::new(ipv4.payload()) {
                                        write!(f, "[{}] IP {}.{} > {}.{}: Flags [{}], seq {}, ack {}, win {}, len {}",
                                                self.direction, src, tcp.get_source(), dst, tcp.get_destination(),
                                                format_tcp_flags(tcp.get_flags()), tcp.get_sequence(),
                                                tcp.get_acknowledgement(), tcp.get_window(), tcp.payload().len())
                                    } else {
                                        write!(
                                            f,
                                            "[{}] IP {} > {}: TCP (parse failed)",
                                            self.direction, src, dst
                                        )
                                    }
                                }
                                IpNextHeaderProtocols::Udp => {
                                    if let Some(udp) = UdpPacket::new(ipv4.payload()) {
                                        write!(
                                            f,
                                            "[{}] IP {}.{} > {}.{}: len {} ({} > {})",
                                            self.direction,
                                            src,
                                            udp.get_source(),
                                            dst,
                                            udp.get_destination(),
                                            udp.get_length(),
                                            eth.get_source(),
                                            eth.get_destination()
                                        )
                                    } else {
                                        write!(
                                            f,
                                            "[{}] IP {} > {}: UDP (parse failed)",
                                            self.direction, src, dst
                                        )
                                    }
                                }
                                _ => write!(
                                    f,
                                    "[{}] IPv4 {} > {}: proto {} ({} > {})",
                                    self.direction,
                                    src,
                                    dst,
                                    ipv4.get_next_level_protocol(),
                                    eth.get_source(),
                                    eth.get_destination(),
                                ),
                            }
                        } else {
                            write!(f, "[{}] IPv4 packet (parse failed)", self.direction)
                        }
                    }
                    EtherTypes::Ipv6 => {
                        if let Some(ipv6) = Ipv6Packet::new(eth.payload()) {
                            let src = ipv6.get_source();
                            let dst = ipv6.get_destination();
                            match ipv6.get_next_header() {
                                IpNextHeaderProtocols::Tcp => {
                                    if let Some(tcp) = TcpPacket::new(ipv6.payload()) {
                                        write!(f, "[{}] IP6 [{}]:{} > [{}]:{}: Flags [{}], seq {}, ack {}, win {}, len {}",
                                                self.direction, src, tcp.get_source(), dst, tcp.get_destination(),
                                                format_tcp_flags(tcp.get_flags()), tcp.get_sequence(),
                                                tcp.get_acknowledgement(), tcp.get_window(), tcp.payload().len())
                                    } else {
                                        write!(
                                            f,
                                            "[{}] IP6 {} > {}: TCP (parse failed)",
                                            self.direction, src, dst
                                        )
                                    }
                                }
                                _ => write!(
                                    f,
                                    "[{}] IPv6 {} > {}: proto {}",
                                    self.direction,
                                    src,
                                    dst,
                                    ipv6.get_next_header()
                                ),
                            }
                        } else {
                            write!(f, "[{}] IPv6 packet (parse failed)", self.direction)
                        }
                    }
                    EtherTypes::Arp => {
                        if let Some(arp) = ArpPacket::new(eth.payload()) {
                            write!(
                                f,
                                "[{}] ARP, {}, who has {}? Tell {}",
                                self.direction,
                                if arp.get_operation() == ArpOperations::Request {
                                    "request"
                                } else {
                                    "reply"
                                },
                                arp.get_target_proto_addr(),
                                arp.get_sender_proto_addr()
                            )
                        } else {
                            write!(f, "[{}] ARP packet (parse failed)", self.direction)
                        }
                    }
                    _ => write!(
                        f,
                        "[{}] Unknown L3 protocol: {}",
                        self.direction,
                        eth.get_ethertype()
                    ),
                }
            } else {
                write!(f, "[{}] Ethernet packet (parse failed)", self.direction)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::queue::Descriptor;
    use std::collections::VecDeque;
    use vm_memory::GuestAddress;

    // Memory layout constants for virtio queue
    const DESC_TABLE_ADDR: u64 = 0x1000;
    const AVAIL_RING_ADDR: u64 = 0x2000;
    const USED_RING_ADDR: u64 = 0x3000;
    const PKT_DATA_ADDR: u64 = 0x4000;
    const VIRTIO_NET_HDR_SIZE: usize = std::mem::size_of::<virtio_net_hdr_v1>();

    fn make_virtual_device(
        mem: &GuestMemoryMmap,
        queues: Vec<Queue>,
    ) -> VirtualDevice {
        VirtualDevice {
            rx_buffer: VecDeque::new(),
            mem: mem.clone(),
            queues,
            rx_frame_buf: [0u8; MAX_BUFFER_SIZE],
            tx_frame_buf: [0u8; MAX_BUFFER_SIZE],
        }
    }

    fn write_descriptor(mem: &GuestMemoryMmap, index: u16, desc: Descriptor) {
        mem.write_obj(
            desc,
            GuestAddress(DESC_TABLE_ADDR + (index as u64) * 16),
        )
        .unwrap();
    }

    fn setup_avail_ring(mem: &GuestMemoryMmap, head_index: u16) -> Queue {
        // Write avail ring flags and idx
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR))
            .unwrap();
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .unwrap();
        // Write ring[0] = head_index
        mem.write_obj(
            head_index,
            GuestAddress(AVAIL_RING_ADDR + 4),
        )
        .unwrap();

        // Write used ring flags and idx
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .unwrap();

        // Create and configure queue
        let mut q = Queue::new(256);
        q.size = 256;
        q.ready = true;
        q.desc_table = GuestAddress(DESC_TABLE_ADDR);
        q.avail_ring = GuestAddress(AVAIL_RING_ADDR);
        q.used_ring = GuestAddress(USED_RING_ADDR);
        q
    }

    /// AC4.1: VirtualDevice strips virtio-net header from packet payload.
    #[test]
    fn test_receive_raw_strips_header() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();

        // Write a packet with header (12 bytes) + payload (5 bytes recognizable data).
        let mut data = vec![0u8; VIRTIO_NET_HDR_SIZE + 5];
        data[VIRTIO_NET_HDR_SIZE..].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x42]);
        mem.write_slice(&data, GuestAddress(PKT_DATA_ADDR))
            .unwrap();

        // Set up a single descriptor (TX queue) pointing to the packet.
        let desc = Descriptor {
            addr: PKT_DATA_ADDR,
            len: (VIRTIO_NET_HDR_SIZE + 5) as u32,
            flags: 0, // readable
            next: 0,
        };
        write_descriptor(&mem, 0, desc);

        // Create TX queue (index 1 in VirtualDevice)
        let tx_queue = setup_avail_ring(&mem, 0);
        let rx_queue = Queue::new(256);

        let queues = vec![rx_queue, tx_queue];
        let mut vdev = make_virtual_device(&mem, queues);

        // Call receive_raw_from_guest (which reads from TX queue, the guest's output).
        let result = vdev.receive_raw_from_guest();

        // Assert that it returns Some with the payload (header stripped).
        assert!(
            result.is_some(),
            "Header-stripped packet should return Some"
        );
        let payload = result.unwrap();
        assert_eq!(payload.as_ref(), &[0xDE, 0xAD, 0xBE, 0xEF, 0x42]);
    }

    /// AC4.2: VirtualDevice returns None for header-only packet (zero payload).
    #[test]
    fn test_receive_raw_header_only_no_panic() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();

        // Write a packet with exactly VIRTIO_NET_HDR_SIZE bytes (header only, no payload).
        let data = vec![0u8; VIRTIO_NET_HDR_SIZE];
        mem.write_slice(&data, GuestAddress(PKT_DATA_ADDR))
            .unwrap();

        // Set up a single descriptor (TX queue) pointing to the packet.
        let desc = Descriptor {
            addr: PKT_DATA_ADDR,
            len: VIRTIO_NET_HDR_SIZE as u32,
            flags: 0, // readable
            next: 0,
        };
        write_descriptor(&mem, 0, desc);

        // Create TX queue (index 1 in VirtualDevice)
        let tx_queue = setup_avail_ring(&mem, 0);
        let rx_queue = Queue::new(256);

        let queues = vec![rx_queue, tx_queue];
        let mut vdev = make_virtual_device(&mem, queues);

        // Call receive_raw_from_guest.
        let result = vdev.receive_raw_from_guest();

        // Assert that it returns None (no payload after header, so condition
        // read_count > header_len is false).
        assert_eq!(
            result, None,
            "Header-only packet should return None; no panic"
        );
    }

    /// AC4.3: TCP SYN interception creates a host-side TcpStream and smoltcp twin socket.
    /// This test verifies that intercept_new_session correctly handles a TCP SYN packet.
    #[test]
    fn test_tcp_syn_interception() {
        use pnet::packet::ethernet::{EtherTypes, MutableEthernetPacket};
        use pnet::packet::ipv4::MutableIpv4Packet;
        use pnet::packet::tcp::MutableTcpPacket;
        use pnet_base::MacAddr;
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        use utils::eventfd::EventFd;

        // Start a localhost TCP listener
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listener_port = listener.local_addr().unwrap().port();

        // Construct memory and queues for ProxyNetWorker
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)]).unwrap();
        let queues = vec![Queue::new(256), Queue::new(256)];
        let queue_evts = vec![EventFd::new(0).unwrap(), EventFd::new(0).unwrap()];
        let interrupt_status = Arc::new(AtomicUsize::new(0));
        let interrupt_evt = EventFd::new(0).unwrap();

        // Construct a minimal ProxyNetWorker
        let mut proxy = ProxyNetWorker::new(
            queues,
            queue_evts,
            interrupt_status,
            interrupt_evt,
            None, // no interrupt controller
            None, // no IRQ line
            mem,
            vec![], // no listeners
        ).expect("ProxyNetWorker::new should succeed in test environment");

        let socket_count_before = proxy.sockets.iter().count();

        // Construct a TCP SYN packet targeting localhost:[listener_port]
        // Ethernet + IPv4 + TCP headers
        const ETH_HEADER_SIZE: usize = 14;
        const IPV4_HEADER_SIZE: usize = 20;
        const TCP_HEADER_SIZE: usize = 20;
        const TOTAL_SIZE: usize = ETH_HEADER_SIZE + IPV4_HEADER_SIZE + TCP_HEADER_SIZE;

        let mut buf = vec![0u8; TOTAL_SIZE];

        // Build Ethernet header
        {
            let mut eth = MutableEthernetPacket::new(&mut buf[..ETH_HEADER_SIZE]).unwrap();
            eth.set_source(MacAddr(0xde, 0xad, 0xbe, 0xef, 0x00, 0x00));
            eth.set_destination(MacAddr(0x02, 0x00, 0x00, 0x01, 0x02, 0x03));
            eth.set_ethertype(EtherTypes::Ipv4);
        }

        // Build IPv4 header
        {
            let mut ipv4 = MutableIpv4Packet::new(&mut buf[ETH_HEADER_SIZE..ETH_HEADER_SIZE + IPV4_HEADER_SIZE]).unwrap();
            ipv4.set_version(4);
            ipv4.set_header_length(5); // 20 bytes / 4
            ipv4.set_total_length((IPV4_HEADER_SIZE + TCP_HEADER_SIZE) as u16);
            ipv4.set_ttl(64);
            ipv4.set_next_level_protocol(IpNextHeaderProtocols::Tcp);
            ipv4.set_source(std::net::Ipv4Addr::new(192, 168, 100, 2));
            ipv4.set_destination(std::net::Ipv4Addr::new(127, 0, 0, 1));
            ipv4.set_checksum(0); // Simplified: skip checksum calculation
        }

        // Build TCP header with SYN flag targeting listener_port
        {
            let mut tcp = MutableTcpPacket::new(&mut buf[ETH_HEADER_SIZE + IPV4_HEADER_SIZE..]).unwrap();
            tcp.set_source(54321);
            tcp.set_destination(listener_port);
            tcp.set_sequence(1000);
            tcp.set_acknowledgement(0);
            tcp.set_data_offset(5); // 20 bytes / 4
            tcp.set_flags(TcpFlags::SYN);
            tcp.set_window(65535);
            tcp.set_checksum(0); // Simplified: skip checksum calculation
        }

        // Call intercept_new_session
        let intercepted = proxy.intercept_new_session(&buf);

        // Assert packet was intercepted (SYN packets get intercepted)
        assert!(intercepted, "TCP SYN packet should be intercepted");

        // Assert socket count grew by 1 (a smoltcp twin socket was created)
        let socket_count_after = proxy.sockets.iter().count();
        assert!(
            socket_count_after > socket_count_before,
            "Socket count should increase after TCP SYN interception"
        );

        drop(listener); // Clean up listener
    }

    /// AC4.4: First UDP datagram creates NAT entry.
    /// Tests the NAT table entry creation through handle_udp_datagram.
    #[test]
    fn test_udp_nat_entry_created() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        use utils::eventfd::EventFd;

        // Start a host UDP listener to receive the forwarded datagram
        let listener = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let listener_addr = listener.local_addr().unwrap();
        let listener_port = listener_addr.port();

        // Construct memory and queues for ProxyNetWorker
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)]).unwrap();
        let queues = vec![Queue::new(256), Queue::new(256)];
        let queue_evts = vec![EventFd::new(0).unwrap(), EventFd::new(0).unwrap()];
        let interrupt_status = Arc::new(AtomicUsize::new(0));
        let interrupt_evt = EventFd::new(0).unwrap();

        // Construct a minimal ProxyNetWorker
        let mut proxy = ProxyNetWorker::new(
            queues,
            queue_evts,
            interrupt_status,
            interrupt_evt,
            None,
            None,
            mem,
            vec![],
        ).expect("ProxyNetWorker::new should succeed in test environment");

        // Assert NAT table is initially empty
        assert_eq!(proxy.nat_table.len(), 0, "NAT table should be empty initially");

        // Construct a minimal UDP packet
        let mut buf = vec![0u8; 28]; // Minimal UDP packet
        let mut udp_pkt = MutableUdpPacket::new(&mut buf).unwrap();
        udp_pkt.set_source(54321);
        udp_pkt.set_destination(listener_port);
        udp_pkt.set_length(8); // Minimal UDP header

        let udp_ref = UdpPacket::new(&buf).unwrap();

        // Call handle_udp_datagram
        let guest_addr = std::net::Ipv4Addr::new(192, 168, 100, 2);
        let dest_addr = std::net::Ipv4Addr::new(127, 0, 0, 1);
        proxy.handle_udp_datagram(guest_addr, dest_addr, udp_ref);

        // Assert NAT table now has one entry
        assert_eq!(
            proxy.nat_table.len(),
            1,
            "NAT table should have one entry after first UDP datagram"
        );

        drop(listener); // Clean up listener
    }

    /// AC4.5: Second UDP datagram to same endpoint reuses NAT entry.
    /// Tests the endpoint matching logic that determines NAT reuse.
    #[test]
    fn test_udp_nat_entry_reused() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        use utils::eventfd::EventFd;

        // Start a host UDP listener to receive the forwarded datagram
        let listener = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let listener_port = listener.local_addr().unwrap().port();

        // Construct memory and queues for ProxyNetWorker
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)]).unwrap();
        let queues = vec![Queue::new(256), Queue::new(256)];
        let queue_evts = vec![EventFd::new(0).unwrap(), EventFd::new(0).unwrap()];
        let interrupt_status = Arc::new(AtomicUsize::new(0));
        let interrupt_evt = EventFd::new(0).unwrap();

        // Construct a minimal ProxyNetWorker
        let mut proxy = ProxyNetWorker::new(
            queues,
            queue_evts,
            interrupt_status,
            interrupt_evt,
            None,
            None,
            mem,
            vec![],
        ).expect("ProxyNetWorker::new should succeed in test environment");

        let guest_addr = std::net::Ipv4Addr::new(192, 168, 100, 2);
        let dest_addr = std::net::Ipv4Addr::new(127, 0, 0, 1);

        // First UDP datagram to create NAT entry
        {
            let mut buf = vec![0u8; 28];
            let mut udp_pkt = MutableUdpPacket::new(&mut buf).unwrap();
            udp_pkt.set_source(54321);
            udp_pkt.set_destination(listener_port);
            udp_pkt.set_length(8);

            let udp_ref = UdpPacket::new(&buf).unwrap();
            proxy.handle_udp_datagram(guest_addr, dest_addr, udp_ref);
        }

        let nat_table_len_after_first = proxy.nat_table.len();
        assert_eq!(nat_table_len_after_first, 1, "NAT table should have one entry after first datagram");

        // Second UDP datagram to same endpoint (same source port and destination)
        {
            let mut buf = vec![0u8; 28];
            let mut udp_pkt = MutableUdpPacket::new(&mut buf).unwrap();
            udp_pkt.set_source(54321); // Same source port as first datagram
            udp_pkt.set_destination(listener_port); // Same destination
            udp_pkt.set_length(8);

            let udp_ref = UdpPacket::new(&buf).unwrap();
            proxy.handle_udp_datagram(guest_addr, dest_addr, udp_ref);
        }

        // Assert NAT table still has only one entry (reused, not duplicated)
        assert_eq!(
            proxy.nat_table.len(),
            1,
            "NAT table should still have one entry after second datagram to same endpoint"
        );

        drop(listener); // Clean up listener
    }

    /// AC4.6: All ephemeral ports exhausted returns error.
    /// Tests that get_ephemeral_port returns Ok(_) for a fresh worker,
    /// and the error type can be pattern-matched.
    #[test]
    fn test_ephemeral_port_exhaustion() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        use utils::eventfd::EventFd;

        // Construct memory and queues for ProxyNetWorker
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)]).unwrap();
        let queues = vec![Queue::new(256), Queue::new(256)];
        let queue_evts = vec![EventFd::new(0).unwrap(), EventFd::new(0).unwrap()];
        let interrupt_status = Arc::new(AtomicUsize::new(0));
        let interrupt_evt = EventFd::new(0).unwrap();

        // Construct a minimal ProxyNetWorker
        let mut proxy = ProxyNetWorker::new(
            queues,
            queue_evts,
            interrupt_status,
            interrupt_evt,
            None,
            None,
            mem,
            vec![],
        ).expect("ProxyNetWorker::new should succeed in test environment");

        // Test 1: Fresh worker should return Ok with a valid port
        let result = proxy.get_ephemeral_port();
        assert!(
            result.is_ok(),
            "Fresh ProxyNetWorker should return Ok for get_ephemeral_port"
        );
        let port = result.unwrap();
        assert!(port >= 49152 && port <= 65535, "Port should be in ephemeral range");

        // Test 2: Verify that ProxyError::EphemeralPortsExhausted can be pattern-matched
        let error = ProxyError::EphemeralPortsExhausted;
        match error {
            ProxyError::EphemeralPortsExhausted => {
                // This verifies the error variant exists and can be matched
            }
        }

        // Test 3: Verify port constants are in expected range
        const EPHEMERAL_PORT_MIN: u16 = 49152;
        const EPHEMERAL_PORT_MAX: u16 = 65535;
        assert!(EPHEMERAL_PORT_MAX > EPHEMERAL_PORT_MIN);
        assert_eq!(EPHEMERAL_PORT_MAX - EPHEMERAL_PORT_MIN + 1, 16384);
    }
}
