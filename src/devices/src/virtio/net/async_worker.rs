// Copyright 2024 Anthropic. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Async network worker for virtio-net devices.
//!
//! This worker provides a tokio-based event loop for processing virtio-net
//! queues, delegating actual network handling to a pluggable backend.
//!
//! # Design
//!
//! - Runs on a single-threaded tokio runtime with `LocalSet` for `!Send` futures
//! - TX path: Reads packets from virtio TX queue, passes borrowed slices to backend
//! - RX path: Receives packets from backend via channel, writes to virtio RX queue
//! - Backend handles all networking logic (TCP/IP stack, host sockets, NAT, etc.)

use std::cmp;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use log::{debug, error, trace};
use tokio::io::unix::AsyncFd;
use tokio::task::LocalSet;
use utils::eventfd::EventFd;
use virtio_bindings::virtio_net::virtio_net_hdr_v1;
use vm_memory::{Address, Bytes as VmBytes, GuestMemoryMmap};

use super::async_backend::{AsyncNetBackendFactory, NetBackendHandle};
use crate::virtio::queue::DescriptorChain;
use crate::virtio::{InterruptTransport, Queue};

const VIRTIO_NET_HDR_SIZE: usize = std::mem::size_of::<virtio_net_hdr_v1>();
const MAX_BUFFER_SIZE: usize = 65535;

/// The index of the RX queue (guest receives on this).
const RX_INDEX: usize = 0;
/// The index of the TX queue (guest sends on this).
const TX_INDEX: usize = 1;

/// Async network worker that processes virtio-net queues using tokio.
pub struct AsyncNetWorker {
    queues: Vec<Queue>,
    queue_evts: Vec<EventFd>,
    interrupt: InterruptTransport,
    mem: GuestMemoryMmap,
    factory: Box<dyn AsyncNetBackendFactory>,
    stop_fd: EventFd,
    resync_fd: EventFd,
    shared_queues: Arc<Mutex<Vec<Queue>>>,
    shared_generation: Arc<AtomicU64>,
    quiesce_fd: EventFd,
    resume_fd: EventFd,
    quiesce_ack: Arc<(Mutex<bool>, Condvar)>,
    shared_backend_state: Arc<Mutex<Option<Vec<u8>>>>,
}

