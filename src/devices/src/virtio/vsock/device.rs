// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use utils::byte_order;
use utils::eventfd::EventFd;
use vm_memory::GuestMemoryMmap;

use super::super::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, Queue as VirtQueue, QueueConfig,
    VirtioDevice,
};
use super::muxer::VsockMuxer;
use super::packet::VsockPacket;
use super::TsiFlags;
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;

pub(crate) const RXQ_INDEX: usize = 0;
pub(crate) const TXQ_INDEX: usize = 1;
pub(crate) const EVQ_INDEX: usize = 2;

/// The virtio features supported by our vsock device:
/// - VIRTIO_F_VERSION_1: the device conforms to at least version 1.0 of the VirtIO spec.
/// - VIRTIO_F_IN_ORDER: the device returns used buffers in the same order that the driver makes
///   them available.
pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_F_VERSION_1 as u64)
    | (1 << uapi::VIRTIO_F_IN_ORDER as u64)
    | (1 << uapi::VIRTIO_VSOCK_F_DGRAM);

pub struct Vsock {
    cid: u64,
    host_port_map: Option<super::HostPortMap>,
    unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
    tsi_flags: TsiFlags,
    socket_policy: super::policy::SocketPolicy,
    pub(crate) muxer: VsockMuxer,
    pub(crate) queue_rx: Option<Arc<Mutex<VirtQueue>>>,
    pub(crate) queue_tx: Option<Arc<Mutex<VirtQueue>>>,
    /// Device-local queue state.
    pub(crate) queues: Vec<VirtQueue>,
    pub(crate) queue_events: Vec<Arc<EventFd>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    stop_evt: EventFd,
    pub(crate) device_state: DeviceState,
}

