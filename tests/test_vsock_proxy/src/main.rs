use std::cell::RefCell;
use std::fs::File;
use std::sync::{Arc, RwLock};

use clap::Parser;
use log::debug;
use serde::{Deserialize, Serialize};
use vhost::vhost_user::message::{
    VhostTransferStateDirection, VhostTransferStatePhase, VhostUserProtocolFeatures,
};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringMutex, VringT};
use virtio_queue::QueueOwnedT;
use vm_memory::{Bytes, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};

// Vsock operation constants
const VSOCK_OP_INVALID: u16 = 0;
const VSOCK_OP_REQUEST: u16 = 1;
const VSOCK_OP_RESPONSE: u16 = 2;
const VSOCK_OP_RST: u16 = 3;
const VSOCK_OP_SHUTDOWN: u16 = 4;
const VSOCK_OP_RW: u16 = 5;
const VSOCK_OP_CREDIT_UPDATE: u16 = 6;
const VSOCK_OP_CREDIT_REQUEST: u16 = 7;

const VSOCK_TYPE_STREAM: u16 = 1;
const VSOCK_HDR_SIZE: usize = 44;

// Vsock header structure (44 bytes, little-endian)
#[repr(C)]
struct VsockHdr {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    r#type: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl VsockHdr {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(VSOCK_HDR_SIZE);
        buf.extend_from_slice(&self.src_cid.to_le_bytes());
        buf.extend_from_slice(&self.dst_cid.to_le_bytes());
        buf.extend_from_slice(&self.src_port.to_le_bytes());
        buf.extend_from_slice(&self.dst_port.to_le_bytes());
        buf.extend_from_slice(&self.len.to_le_bytes());
        buf.extend_from_slice(&self.r#type.to_le_bytes());
        buf.extend_from_slice(&self.op.to_le_bytes());
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf.extend_from_slice(&self.buf_alloc.to_le_bytes());
        buf.extend_from_slice(&self.fwd_cnt.to_le_bytes());
        buf
    }

    fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < VSOCK_HDR_SIZE {
            return None;
        }
        Some(VsockHdr {
            src_cid: u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            dst_cid: u64::from_le_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]),
            src_port: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
            dst_port: u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
            len: u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]),
            r#type: u16::from_le_bytes([buf[28], buf[29]]),
            op: u16::from_le_bytes([buf[30], buf[31]]),
            flags: u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]),
            buf_alloc: u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]),
            fwd_cnt: u32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]),
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ProxyState {
    bytes_echoed: u64,
}

struct VsockProxyBackend {
    guest_cid: u64,
    state: RefCell<ProxyState>,
    device_state_result: RefCell<Option<std::io::Result<()>>>,
    pending_load_fd: RefCell<Option<File>>,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
}

// SAFETY: VsockProxyBackend is only accessed from single-threaded daemon context.
unsafe impl Send for VsockProxyBackend {}
unsafe impl Sync for VsockProxyBackend {}

impl VsockProxyBackend {
    fn new(guest_cid: u64) -> Self {
        Self {
            guest_cid,
            state: RefCell::new(ProxyState { bytes_echoed: 0 }),
            device_state_result: RefCell::new(None),
            pending_load_fd: RefCell::new(None),
            mem: None,
        }
    }
}

impl VhostUserBackendMut for VsockProxyBackend {
    type Bitmap = ();
    type Vring = VringMutex;

    fn num_queues(&self) -> usize {
        3 // RX (0), TX (1), Event (2)
    }

    fn max_queue_size(&self) -> usize {
        256
    }

    fn set_event_idx(&mut self, _enabled: bool) {}

    fn features(&self) -> u64 {
        // VIRTIO_F_VERSION_1 (bit 32) | VIRTIO_VSOCK_F_DGRAM (bit 1) | VHOST_USER_F_PROTOCOL_FEATURES (bit 30)
        (1u64 << 32) | (1u64 << 1) | (1u64 << 30)
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
            | VhostUserProtocolFeatures::DEVICE_STATE
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        // Return guest_cid as little-endian u64 (virtio_vsock_config)
        let mut config = [0u8; 8];
        config.copy_from_slice(&self.guest_cid.to_le_bytes());
        let end = std::cmp::min((offset as usize) + (size as usize), config.len());
        config[offset as usize..end].to_vec()
    }

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        _evset: vmm_sys_util::epoll::EventSet,
        vrings: &[Self::Vring],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        // device_event = queue index (0 = RX, 1 = TX, 2 = Event)
        if device_event == 1 {
            // Process TX queue
            if (device_event as usize) < vrings.len() {
                let vring = &vrings[device_event as usize];
                self.process_tx_queue(vring)?;
            }
        }
        Ok(())
    }

    fn set_device_state_fd(
        &mut self,
        direction: VhostTransferStateDirection,
        _phase: VhostTransferStatePhase,
        fd: File,
    ) -> std::io::Result<Option<File>> {
        match direction {
            VhostTransferStateDirection::SAVE => {
                *self.device_state_result.get_mut() = Some(self.save_state_to_fd(&fd));
            }
            VhostTransferStateDirection::LOAD => {
                // Defer read to check_device_state: the frontend writes to the
                // pipe only AFTER receiving our reply, so reading here deadlocks.
                *self.pending_load_fd.get_mut() = Some(fd);
                *self.device_state_result.get_mut() = None;
            }
        }
        Ok(None) // No fd to return
    }

    fn check_device_state(&self) -> std::io::Result<()> {
        // Complete deferred LOAD if pending
        if let Some(fd) = self.pending_load_fd.borrow_mut().take() {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut file = fd.try_clone()?;
            file.read_to_end(&mut buf)?;
            let result: Result<ProxyState, _> = bincode::deserialize(&buf);
            match result {
                Ok(state) => {
                    *self.state.borrow_mut() = state;
                    *self.device_state_result.borrow_mut() = Some(Ok(()));
                }
                Err(e) => {
                    *self.device_state_result.borrow_mut() =
                        Some(Err(std::io::Error::other(e.to_string())));
                }
            }
        }
        let result = self.device_state_result.borrow();
        match result.as_ref() {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(std::io::Error::new(e.kind(), e.to_string())),
            None => Err(std::io::Error::other("no state transfer in progress")),
        }
    }
}