impl AsyncNetWorker {
    /// Create a new async network worker.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        queues: Vec<Queue>,
        queue_evts: Vec<EventFd>,
        interrupt: InterruptTransport,
        mem: GuestMemoryMmap,
        factory: Box<dyn AsyncNetBackendFactory>,
        stop_fd: EventFd,
        resync_fd: EventFd,
        shared_queues: Arc<Mutex<Vec<Queue>>>,
        shared_generation: Arc<AtomicU64>,
        quiesce_fd: EventFd,
        resume_fd: EventFd,
        quiesce_ack: Arc<(Mutex<bool>, Condvar)>,
        shared_backend_state: Arc<Mutex<Option<Vec<u8>>>>,
    ) -> Self {
        Self {
            queues,
            queue_evts,
            interrupt,
            mem,
            factory,
            stop_fd,
            resync_fd,
            shared_queues,
            shared_generation,
            quiesce_fd,
            resume_fd,
            quiesce_ack,
            shared_backend_state,
        }
    }

    /// Start the async worker in a new thread.
    pub fn run(self) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("async-net-worker".into())
            .spawn(move || {
                debug!("async net worker: thread started, creating runtime");

                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to create tokio runtime");

                let local = LocalSet::new();
                rt.block_on(local.run_until(self.work_async()));

                debug!("async net worker: work_async completed");
            })
            .expect("failed to spawn async net worker")
    }

    /// Main async work loop.
    async fn work_async(self) {
        debug!("async net worker: starting");

        // Destructure self so we can consume factory separately
        let AsyncNetWorker {
            mut queues,
            queue_evts,
            interrupt,
            mem,
            factory,
            stop_fd,
            resync_fd,
            shared_queues,
            shared_generation,
            quiesce_fd,
            resume_fd,
            quiesce_ack,
            shared_backend_state,
        } = self;

        // Wrap TX eventfd for async early — we need to drain TX during backend creation
        // to prevent NETDEV WATCHDOG timeouts if the factory takes a while.
        let async_tx_evt = match AsyncFd::new(dup_fd(&queue_evts[TX_INDEX])) {
            Ok(fd) => fd,
            Err(e) => {
                error!("failed to create AsyncFd for TX queue: {e}");
                return;
            }
        };

        let async_stop = match AsyncFd::new(dup_fd(&stop_fd)) {
            Ok(fd) => fd,
            Err(e) => {
                error!("failed to create AsyncFd for stop: {e}");
                return;
            }
        };

        let async_resync = match AsyncFd::new(dup_fd(&resync_fd)) {
            Ok(fd) => fd,
            Err(e) => {
                error!("failed to create AsyncFd for resync: {e}");
                return;
            }
        };

        let async_quiesce = match AsyncFd::new(dup_fd(&quiesce_fd)) {
            Ok(fd) => fd,
            Err(e) => {
                error!("failed to create AsyncFd for quiesce: {e}");
                return;
            }
        };

        let async_resume = match AsyncFd::new(dup_fd(&resume_fd)) {
            Ok(fd) => fd,
            Err(e) => {
                error!("failed to create AsyncFd for resume: {e}");
                return;
            }
        };

        // Create the backend. While the factory initializes (host socket setup, etc.),
        // drain TX descriptors to prevent NETDEV WATCHDOG timeouts from the guest kernel.
        let mut backend_fut = factory.create();
        let NetBackendHandle {
            mut backend,
            mut to_guest_rx,
            mut wake_rx,
        } = loop {
            tokio::select! {
                biased;
                result = &mut backend_fut => {
                    match result {
                        Ok(handle) => break handle,
                        Err(e) => {
                            error!("failed to create net backend: {e}");
                            return;
                        }
                    }
                }
                ready = async_tx_evt.readable() => {
                    if let Ok(mut guard) = ready {
                        guard.clear_ready();
                        if queue_evts[TX_INDEX].read().is_ok() {
                            discard_tx_queue(&mut queues, &mem, &interrupt);
                        }
                    }
                }
                ready = async_stop.readable() => {
                    if ready.is_ok() {
                        debug!("async net worker: stopping during backend creation");
                        let _ = stop_fd.read();
                        return;
                    }
                }
            }
        };

        debug!("async net worker: backend created");

        // Scratch buffer for reading TX packets - reused to avoid allocations
        let mut tx_buf = vec![0u8; MAX_BUFFER_SIZE];

        debug!("async net worker: entering main loop");
        let mut applied_generation: u64 = 0;

        loop {
            // Cap poll delay to 1 second to ensure timely interrupt re-signaling.
            // Without this cap, idle backends (poll_delay = None → 3600s) would leave
            // the guest waiting too long if an interrupt was lost.
            let delay = backend
                .poll_delay()
                .unwrap_or(Duration::from_secs(1))
                .min(Duration::from_secs(1));

            tokio::select! {
                biased;

                // Snapshot quiesce: publish queue state, ACK, then park until resume
                ready = async_quiesce.readable() => {
                    if let Ok(mut guard) = ready {
                        guard.clear_ready();
                        if quiesce_fd.read().is_ok() {
                            debug!("async net worker: quiesce requested, publishing queue + backend state");
                            // Publish local queues to shared state
                            if let Ok(mut shared) = shared_queues.lock() {
                                for (dst, src) in shared.iter_mut().zip(queues.iter()) {
                                    *dst = src.clone();
                                }
                            }
                            // Publish backend snapshot state
                            if let Ok(mut shared) = shared_backend_state.lock() {
                                *shared = backend.save_snapshot_state();
                            }
                            // Signal the device that we've parked
                            {
                                let (lock, cvar) = &*quiesce_ack;
                                *lock.lock().unwrap() = true;
                                cvar.notify_one();
                            }
                            // Park: wait for resume_fd
                            debug!("async net worker: parked, waiting for resume");
                            loop {
                                match async_resume.readable().await {
                                    Ok(mut rg) => {
                                        rg.clear_ready();
                                        if resume_fd.read().is_ok() {
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        error!("async net worker: resume fd error: {e}");
                                        break;
                                    }
                                }
                            }
                            debug!("async net worker: resumed from quiesce");
                        }
                    }
                }

                // Guest sent packets (TX queue)
                ready = async_tx_evt.readable() => {
                    match ready {
                        Ok(mut guard) => {
                            guard.clear_ready();
                            if queue_evts[TX_INDEX].read().is_ok() {
                                apply_shared_queue_state_if_needed(
                                    &mut queues,
                                    &mem,
                                    &shared_queues,
                                    &shared_generation,
                                    &mut applied_generation,
                                );
                                trace!("async net worker: TX queue event");
                                drain_tx_queue(
                                    &mut queues,
                                    &mem,
                                    &interrupt,
                                    &mut *backend,
                                    &mut tx_buf,
                                );
                                backend.poll();
                            }
                        }
                        Err(e) => {
                            error!("TX queue fd error: {e}");
                        }
                    }
                }

                // Backend has packet for guest
                Some(packet) = to_guest_rx.recv() => {
                    apply_shared_queue_state_if_needed(
                        &mut queues,
                        &mem,
                        &shared_queues,
                        &shared_generation,
                        &mut applied_generation,
                    );
                    trace!("async net worker: RX packet from backend, len={}", packet.len());
                    push_to_rx_queue(
                        &mut queues,
                        &mem,
                        &interrupt,
                        &packet,
                    );
                    // Drain TX while processing RX to prevent TX starvation during RX floods
                    if queues[TX_INDEX].ready && !queues[TX_INDEX].is_empty(&mem) {
                        drain_tx_queue(
                            &mut queues,
                            &mem,
                            &interrupt,
                            &mut *backend,
                            &mut tx_buf,
                        );
                        backend.poll();
                    }
                }

                // Timer for backend polling + interrupt heartbeat
                _ = tokio::time::sleep(delay) => {
                    trace!("async net worker: poll timer");
                    backend.poll();
                    // Heartbeat: re-signal the used queue in case a previous interrupt
                    // was lost to an ISR race condition. This is cheap (one GIC interrupt
                    // per poll cycle) and ensures the guest eventually processes TX
                    // completions even if an interrupt was eaten.
                    if queues[TX_INDEX].ready {
                        let _ = interrupt.try_signal_used_queue();
                    }
                    if queues[TX_INDEX].ready && !queues[TX_INDEX].is_empty(&mem) {
                        trace!("async net worker: timer detected pending TX without event");
                        drain_tx_queue(
                            &mut queues,
                            &mem,
                            &interrupt,
                            &mut *backend,
                            &mut tx_buf,
                        );
                        backend.poll();
                    }
                }

                // Wake notification from backend's background tasks
                Some(_) = async { wake_rx.as_mut()?.recv().await }, if wake_rx.is_some() => {
                    trace!("async net worker: wake notification");
                    backend.poll();
                    if queues[TX_INDEX].ready && !queues[TX_INDEX].is_empty(&mem) {
                        trace!("async net worker: wake detected pending TX without event");
                        drain_tx_queue(
                            &mut queues,
                            &mem,
                            &interrupt,
                            &mut *backend,
                            &mut tx_buf,
                        );
                        backend.poll();
                    }
                }

                // Shutdown
                ready = async_stop.readable() => {
                    if ready.is_ok() {
                        debug!("async net worker: stopping");
                        let _ = stop_fd.read();
                        backend.on_exit();
                        return;
                    }
                }

                // Queue resync after snapshot restore
                ready = async_resync.readable() => {
                    if let Ok(mut guard) = ready {
                        guard.clear_ready();
                        if resync_fd.read().is_ok() {
                            apply_shared_queue_state_if_needed(
                                &mut queues,
                                &mem,
                                &shared_queues,
                                &shared_generation,
                                &mut applied_generation,
                            );
                            // Restore backend state if the device provided one
                            if let Ok(mut shared) = shared_backend_state.lock() {
                                if let Some(data) = shared.take() {
                                    debug!("async net worker: restoring backend state ({} bytes)", data.len());
                                    backend.restore_snapshot_state(&data);
                                }
                            }
                            if queues[TX_INDEX].ready && !queues[TX_INDEX].is_empty(&mem) {
                                trace!("async net worker: resync detected pending TX without event");
                                drain_tx_queue(
                                    &mut queues,
                                    &mem,
                                    &interrupt,
                                    &mut *backend,
                                    &mut tx_buf,
                                );
                                backend.poll();
                            }
                            trace!("async net worker: queues resynced from snapshot state");
                        }
                    }
                }
            }
        }
    }
}

fn apply_shared_queue_state_if_needed(
    queues: &mut [Queue],
    mem: &GuestMemoryMmap,
    shared_queues: &Arc<Mutex<Vec<Queue>>>,
    shared_generation: &Arc<AtomicU64>,
    applied_generation: &mut u64,
) {
    let generation = shared_generation.load(Ordering::Acquire);
    if generation == *applied_generation {
        return;
    }

    if let Ok(shared) = shared_queues.lock() {
        for (dst, src) in queues.iter_mut().zip(shared.iter()) {
            *dst = src.clone();

            if dst.ready {
                if let Some(used_idx_addr) = dst.used_ring.checked_add(2) {
                    if let Ok(used_idx) = mem.read_obj::<u16>(used_idx_addr) {
                        dst.set_next_used(used_idx);
                    }
                }

                if let Some(avail_idx_addr) = dst.avail_ring.checked_add(2) {
                    if let Ok(avail_idx) = mem.read_obj::<u16>(avail_idx_addr) {
                        if dst.next_avail().0 > avail_idx {
                            dst.set_next_avail(avail_idx);
                        }
                    }
                }
            }
        }
        *applied_generation = generation;
    }
}