impl Vsock {
    /// Create a new virtio-vsock device with the given VM CID.
    pub fn new(
        cid: u64,
        host_port_map: Option<super::HostPortMap>,
        unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
        tsi_flags: TsiFlags,
    ) -> super::Result<Vsock> {
        let queues: Vec<VirtQueue> = defs::QUEUE_SIZES
            .iter()
            .map(|&s| VirtQueue::new(s))
            .collect();

        let socket_policy = super::policy::SocketPolicy::default();
        Ok(Vsock {
            cid,
            host_port_map: host_port_map.clone(),
            unix_ipc_port_map: unix_ipc_port_map.clone(),
            tsi_flags,
            socket_policy: socket_policy.clone(),
            muxer: VsockMuxer::new(
                cid,
                host_port_map,
                unix_ipc_port_map,
                tsi_flags,
                socket_policy,
            ),
            queue_rx: None,
            queue_tx: None,
            queues,
            queue_events: Vec::new(),
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::VsockError::EventFd)?,
            stop_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::VsockError::EventFd)?,
            device_state: DeviceState::Inactive,
        })
    }

    pub fn set_socket_policy(&mut self, policy: super::policy::SocketPolicy) {
        self.socket_policy = policy.clone();
        self.muxer.socket_policy = policy;
    }

    /// Connection rule retained across guest-controlled transport resets.
    pub fn socket_policy(&self) -> &super::policy::SocketPolicy {
        &self.socket_policy
    }

    pub fn id(&self) -> &str {
        defs::VSOCK_DEV_ID
    }

    pub fn cid(&self) -> u64 {
        self.cid
    }

    /// Walk the driver-provided RX queue buffers and attempt to fill them up with any data that we
    /// have pending. Return `true` if descriptors have been added to the used ring, and `false`
    /// otherwise.
    pub fn process_stream_rx(&mut self) -> bool {
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => unreachable!(),
        };

        let mut have_used = false;

        let queue_rx = self
            .queue_rx
            .as_ref()
            .expect("queue_rx should exist when activated");
        let mut queue_rx = queue_rx.lock().unwrap();
        debug!(
            "vsock: process_stream_rx: next_avail={} next_used={} pending_rx={}",
            queue_rx.next_avail(),
            queue_rx.next_used(),
            self.muxer.has_pending_rx()
        );
        while let Some(head) = queue_rx.pop(mem) {
            let used_len = match VsockPacket::from_rx_virtq_head(&head) {
                Ok(mut pkt) => {
                    if self.muxer.recv_pkt(&mut pkt).is_ok() {
                        debug!(
                            "vsock: RX pkt: op={} src={}:{} dst={}:{} len={} type={}",
                            pkt.op(),
                            pkt.src_cid(),
                            pkt.src_port(),
                            pkt.dst_cid(),
                            pkt.dst_port(),
                            pkt.len(),
                            pkt.type_()
                        );
                        pkt.hdr().len() as u32 + pkt.len()
                    } else {
                        queue_rx.undo_pop();
                        break;
                    }
                }
                Err(e) => {
                    warn!("vsock: RX queue head error: {e:?}");
                    0
                }
            };

            have_used = true;
            if let Err(e) = queue_rx.add_used(mem, head.index, used_len) {
                error!("vsock: RX add_used failed: {e:?}");
            }
        }

        self.queues[RXQ_INDEX] = queue_rx.clone();

        if have_used {
            debug!(
                "vsock: process_stream_rx: delivered packets, next_avail={} next_used={}",
                self.queues[RXQ_INDEX].next_avail(),
                self.queues[RXQ_INDEX].next_used()
            );
        }
        have_used
    }

    /// Walk the driver-provided TX queue buffers, package them up as vsock packets, and process
    /// them. Return `true` if descriptors have been added to the used ring, and `false` otherwise.
    pub fn process_stream_tx(&mut self) -> bool {
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => unreachable!(),
        };

        let mut have_used = false;

        let queue_tx = self
            .queue_tx
            .as_ref()
            .expect("queue_tx should exist when activated");
        let mut queue_tx = queue_tx.lock().unwrap();
        debug!(
            "vsock: process_stream_tx: next_avail={} next_used={}",
            queue_tx.next_avail(),
            queue_tx.next_used()
        );
        while let Some(head) = queue_tx.pop(mem) {
            let pkt = match VsockPacket::from_tx_virtq_head(&head) {
                Ok(pkt) => pkt,
                Err(e) => {
                    error!("vsock: TX packet read error: {e:?}");
                    have_used = true;
                    if let Err(e) = queue_tx.add_used(mem, head.index, 0) {
                        error!("vsock: TX add_used failed: {e:?}");
                    }
                    continue;
                }
            };

            debug!(
                "vsock: TX pkt: op={} src={}:{} dst={}:{} len={} type={}",
                pkt.op(),
                pkt.src_cid(),
                pkt.src_port(),
                pkt.dst_cid(),
                pkt.dst_port(),
                pkt.len(),
                pkt.type_()
            );

            if pkt.type_() == uapi::VSOCK_TYPE_DGRAM {
                if self.muxer.send_dgram_pkt(&pkt).is_err() {
                    queue_tx.undo_pop();
                    break;
                }
            } else if self.muxer.send_stream_pkt(&pkt).is_err() {
                queue_tx.undo_pop();
                break;
            }

            have_used = true;
            if let Err(e) = queue_tx.add_used(mem, head.index, 0) {
                error!("vsock: TX add_used failed: {e:?}");
            }
        }

        self.queues[TXQ_INDEX] = queue_tx.clone();

        have_used
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::legacy::DummyIrqChip;
    use vm_memory::GuestAddress;

    static WORKER_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn worker_count() -> usize {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|entry| std::fs::read_to_string(entry.ok()?.path().join("comm")).ok())
            .filter(|name| matches!(name.trim(), "vsock muxer" | "vsock reaper"))
            .count()
    }

    fn assert_worker_count(expected: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let count = worker_count();
            if count == expected {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "expected {expected} vsock workers, found {count}"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn configured_policy_survives_reset() {
        let _guard = WORKER_TEST_LOCK.lock().unwrap();
        use super::super::policy::{AllowlistPolicy, SocketAddress, SocketOperation, SocketPolicy};
        use nix::errno::Errno;
        use std::os::fd::AsRawFd;
        let host_ports = Some(HashMap::from([(18080, "127.0.0.1:8080".parse().unwrap())]));
        let unix_ports = Some(HashMap::from([
            (7000, (PathBuf::from("/tmp/reset-rule.sock"), false)),
            (7001, (PathBuf::from("/tmp/reset-listen.sock"), true)),
        ]));
        let flags = TsiFlags::HIJACK_UNIX;
        let mut device = Vsock::new(42, host_ports.clone(), unix_ports.clone(), flags).unwrap();
        let features = device.avail_features;
        let activate_fd = device.activate_evt.as_raw_fd();
        device.set_socket_policy(SocketPolicy {
            policy: Some(Arc::new(AllowlistPolicy::new([]))),
            internet_enabled: false,
            host_sockets: None,
        });
        for _ in 0..2 {
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let interrupt =
                InterruptTransport::new(DummyIrqChip::new().into(), "test-vsock".into()).unwrap();
            let queues = defs::QUEUE_SIZES
                .iter()
                .map(|&size| DeviceQueue {
                    queue: VirtQueue::new(size),
                    event: Arc::new(EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap()),
                })
                .collect();
            device.activate(mem, interrupt, queues).unwrap();
            use nix::sys::socket::SockaddrLike;
            let unix = nix::sys::socket::UnixAddr::new("/tmp/reset-rule.sock").unwrap();
            let addr: SocketAddress =
                unsafe { SocketAddress::from_raw(unix.as_ptr().cast(), Some(unix.len())) }.unwrap();
            assert_eq!(
                device
                    .muxer
                    .socket_policy
                    .check(SocketOperation::UnixConnect, &addr),
                Err(Errno::EACCES)
            );
            assert!(!device.muxer.socket_policy.internet_enabled);
            assert_eq!(device.cid(), 42);
            device
                .muxer
                .assert_configuration(42, &host_ports, &unix_ports, flags);
            assert_eq!(device.avail_features, features);
            assert_eq!(device.activate_evt.as_raw_fd(), activate_fd);
            assert!(device.reset());
        }
    }

    #[test]
    fn repeated_reset_joins_workers() {
        let _guard = WORKER_TEST_LOCK.lock().unwrap();
        let baseline = worker_count();
        let mut device = Vsock::new(3, None, None, TsiFlags::empty()).unwrap();
        for _ in 0..16 {
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let interrupt =
                InterruptTransport::new(DummyIrqChip::new().into(), "test-vsock".into()).unwrap();
            let queues = defs::QUEUE_SIZES
                .iter()
                .map(|&size| DeviceQueue {
                    queue: VirtQueue::new(size),
                    event: Arc::new(EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap()),
                })
                .collect();
            device.activate(mem, interrupt, queues).unwrap();
            assert_worker_count(baseline + 2);
            assert!(device.reset());
            assert_worker_count(baseline);
        }
    }
}

impl Drop for Vsock {
    fn drop(&mut self) {
        self.muxer.stop(&self.stop_evt);
    }
}

impl VirtioDevice for Vsock {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_VSOCK
    }

    fn device_name(&self) -> &str {
        "vsock"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn queues(&self) -> &[VirtQueue] {
        &self.queues
    }

    fn queues_mut(&mut self) -> &mut [VirtQueue] {
        &mut self.queues
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        match offset {
            0 if data.len() == 8 => byte_order::write_le_u64(data, self.cid()),
            0 if data.len() == 4 => {
                byte_order::write_le_u32(data, (self.cid() & 0xffff_ffff) as u32)
            }
            4 if data.len() == 4 => {
                byte_order::write_le_u32(data, ((self.cid() >> 32) & 0xffff_ffff) as u32)
            }
            _ => warn!(
                "virtio-vsock received invalid read request of {} bytes at offset {}",
                data.len(),
                offset
            ),
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if queues.len() != defs::NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                defs::NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        if self.activate_evt.write(1).is_err() {
            error!("Cannot write to activate_evt",);
            return Err(ActivateError::BadActivate);
        }

        // Store queue events for event handling.
        self.queue_events = queues.iter().map(|dq| dq.event.clone()).collect();

        // Extract queues from DeviceQueues and wrap in Arc<Mutex<>>.
        let mut queues_vec: Vec<VirtQueue> = queues.into_iter().map(|dq| dq.queue).collect();
        // Note: EVQ (index 2) is currently unused, we just take it to maintain the vec.
        let _evq = queues_vec.pop().unwrap();
        let tx_queue = queues_vec.pop().unwrap();
        let rx_queue = queues_vec.pop().unwrap();

        self.queue_tx = Some(Arc::new(Mutex::new(tx_queue)));
        self.queue_rx = Some(Arc::new(Mutex::new(rx_queue)));

        let rxq_kick = (*self.queue_events[RXQ_INDEX])
            .try_clone()
            .map_err(|_| ActivateError::BadActivate)?;
        self.muxer.activate(
            mem.clone(),
            self.queue_rx.clone().unwrap(),
            interrupt.clone(),
            rxq_kick,
            self.stop_evt
                .try_clone()
                .map_err(|_| ActivateError::BadActivate)?,
        );

        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn reset(&mut self) -> bool {
        self.muxer.stop(&self.stop_evt);
        self.device_state = DeviceState::Inactive;
        self.queue_rx = None;
        self.queue_tx = None;
        self.muxer = VsockMuxer::new(
            self.cid,
            self.host_port_map.clone(),
            self.unix_ipc_port_map.clone(),
            self.tsi_flags,
            self.socket_policy.clone(),
        );
        true
    }
}