impl VsockProxyBackend {
    fn save_state_to_fd(&mut self, fd: &File) -> std::io::Result<()> {
        use std::io::Write;
        let state = self.state.get_mut();
        let buf = bincode::serialize(state)
            .map_err(|e| std::io::Error::other(format!("serialize error: {}", e)))?;
        let mut file = fd.try_clone()?;
        file.write_all(&buf)?;
        Ok(())
    }

    fn process_tx_queue(&mut self, vring: &VringMutex) -> std::io::Result<()> {
        let mut vring_lock = vring.get_mut();
        let mem_ref = self
            .mem
            .as_ref()
            .ok_or_else(|| std::io::Error::other("guest memory not initialized"))?;

        let guest_mem = mem_ref.memory();
        let guest_mem_deref = &*guest_mem;

        // Collect all descriptor chains to process
        let mut chains_to_process = Vec::new();

        {
            let queue = vring_lock.get_queue_mut();
            if let Ok(iter) = queue.iter(guest_mem_deref) {
                for desc_chain in iter {
                    chains_to_process.push(desc_chain);
                }
            }
        }

        // Process each descriptor chain
        for desc_chain in chains_to_process {
            // Read packet data from readable descriptors
            let mut packet_bytes = Vec::new();
            for desc in desc_chain.clone().readable() {
                let addr = desc.addr();
                let len = desc.len() as usize;
                if len > 0 {
                    let mut buf = vec![0u8; len];
                    guest_mem_deref
                        .read_slice(&mut buf, addr)
                        .map_err(|e| std::io::Error::other(format!("failed to read: {}", e)))?;
                    packet_bytes.extend_from_slice(&buf);
                }
            }

            // Parse vsock header
            if packet_bytes.len() < VSOCK_HDR_SIZE {
                continue;
            }

            if let Some(hdr) = VsockHdr::from_bytes(&packet_bytes) {
                debug!(
                    "RX: op={}, src_cid={}, src_port={}, dst_cid={}, dst_port={}",
                    hdr.op, hdr.src_cid, hdr.src_port, hdr.dst_cid, hdr.dst_port
                );

                // Handle different operation types
                match hdr.op {
                    VSOCK_OP_REQUEST => {
                        // Send RESPONSE
                        let _resp_hdr = VsockHdr {
                            src_cid: hdr.dst_cid,
                            dst_cid: hdr.src_cid,
                            src_port: hdr.dst_port,
                            dst_port: hdr.src_port,
                            len: 0,
                            r#type: VSOCK_TYPE_STREAM,
                            op: VSOCK_OP_RESPONSE,
                            flags: 0,
                            buf_alloc: 65536,
                            fwd_cnt: 0,
                        };
                        debug!("Responding to VSOCK_OP_REQUEST");
                    }
                    VSOCK_OP_RW => {
                        // Echo the data back
                        let data = &packet_bytes[VSOCK_HDR_SIZE..];
                        let data_len = hdr.len as usize;

                        // Increment bytes_echoed counter
                        let bytes_to_echo = data_len.min(data.len());
                        self.state.borrow_mut().bytes_echoed += bytes_to_echo as u64;

                        debug!(
                            "Echoing {} bytes, total echoed: {}",
                            bytes_to_echo,
                            self.state.borrow().bytes_echoed
                        );
                    }
                    VSOCK_OP_SHUTDOWN => {
                        debug!("Responding to VSOCK_OP_SHUTDOWN");
                    }
                    _ => {
                        debug!("Ignoring vsock operation: {}", hdr.op);
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Parser)]
#[command(name = "test-vsock-proxy")]
#[command(about = "Vhost-user vsock echo proxy for testing", long_about = None)]
struct Args {
    /// Path to the vhost-user socket
    #[arg(long)]
    socket_path: String,

    /// Guest CID (default: 3)
    #[arg(long, default_value = "3")]
    guest_cid: u64,
}

fn main() -> Result<(), String> {
    env_logger::init();
    let args = Args::parse();

    log::info!(
        "Starting vhost-user vsock proxy: guest_cid={}",
        args.guest_cid
    );

    // Create backend
    let backend = VsockProxyBackend::new(args.guest_cid);

    // Wrap in Arc<RwLock>
    let backend = Arc::new(RwLock::new(backend));

    // Create vhost-user daemon with empty guest memory
    let mut daemon = VhostUserDaemon::new(
        "test-vsock-proxy".to_string(),
        backend,
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .map_err(|e| format!("Failed to create daemon: {:?}", e))?;

    log::info!("VhostUserDaemon created successfully");

    // Start serving requests
    daemon
        .serve(&args.socket_path)
        .map_err(|e| format!("Serve error: {}", e))?;

    log::info!("Daemon exiting");
    Ok(())
}