/// Consume TX descriptors without forwarding to the backend.
/// Used during backend creation to prevent NETDEV WATCHDOG timeouts.
fn discard_tx_queue(queues: &mut [Queue], mem: &GuestMemoryMmap, interrupt: &InterruptTransport) {
    while let Some(head) = queues[TX_INDEX].pop(mem) {
        queues[TX_INDEX].add_used(mem, head.index, 0).ok();
    }
    if let Err(e) = interrupt.try_signal_used_queue() {
        error!("failed to signal TX used queue: {e:?}");
    }
}

/// Drain all packets from the TX queue and pass to backend.
fn drain_tx_queue(
    queues: &mut [Queue],
    mem: &GuestMemoryMmap,
    interrupt: &InterruptTransport,
    backend: &mut dyn super::async_backend::AsyncNetBackend,
    buf: &mut [u8],
) {
    queues[TX_INDEX].disable_notification(mem).ok();

    loop {
        let next_avail_before = queues[TX_INDEX].next_avail();

        while let Some(head) = queues[TX_INDEX].pop(mem) {
            let head_index = head.index;

            // Read packet into scratch buffer
            if let Some(len) = read_tx_packet(mem, &head, buf) {
                trace!(
                    "async net worker: TX packet, index={}, len={}",
                    head_index,
                    len
                );
                // Pass borrowed slice to backend - zero copy at interface
                backend.handle_guest_tx(&buf[..len]);
            }

            // Mark descriptor as used
            queues[TX_INDEX].add_used(mem, head_index, 0).ok();
        }

        if queues[TX_INDEX].next_avail() == next_avail_before {
            // Queue made no progress; distinguish two cases:
            // 1) Guest advanced avail_idx concurrently -> retry without mutating next_avail.
            // 2) next_avail drifted ahead of avail_idx (e.g. after restore) -> rewind.
            if let Some(avail_idx_addr) = queues[TX_INDEX].avail_ring.checked_add(2) {
                if let Ok(avail_idx) = mem.read_obj::<u16>(avail_idx_addr) {
                    let next_avail = queues[TX_INDEX].next_avail().0;
                    if avail_idx != next_avail {
                        let queue_size = queues[TX_INDEX].actual_size();
                        if queue_size != 0 {
                            let pending = avail_idx.wrapping_sub(next_avail);
                            if pending <= queue_size {
                                continue; // New/pending descriptors are available; retry pop()
                            }
                        }

                        error!(
                            "async net worker: TX queue next_avail appears ahead; rewinding {} -> {}",
                            next_avail,
                            avail_idx
                        );
                        queues[TX_INDEX].set_next_avail(avail_idx);
                        continue;
                    }
                }
            }
            break;
        }

        if !queues[TX_INDEX].enable_notification(mem).unwrap_or(false) {
            break;
        }
    }
    // Signal guest that we consumed descriptors.
    // Snapshot restore can desynchronize notification heuristics; signal unconditionally.
    if let Err(e) = interrupt.try_signal_used_queue() {
        error!("failed to signal TX used queue: {e:?}");
    }
}

/// Read a TX packet into buffer, stripping the virtio-net header.
/// Returns the payload length (without header), or None if invalid.
fn read_tx_packet(mem: &GuestMemoryMmap, head: &DescriptorChain, buf: &mut [u8]) -> Option<usize> {
    let mut offset = 0;
    let mut desc = Some(head.clone());

    while let Some(d) = desc {
        if !d.is_write_only() {
            let len = cmp::min(d.len as usize, buf.len() - offset);
            if mem
                .read_slice(&mut buf[offset..offset + len], d.addr)
                .is_ok()
            {
                offset += len;
            }
        }
        desc = d.next_descriptor();
    }

    // Strip virtio-net header
    if offset > VIRTIO_NET_HDR_SIZE {
        // Shift payload to start of buffer to avoid tracking offset
        buf.copy_within(VIRTIO_NET_HDR_SIZE..offset, 0);
        Some(offset - VIRTIO_NET_HDR_SIZE)
    } else {
        None
    }
}

/// Push a packet to the guest via the RX queue.
fn push_to_rx_queue(
    queues: &mut [Queue],
    mem: &GuestMemoryMmap,
    interrupt: &InterruptTransport,
    packet: &[u8],
) {
    let Some(head) = queues[RX_INDEX].pop(mem) else {
        // This is expected under high load - guest can't replenish RX buffers fast enough
        trace!("async net worker: no RX buffers available, dropping packet");
        return;
    };

    let head_index = head.index;
    let mut written = 0;
    let mut desc = Some(head);

    // Virtio-net header (all zeros is valid)
    let header = [0u8; VIRTIO_NET_HDR_SIZE];

    while let Some(d) = desc {
        if d.is_write_only() {
            let available = d.len as usize;

            // First write header, then packet data
            if written < VIRTIO_NET_HDR_SIZE {
                let hdr_remaining = VIRTIO_NET_HDR_SIZE - written;
                let hdr_write = cmp::min(hdr_remaining, available);

                mem.write_slice(&header[written..written + hdr_write], d.addr)
                    .ok();

                if hdr_write < available {
                    // Room for packet data in this descriptor
                    let pkt_write = cmp::min(packet.len(), available - hdr_write);
                    mem.write_slice(&packet[..pkt_write], d.addr.unchecked_add(hdr_write as u64))
                        .ok();
                    written = VIRTIO_NET_HDR_SIZE + pkt_write;
                } else {
                    written += hdr_write;
                }
            } else {
                let pkt_offset = written - VIRTIO_NET_HDR_SIZE;
                let pkt_remaining = packet.len().saturating_sub(pkt_offset);
                let pkt_write = cmp::min(pkt_remaining, available);

                if pkt_write > 0 {
                    mem.write_slice(&packet[pkt_offset..pkt_offset + pkt_write], d.addr)
                        .ok();
                }
                written += pkt_write;
            }
        }
        desc = d.next_descriptor();
    }

    // Mark descriptor as used with the number of bytes written
    queues[RX_INDEX]
        .add_used(mem, head_index, written as u32)
        .ok();

    // Signal guest.
    // Snapshot restore can desynchronize notification heuristics; signal unconditionally.
    if let Err(e) = interrupt.try_signal_used_queue() {
        error!("failed to signal RX used queue: {e:?}");
    }
}

/// Duplicate a file descriptor for use with AsyncFd.
fn dup_fd(evt: &EventFd) -> OwnedFd {
    // SAFETY: We're duplicating a valid fd that we own
    unsafe { OwnedFd::from_raw_fd(libc::dup(evt.as_raw_fd())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Poll until `condition` returns true or 5 seconds elapse.
    fn poll_until<F: Fn() -> bool>(msg: &str, condition: F) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if condition() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for: {msg}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn test_virtio_net_hdr_size() {
        // Verify our constant matches the actual struct size
        assert_eq!(
            VIRTIO_NET_HDR_SIZE,
            std::mem::size_of::<virtio_net_hdr_v1>()
        );
    }

    #[test]
    fn test_dup_fd() {
        let evt = EventFd::new(0).unwrap();
        let duped = dup_fd(&evt);
        // The duped fd should be different from the original
        assert_ne!(duped.as_raw_fd(), evt.as_raw_fd());
    }

    use super::super::async_backend::{
        AsyncNetBackend, AsyncNetBackendFactory, NetBackendHandle, SendBoxFuture,
    };
    use crate::legacy::DummyIrqChip;
    use crate::virtio::InterruptTransport;
    use std::sync::Condvar;
    use vm_memory::GuestAddress;

    /// Dummy net backend that does nothing (for quiesce testing).
    struct DummyNetBackend;

    impl AsyncNetBackend for DummyNetBackend {
        fn handle_guest_tx(&mut self, _packet: &[u8]) {}
        fn poll(&mut self) {}
        fn on_exit(&mut self) {}
    }

    /// Factory that creates a DummyNetBackend.
    struct DummyNetBackendFactory;

    impl AsyncNetBackendFactory for DummyNetBackendFactory {
        fn create(self: Box<Self>) -> SendBoxFuture<'static, std::io::Result<NetBackendHandle>> {
            Box::pin(async move {
                let (to_guest_tx, to_guest_rx) = tokio::sync::mpsc::channel(16);
                // Keep tx alive so the channel doesn't close
                std::mem::forget(to_guest_tx);
                Ok(NetBackendHandle {
                    backend: Box::new(DummyNetBackend),
                    to_guest_rx,
                    wake_rx: None,
                })
            })
        }
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use bytes::Bytes;

    /// Mock AsyncNetBackend that records TX calls and supports injecting RX packets.
    struct TrackingNetBackend {
        /// All payloads delivered via handle_guest_tx, in order.
        tx_received: Arc<Mutex<Vec<Vec<u8>>>>,
        /// Set this before running a test to control poll_delay().
        poll_delay_value: Option<Duration>,
        /// Count of poll() invocations.
        poll_count: Arc<AtomicUsize>,
        /// Data returned by save_snapshot_state (None means no state).
        snapshot_data: Option<Vec<u8>>,
        /// Data received by restore_snapshot_state.
        restored_state: Arc<Mutex<Option<Vec<u8>>>>,
    }

    impl TrackingNetBackend {
        fn new(snapshot_data: Option<Vec<u8>>) -> (
            Self,
            Arc<Mutex<Vec<Vec<u8>>>>,
            Arc<AtomicUsize>,
            Arc<Mutex<Option<Vec<u8>>>>,
        ) {
            let tx_received = Arc::new(Mutex::new(Vec::new()));
            let poll_count = Arc::new(AtomicUsize::new(0));
            let restored_state = Arc::new(Mutex::new(None));
            let backend = TrackingNetBackend {
                tx_received: tx_received.clone(),
                poll_delay_value: None,
                poll_count: poll_count.clone(),
                snapshot_data,
                restored_state: restored_state.clone(),
            };
            (backend, tx_received, poll_count, restored_state)
        }
    }

    impl AsyncNetBackend for TrackingNetBackend {
        fn handle_guest_tx(&mut self, packet: &[u8]) {
            self.tx_received.lock().unwrap().push(packet.to_vec());
        }

        fn poll(&mut self) {
            self.poll_count.fetch_add(1, Ordering::SeqCst);
        }

        fn poll_delay(&mut self) -> Option<Duration> {
            self.poll_delay_value
        }

        fn save_snapshot_state(&self) -> Option<Vec<u8>> {
            self.snapshot_data.clone()
        }

        fn restore_snapshot_state(&mut self, data: &[u8]) {
            *self.restored_state.lock().unwrap() = Some(data.to_vec());
        }

        fn on_exit(&mut self) {}
    }

    /// Factory that creates a TrackingNetBackend with optional wake and RX channels.
    struct TrackingNetBackendFactory {
        backend: Option<TrackingNetBackend>,
        tx_from_factory: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Bytes>>>>,
        wake_enabled: Arc<std::sync::atomic::AtomicBool>,
        wake_tx_from_factory: Arc<Mutex<Option<tokio::sync::mpsc::Sender<()>>>>,
    }

    impl TrackingNetBackendFactory {
        fn new(
            backend: TrackingNetBackend,
            tx_from_factory: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Bytes>>>>,
            wake_enabled: Arc<std::sync::atomic::AtomicBool>,
            wake_tx_from_factory: Arc<Mutex<Option<tokio::sync::mpsc::Sender<()>>>>,
        ) -> Self {
            Self {
                backend: Some(backend),
                tx_from_factory,
                wake_enabled,
                wake_tx_from_factory,
            }
        }
    }

    impl AsyncNetBackendFactory for TrackingNetBackendFactory {
        fn create(mut self: Box<Self>) -> SendBoxFuture<'static, std::io::Result<NetBackendHandle>> {
            let tx_from_factory = self.tx_from_factory.clone();
            let wake_enabled = self.wake_enabled.clone();
            let wake_tx_from_factory = self.wake_tx_from_factory.clone();
            Box::pin(async move {
                // Create a channel for the worker to receive RX packets on
                let (to_guest_tx, to_guest_rx) = tokio::sync::mpsc::channel(16);
                // Store the sender so tests can send packets
                *tx_from_factory.lock().unwrap() = Some(to_guest_tx);

                let backend = self.backend.take().expect("backend should be present");
                let wake_rx = if wake_enabled.load(std::sync::atomic::Ordering::SeqCst) {
                    // Create wake channel
                    let (wake_tx, wake_rx) = tokio::sync::mpsc::channel(1);
                    // Store the sender for the test to use
                    *wake_tx_from_factory.lock().unwrap() = Some(wake_tx);
                    Some(wake_rx)
                } else {
                    None
                };
                Ok(NetBackendHandle {
                    backend: Box::new(backend),
                    to_guest_rx,
                    wake_rx,
                })
            })
        }
    }

    use crate::virtio::queue::Descriptor;

    const DESC_TABLE_ADDR: u64 = 0x1000;
    const AVAIL_RING_ADDR: u64 = 0x2000;
    const USED_RING_ADDR: u64 = 0x3000;
    const PKT_DATA_ADDR: u64 = 0x4000;

    /// Write a descriptor to guest memory at the given table offset.
    fn write_descriptor(mem: &GuestMemoryMmap, index: u16, desc: Descriptor) {
        mem.write_obj(
            desc,
            GuestAddress(DESC_TABLE_ADDR + (index as u64) * 16),
        )
        .unwrap();
    }

    /// Set up the available ring and create a Queue ready to pop.
    fn setup_avail_ring(mem: &GuestMemoryMmap, head_index: u16) -> Queue {
        // Write avail ring flags and idx
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR)).unwrap();
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .unwrap();
        // Write ring[0] = head_index
        mem.write_obj(
            head_index,
            GuestAddress(AVAIL_RING_ADDR + 4),
        )
        .unwrap();

        // Write used ring flags and idx
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR)).unwrap();
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .unwrap();

        // Create and configure queue
        let mut q = Queue::new(256);
        q.size = 256;  // Must set the size that the driver negotiated
        q.ready = true;
        q.desc_table = GuestAddress(DESC_TABLE_ADDR);
        q.avail_ring = GuestAddress(AVAIL_RING_ADDR);
        q.used_ring = GuestAddress(USED_RING_ADDR);
        q
    }

    /// AC3.1: TX header-only packet (virtio header only, zero payload) returns None.
    #[test]
    fn test_header_only_tx_returns_none() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();

        // Single descriptor with exactly VIRTIO_NET_HDR_SIZE bytes (header only, no payload).
        // The production code checks `if offset > VIRTIO_NET_HDR_SIZE`, so an offset of
        // exactly VIRTIO_NET_HDR_SIZE should return None.
        let data = vec![0u8; VIRTIO_NET_HDR_SIZE];
        mem.write_slice(&data, GuestAddress(PKT_DATA_ADDR))
            .unwrap();

        let desc = Descriptor {
            addr: PKT_DATA_ADDR,
            len: VIRTIO_NET_HDR_SIZE as u32,
            flags: 0,
            next: 0,
        };
        write_descriptor(&mem, 0, desc);

        let mut q = setup_avail_ring(&mem, 0);
        let chain = q.pop(&mem).unwrap();

        let mut buf = vec![0u8; 65535 + VIRTIO_NET_HDR_SIZE];
        let result = read_tx_packet(&mem, &chain, &mut buf);

        assert_eq!(
            result, None,
            "TX packet with header only (zero payload) should return None; backend receives no data"
        );
    }

    /// AC3.1 (sub-case): TX minimal valid packet (header + 1 byte payload).
    #[test]
    fn test_minimal_valid_tx_packet() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();

        // Single descriptor with header + 1 byte payload (minimal valid packet).
        // The production code checks `if offset > VIRTIO_NET_HDR_SIZE`, so an offset of
        // VIRTIO_NET_HDR_SIZE + 1 should return Some(1).
        let data = vec![0u8; VIRTIO_NET_HDR_SIZE + 1];
        mem.write_slice(&data, GuestAddress(PKT_DATA_ADDR))
            .unwrap();

        let desc = Descriptor {
            addr: PKT_DATA_ADDR,
            len: (VIRTIO_NET_HDR_SIZE + 1) as u32,
            flags: 0,
            next: 0,
        };
        write_descriptor(&mem, 0, desc);

        let mut q = setup_avail_ring(&mem, 0);
        let chain = q.pop(&mem).unwrap();

        let mut buf = vec![0u8; 65535 + VIRTIO_NET_HDR_SIZE];
        let result = read_tx_packet(&mem, &chain, &mut buf);

        assert_eq!(
            result, Some(1),
            "TX packet with header + 1 byte payload should return Some(1)"
        );
    }

    /// AC3.2: TX max-size packet (65535 B payload).
    #[test]
    fn test_max_size_tx_packet() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();

        // Write header
        let header_data = vec![0u8; VIRTIO_NET_HDR_SIZE];
        mem.write_slice(&header_data, GuestAddress(PKT_DATA_ADDR))
            .unwrap();

        // Write payload (65535 bytes filled with 0xAB)
        let payload = vec![0xAB; 65535];
        mem.write_slice(
            &payload,
            GuestAddress(PKT_DATA_ADDR + VIRTIO_NET_HDR_SIZE as u64),
        )
        .unwrap();

        let desc = Descriptor {
            addr: PKT_DATA_ADDR,
            len: (VIRTIO_NET_HDR_SIZE + 65535) as u32,
            flags: 0,
            next: 0,
        };
        write_descriptor(&mem, 0, desc);

        let mut q = setup_avail_ring(&mem, 0);
        let chain = q.pop(&mem).unwrap();

        let mut buf = vec![0u8; 65535 + VIRTIO_NET_HDR_SIZE];
        let result = read_tx_packet(&mem, &chain, &mut buf);

        assert_eq!(result, Some(65535), "Max-size packet should return Some(65535)");
        // Verify payload was copied correctly (without header)
        assert!(buf[..65535].iter().all(|&b| b == 0xAB));
    }

    /// AC3.3: TX packet split across multiple virtio descriptors.
    #[test]
    fn test_multi_descriptor_tx() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();

        // First descriptor: header only
        let header_data = vec![0u8; VIRTIO_NET_HDR_SIZE];
        mem.write_slice(&header_data, GuestAddress(PKT_DATA_ADDR))
            .unwrap();

        // Second descriptor: 100-byte payload filled with 0xCD
        let payload = vec![0xCD; 100];
        mem.write_slice(
            &payload,
            GuestAddress(PKT_DATA_ADDR + VIRTIO_NET_HDR_SIZE as u64 + 1000),
        )
        .unwrap();

        // First descriptor: header with NEXT flag set to 1
        let desc0 = Descriptor {
            addr: PKT_DATA_ADDR,
            len: VIRTIO_NET_HDR_SIZE as u32,
            flags: 0x1, // VIRTQ_DESC_F_NEXT
            next: 1,
        };
        write_descriptor(&mem, 0, desc0);

        // Second descriptor: payload, no NEXT flag
        let desc1 = Descriptor {
            addr: PKT_DATA_ADDR + VIRTIO_NET_HDR_SIZE as u64 + 1000,
            len: 100,
            flags: 0,
            next: 0,
        };
        write_descriptor(&mem, 1, desc1);

        let mut q = setup_avail_ring(&mem, 0);
        let chain = q.pop(&mem).unwrap();

        let mut buf = vec![0u8; 65535 + VIRTIO_NET_HDR_SIZE];
        let result = read_tx_packet(&mem, &chain, &mut buf);

        assert_eq!(result, Some(100), "Multi-descriptor packet should return Some(100)");
        // Verify payload was copied correctly (without header)
        assert!(buf[..100].iter().all(|&b| b == 0xCD));
    }

    /// AC3.4: TX descriptor with header smaller than VIRTIO_NET_HDR_SIZE.
    #[test]
    fn test_truncated_header_returns_none() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();

        // Write truncated header (VIRTIO_NET_HDR_SIZE - 1 bytes)
        let truncated_header = vec![0u8; VIRTIO_NET_HDR_SIZE - 1];
        mem.write_slice(&truncated_header, GuestAddress(PKT_DATA_ADDR))
            .unwrap();

        let desc = Descriptor {
            addr: PKT_DATA_ADDR,
            len: (VIRTIO_NET_HDR_SIZE - 1) as u32,
            flags: 0,
            next: 0,
        };
        write_descriptor(&mem, 0, desc);

        let mut q = setup_avail_ring(&mem, 0);
        let chain = q.pop(&mem).unwrap();

        let mut buf = vec![0u8; 65535 + VIRTIO_NET_HDR_SIZE];
        let result = read_tx_packet(&mem, &chain, &mut buf);

        assert_eq!(
            result, None,
            "Truncated header should return None without panicking"
        );
    }

    /// AC3.5: RX packet delivered to guest RX queue with pre-populated buffer.
    #[test]
    fn test_rx_packet_delivered() {
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-net".into()).unwrap();

        // Create RX and TX queues
        let mut rx_queue = Queue::new(256);
        rx_queue.size = 256;
        rx_queue.ready = true;
        rx_queue.desc_table = GuestAddress(DESC_TABLE_ADDR);
        rx_queue.avail_ring = GuestAddress(AVAIL_RING_ADDR);
        rx_queue.used_ring = GuestAddress(USED_RING_ADDR);

        // Pre-populate one RX buffer (descriptor 0: writeable, 256 bytes)
        let desc = Descriptor {
            addr: PKT_DATA_ADDR,
            len: 256,
            flags: 0x2, // VIRTQ_DESC_F_WRITE
            next: 0,
        };
        write_descriptor(&mem, 0, desc);

        // Set up available ring with one buffer
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR)).unwrap();
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2)).unwrap();
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 4)).unwrap();

        // Initialize used ring
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR)).unwrap();
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2)).unwrap();

        let tx_queue = Queue::new(256);
        let queues = vec![rx_queue, tx_queue];
        let queue_evts = vec![
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        ];
        let stop_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resync_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let quiesce_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resume_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();

        let shared_queues = Arc::new(Mutex::new(queues.clone()));
        let shared_generation = Arc::new(AtomicU64::new(0));
        let quiesce_ack = Arc::new((Mutex::new(false), Condvar::new()));
        let shared_backend_state = Arc::new(Mutex::new(None));

        // Create TrackingNetBackend
        let (backend, _tx_received, _poll_count, _restored_state) = TrackingNetBackend::new(None);

        // Create a holder for the tx sender that the factory will populate
        let tx_from_factory = Arc::new(Mutex::new(None));
        let tx_for_test = tx_from_factory.clone();
        let wake_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wake_tx_from_factory = Arc::new(Mutex::new(None));

        let stop_fd_clone = stop_fd.try_clone().unwrap();

        let factory: Box<dyn AsyncNetBackendFactory> =
            Box::new(TrackingNetBackendFactory::new(backend, tx_from_factory, wake_enabled, wake_tx_from_factory));

        let worker = AsyncNetWorker::new(
            queues,
            queue_evts,
            interrupt,
            mem.clone(),
            factory,
            stop_fd,
            resync_fd,
            shared_queues.clone(),
            shared_generation,
            quiesce_fd,
            resume_fd,
            quiesce_ack,
            shared_backend_state,
        );

        let _handle = worker.run();

        // Wait for factory to be called (worker startup)
        poll_until("factory tx sender populated", || tx_for_test.lock().unwrap().is_some());

        // Get the sender from the factory
        let tx_sender = {
            let mut lock = tx_for_test.lock().unwrap();
            lock.take().expect("Factory should have populated tx sender")
        };

        // Send an RX packet
        let packet_data = b"hello-world";
        let packet_for_send = packet_data.to_vec();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let _ = tx_sender.send(Bytes::from(packet_for_send)).await;
            });
        });

        // Wait for packet to be written into the used ring
        poll_until("used ring incremented", || {
            mem.read_obj::<u16>(GuestAddress(USED_RING_ADDR + 2)).unwrap() > 0
        });

        // Check if packet appears in used ring
        // Used ring format: flags (u16) at +0, idx (u16) at +2, ring[idx] at +4
        let used_idx = mem.read_obj::<u16>(GuestAddress(USED_RING_ADDR + 2)).unwrap();
        assert!(used_idx > 0, "Used ring idx should have incremented after RX packet");

        // Read the packet data from guest memory
        let mut buf = vec![0u8; 256];
        mem.read_slice(&mut buf, GuestAddress(PKT_DATA_ADDR)).unwrap();

        // Verify header (first VIRTIO_NET_HDR_SIZE bytes are zeros)
        assert!(buf[..VIRTIO_NET_HDR_SIZE].iter().all(|b| *b == 0), "Header should be all zeros");

        // Verify payload
        assert_eq!(&buf[VIRTIO_NET_HDR_SIZE..VIRTIO_NET_HDR_SIZE + packet_data.len()], packet_data);

        stop_fd_clone.write(1).unwrap();
        _handle.join().expect("worker thread panicked");
    }

    /// AC3.6: RX packet dropped when no guest RX buffers available.
    #[test]
    fn test_rx_drop_no_buffers() {
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-net".into()).unwrap();

        // Create RX and TX queues with no buffers
        let rx_queue = Queue::new(256);
        let tx_queue = Queue::new(256);
        let queues = vec![rx_queue, tx_queue];

        let queue_evts = vec![
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        ];
        let stop_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resync_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let quiesce_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resume_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();

        let shared_queues = Arc::new(Mutex::new(queues.clone()));
        let shared_generation = Arc::new(AtomicU64::new(0));
        let quiesce_ack = Arc::new((Mutex::new(false), Condvar::new()));
        let shared_backend_state = Arc::new(Mutex::new(None));

        // Create TrackingNetBackend
        let (backend, _tx_received, _poll_count, _restored_state) = TrackingNetBackend::new(None);

        // Create holders for the tx sender that the factory will populate
        let tx_from_factory = Arc::new(Mutex::new(None));
        let tx_for_test = tx_from_factory.clone();
        let wake_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wake_tx_from_factory = Arc::new(Mutex::new(None));

        let stop_fd_clone = stop_fd.try_clone().unwrap();

        let factory: Box<dyn AsyncNetBackendFactory> =
            Box::new(TrackingNetBackendFactory::new(backend, tx_from_factory, wake_enabled, wake_tx_from_factory));

        let worker = AsyncNetWorker::new(
            queues,
            queue_evts,
            interrupt,
            mem,
            factory,
            stop_fd,
            resync_fd,
            shared_queues,
            shared_generation,
            quiesce_fd,
            resume_fd,
            quiesce_ack,
            shared_backend_state,
        );

        let _handle = worker.run();

        // Wait for factory to be called (worker startup)
        poll_until("factory tx sender populated", || tx_for_test.lock().unwrap().is_some());

        // Get the sender from the factory
        let tx_sender = {
            let mut lock = tx_for_test.lock().unwrap();
            lock.take().expect("Factory should have populated tx sender")
        };

        // Try to send packet (should be silently dropped, no panic)
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let _ = tx_sender.send(Bytes::from("test-packet")).await;
            });
        });

        // Stop the worker; if the packet drop caused a panic, join will propagate it
        stop_fd_clone.write(1).unwrap();
        _handle.join().expect("worker thread panicked");
    }

    /// AC3.7: Snapshot state survives quiesce/resync cycle.
    #[test]
    fn test_snapshot_state_survives_quiesce_resync() {
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-net".into()).unwrap();

        let queues = vec![Queue::new(256), Queue::new(256)];
        let queue_evts = vec![
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        ];
        let stop_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resync_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let quiesce_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resume_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();

        let shared_queues = Arc::new(Mutex::new(queues.clone()));
        let shared_generation = Arc::new(AtomicU64::new(0));
        let quiesce_ack = Arc::new((Mutex::new(false), Condvar::new()));
        let quiesce_ack_clone = quiesce_ack.clone();

        let quiesce_fd_clone = quiesce_fd.try_clone().unwrap();
        let resume_fd_clone = resume_fd.try_clone().unwrap();
        let stop_fd_clone = stop_fd.try_clone().unwrap();
        let resync_fd_clone = resync_fd.try_clone().unwrap();

        // Create TrackingNetBackend with snapshot data
        let snapshot_data = b"state-bytes".to_vec();
        let (backend, _tx_received, _poll_count, restored_state) =
            TrackingNetBackend::new(Some(snapshot_data.clone()));

        let tx_from_factory = Arc::new(Mutex::new(None));
        let wake_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wake_tx_from_factory = Arc::new(Mutex::new(None));

        let factory: Box<dyn AsyncNetBackendFactory> =
            Box::new(TrackingNetBackendFactory::new(backend, tx_from_factory, wake_enabled, wake_tx_from_factory));

        let shared_backend_state = Arc::new(Mutex::new(None));
        let shared_backend_state_clone = shared_backend_state.clone();

        let worker = AsyncNetWorker::new(
            queues,
            queue_evts,
            interrupt,
            mem,
            factory,
            stop_fd,
            resync_fd,
            shared_queues,
            shared_generation,
            quiesce_fd,
            resume_fd,
            quiesce_ack,
            shared_backend_state,
        );

        let _handle = worker.run();

        // --- First quiesce cycle: save snapshot state ---
        {
            let (lock, _) = &*quiesce_ack_clone;
            *lock.lock().unwrap() = false;
        }
        quiesce_fd_clone.write(1).unwrap();

        // Wait for ack
        {
            let (lock, cvar) = &*quiesce_ack_clone;
            let guard = lock.lock().unwrap();
            let (guard, timeout_result) = cvar
                .wait_timeout_while(guard, std::time::Duration::from_secs(5), |acked| !*acked)
                .unwrap();
            assert!(
                *guard && !timeout_result.timed_out(),
                "Net worker did not ack quiesce"
            );
        }

        // Check if snapshot state was saved
        let saved_state = shared_backend_state_clone.lock().unwrap().clone();
        assert_eq!(
            saved_state, Some(snapshot_data.clone()),
            "Snapshot state should be saved in shared_backend_state"
        );

        // Resume
        {
            let (lock, _) = &*quiesce_ack_clone;
            *lock.lock().unwrap() = false;
        }
        resume_fd_clone.write(1).unwrap();

        // --- Resync to trigger restore_snapshot_state ---
        resync_fd_clone.write(1).unwrap();

        // Wait for resync to be processed (restore_snapshot_state populates restored_state)
        let restored_state_clone = restored_state.clone();
        poll_until("restore_snapshot_state called", move || {
            restored_state_clone.lock().unwrap().is_some()
        });

        // Check if restore_snapshot_state was called
        let restored = restored_state.lock().unwrap();
        assert_eq!(
            *restored, Some(snapshot_data),
            "restore_snapshot_state should have been called with snapshot data"
        );

        stop_fd_clone.write(1).unwrap();
        _handle.join().expect("worker thread panicked");
    }

    /// AC3.8: wake_rx signal triggers poll() call.
    #[test]
    fn test_wake_rx_triggers_poll() {
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-net".into()).unwrap();

        let queues = vec![Queue::new(256), Queue::new(256)];
        let queue_evts = vec![
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        ];
        let stop_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resync_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let quiesce_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resume_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();

        let shared_queues = Arc::new(Mutex::new(queues.clone()));
        let shared_generation = Arc::new(AtomicU64::new(0));
        let quiesce_ack = Arc::new((Mutex::new(false), Condvar::new()));

        // Create TrackingNetBackend
        let (backend, _tx_received, poll_count, _restored_state) = TrackingNetBackend::new(None);
        let poll_count_clone = poll_count.clone();

        // Create holders for channels
        let tx_from_factory = Arc::new(Mutex::new(None));
        let wake_enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let wake_tx_from_factory = Arc::new(Mutex::new(None));
        let wake_for_test = wake_tx_from_factory.clone();

        let stop_fd_clone = stop_fd.try_clone().unwrap();

        let factory: Box<dyn AsyncNetBackendFactory> =
            Box::new(TrackingNetBackendFactory::new(backend, tx_from_factory, wake_enabled, wake_tx_from_factory));

        let shared_backend_state = Arc::new(Mutex::new(None));

        let worker = AsyncNetWorker::new(
            queues,
            queue_evts,
            interrupt,
            mem,
            factory,
            stop_fd,
            resync_fd,
            shared_queues,
            shared_generation,
            quiesce_fd,
            resume_fd,
            quiesce_ack,
            shared_backend_state,
        );

        let _handle = worker.run();

        // Wait for factory to be called (worker startup)
        poll_until("factory wake sender populated", || wake_for_test.lock().unwrap().is_some());

        // Get the wake sender from the factory
        let wake_sender = {
            let mut lock = wake_for_test.lock().unwrap();
            lock.take().expect("Factory should have populated wake sender")
        };

        // Get initial poll count
        let initial_count = poll_count_clone.load(Ordering::SeqCst);

        // Send wake signal
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let _ = wake_sender.send(()).await;
            });
        });

        // Wait for poll to be triggered by the wake signal
        let poll_count_for_wait = poll_count_clone.clone();
        poll_until("poll_count incremented after wake", move || {
            poll_count_for_wait.load(Ordering::SeqCst) > initial_count
        });

        let final_count = poll_count_clone.load(Ordering::SeqCst);
        assert!(
            final_count > initial_count,
            "poll_count should have incremented from {} to {}",
            initial_count,
            final_count
        );

        stop_fd_clone.write(1).unwrap();
        _handle.join().expect("worker thread panicked");
    }

    /// AC3.9: poll_delay timer fires and triggers poll().
    #[test]
    fn test_poll_delay_timer() {
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-net".into()).unwrap();

        let queues = vec![Queue::new(256), Queue::new(256)];
        let queue_evts = vec![
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        ];
        let stop_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resync_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let quiesce_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resume_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();

        let shared_queues = Arc::new(Mutex::new(queues.clone()));
        let shared_generation = Arc::new(AtomicU64::new(0));
        let quiesce_ack = Arc::new((Mutex::new(false), Condvar::new()));

        // Create TrackingNetBackend with a 50ms poll_delay
        let (mut backend, _tx_received, poll_count, _restored_state) = TrackingNetBackend::new(None);
        backend.poll_delay_value = Some(std::time::Duration::from_millis(50));
        let poll_count_clone = poll_count.clone();

        let tx_from_factory = Arc::new(Mutex::new(None));
        let wake_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wake_tx_from_factory = Arc::new(Mutex::new(None));

        let stop_fd_clone = stop_fd.try_clone().unwrap();

        let factory: Box<dyn AsyncNetBackendFactory> =
            Box::new(TrackingNetBackendFactory::new(backend, tx_from_factory, wake_enabled, wake_tx_from_factory));

        let shared_backend_state = Arc::new(Mutex::new(None));

        let worker = AsyncNetWorker::new(
            queues,
            queue_evts,
            interrupt,
            mem,
            factory,
            stop_fd,
            resync_fd,
            shared_queues,
            shared_generation,
            quiesce_fd,
            resume_fd,
            quiesce_ack,
            shared_backend_state,
        );

        let _handle = worker.run();

        // Wait for worker to start and perform at least one poll (timer fires ~50ms after start)
        poll_until("initial poll performed", || {
            poll_count_clone.load(Ordering::SeqCst) > 0
        });

        // Record count and wait for a subsequent timer-driven poll
        let initial_count = poll_count_clone.load(Ordering::SeqCst);
        poll_until("poll_count incremented by timer", || {
            poll_count_clone.load(Ordering::SeqCst) > initial_count
        });

        let final_count = poll_count_clone.load(Ordering::SeqCst);
        assert!(
            final_count > initial_count,
            "poll_count should have incremented from {} to {} via timer",
            initial_count,
            final_count
        );

        stop_fd_clone.write(1).unwrap();
        _handle.join().expect("worker thread panicked");
    }

    /// Test that the quiesce protocol works: signal quiesce → worker acks → resume.
    #[test]
    fn test_quiesce_ack_resume() {
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-net".into()).unwrap();

        let queues = vec![Queue::new(256), Queue::new(256)];
        let queue_evts = vec![
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        ];
        let stop_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resync_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let quiesce_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let resume_fd = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();

        let shared_queues = Arc::new(Mutex::new(queues.clone()));
        let shared_generation = Arc::new(AtomicU64::new(0));
        let quiesce_ack = Arc::new((Mutex::new(false), Condvar::new()));

        let quiesce_fd_clone = quiesce_fd.try_clone().unwrap();
        let resume_fd_clone = resume_fd.try_clone().unwrap();
        let stop_fd_clone = stop_fd.try_clone().unwrap();
        let quiesce_ack_clone = quiesce_ack.clone();

        let factory: Box<dyn AsyncNetBackendFactory> = Box::new(DummyNetBackendFactory);

        let shared_backend_state = Arc::new(Mutex::new(None));

        let worker = AsyncNetWorker::new(
            queues,
            queue_evts,
            interrupt,
            mem,
            factory,
            stop_fd,
            resync_fd,
            shared_queues.clone(),
            shared_generation.clone(),
            quiesce_fd,
            resume_fd,
            quiesce_ack.clone(),
            shared_backend_state,
        );

        let _handle = worker.run();

        // Give the worker time to start and create backend
        std::thread::sleep(std::time::Duration::from_millis(100));

        // --- Quiesce cycle 1: verify ack ---
        {
            let (lock, _) = &*quiesce_ack_clone;
            *lock.lock().unwrap() = false;
        }
        quiesce_fd_clone.write(1).unwrap();

        // Wait for ack with timeout
        {
            let (lock, cvar) = &*quiesce_ack_clone;
            let guard = lock.lock().unwrap();
            let (guard, timeout_result) = cvar
                .wait_timeout_while(guard, std::time::Duration::from_secs(5), |acked| !*acked)
                .unwrap();
            assert!(
                *guard && !timeout_result.timed_out(),
                "Net worker did not ack quiesce within timeout"
            );
        }

        // Verify shared queues were updated
        {
            let shared = shared_queues.lock().unwrap();
            assert_eq!(shared.len(), 2);
        }

        // Resume
        {
            let (lock, _) = &*quiesce_ack_clone;
            *lock.lock().unwrap() = false;
        }
        resume_fd_clone.write(1).unwrap();

        // Give worker time to resume
        std::thread::sleep(std::time::Duration::from_millis(50));

        // --- Quiesce cycle 2: verify it works a second time ---
        {
            let (lock, _) = &*quiesce_ack_clone;
            *lock.lock().unwrap() = false;
        }
        quiesce_fd_clone.write(1).unwrap();

        {
            let (lock, cvar) = &*quiesce_ack_clone;
            let guard = lock.lock().unwrap();
            let (guard, timeout_result) = cvar
                .wait_timeout_while(guard, std::time::Duration::from_secs(5), |acked| !*acked)
                .unwrap();
            assert!(
                *guard && !timeout_result.timed_out(),
                "Net worker did not ack second quiesce within timeout"
            );
        }

        // Resume and stop
        resume_fd_clone.write(1).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stop_fd_clone.write(1).unwrap();
    }
}
