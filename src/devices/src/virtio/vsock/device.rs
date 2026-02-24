// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
use crate::snapshot::SnapshotError;
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
    host_port_map: Option<HashMap<u16, u16>>,
    unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
    tsi_flags: TsiFlags,
    pub(crate) muxer: VsockMuxer,
    pub(crate) queue_rx: Option<Arc<Mutex<VirtQueue>>>,
    pub(crate) queue_tx: Option<Arc<Mutex<VirtQueue>>>,
    /// Snapshot buffer: holds queue state for save/restore.
    pub(crate) queues: Vec<VirtQueue>,
    pub(crate) queue_events: Vec<Arc<EventFd>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,

    muxer_quiesce_fd: EventFd,
    muxer_resume_fd: EventFd,
    muxer_quiesce_ack: Arc<(Mutex<bool>, Condvar)>,
    timesync_quiesce_fd: EventFd,
    timesync_resume_fd: EventFd,
    timesync_quiesce_ack: Arc<(Mutex<bool>, Condvar)>,
}

impl Vsock {
    /// Create a new virtio-vsock device with the given VM CID.
    pub fn new(
        cid: u64,
        host_port_map: Option<HashMap<u16, u16>>,
        unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
        tsi_flags: TsiFlags,
    ) -> super::Result<Vsock> {
        let queues: Vec<VirtQueue> = defs::QUEUE_SIZES
            .iter()
            .map(|&s| VirtQueue::new(s))
            .collect();

        Ok(Vsock {
            cid,
            host_port_map: host_port_map.clone(),
            unix_ipc_port_map: unix_ipc_port_map.clone(),
            tsi_flags,
            muxer: VsockMuxer::new(cid, host_port_map, unix_ipc_port_map, tsi_flags),
            queue_rx: None,
            queue_tx: None,
            queues,
            queue_events: Vec::new(),
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::VsockError::EventFd)?,
            device_state: DeviceState::Inactive,
            muxer_quiesce_fd: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::VsockError::EventFd)?,
            muxer_resume_fd: EventFd::new(0).map_err(super::VsockError::EventFd)?,
            muxer_quiesce_ack: Arc::new((Mutex::new(false), Condvar::new())),
            timesync_quiesce_fd: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::VsockError::EventFd)?,
            timesync_resume_fd: EventFd::new(0).map_err(super::VsockError::EventFd)?,
            timesync_quiesce_ack: Arc::new((Mutex::new(false), Condvar::new())),
        })
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
        let muxer_quiesce_fd = self
            .muxer_quiesce_fd
            .try_clone()
            .map_err(|_| ActivateError::BadActivate)?;
        let muxer_resume_fd = self
            .muxer_resume_fd
            .try_clone()
            .map_err(|_| ActivateError::BadActivate)?;
        let timesync_quiesce_fd = self
            .timesync_quiesce_fd
            .try_clone()
            .map_err(|_| ActivateError::BadActivate)?;
        let timesync_resume_fd = self
            .timesync_resume_fd
            .try_clone()
            .map_err(|_| ActivateError::BadActivate)?;
        self.muxer.activate(
            mem.clone(),
            self.queue_rx.clone().unwrap(),
            interrupt.clone(),
            rxq_kick,
            muxer_quiesce_fd,
            muxer_resume_fd,
            self.muxer_quiesce_ack.clone(),
            timesync_quiesce_fd,
            timesync_resume_fd,
            self.timesync_quiesce_ack.clone(),
        );

        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn post_restore_kick(&mut self) {
        if !self.is_activated() {
            return;
        }
        for (i, (queue, evt)) in self.queues.iter().zip(self.queue_events.iter()).enumerate() {
            if !queue.ready {
                continue;
            }
            if let Err(e) = evt.write(1) {
                error!("vsock: post_restore_kick queue {i} failed: {e}");
            }
        }
    }

    fn reset(&mut self) -> bool {
        self.device_state = DeviceState::Inactive;
        self.queue_rx = None;
        self.queue_tx = None;
        self.muxer = VsockMuxer::new(
            self.cid,
            self.host_port_map.clone(),
            self.unix_ipc_port_map.clone(),
            self.tsi_flags,
        );
        true
    }

    fn begin_snapshot_quiesce(
        &mut self,
        timeout: Duration,
    ) -> std::result::Result<(), SnapshotError> {
        debug!(
            "vsock: begin_snapshot_quiesce, activated={}",
            self.device_state.is_activated()
        );
        if !self.device_state.is_activated() {
            return Ok(());
        }

        // Quiesce the muxer thread.
        {
            let (lock, _) = &*self.muxer_quiesce_ack;
            *lock.lock().unwrap() = false;
        }
        let _ = self.muxer_quiesce_fd.write(1);

        let (lock, cvar) = &*self.muxer_quiesce_ack;
        let guard = lock.lock().unwrap();
        let (guard, wait_result) = cvar
            .wait_timeout_while(guard, timeout, |acked| !*acked)
            .unwrap();
        if !*guard || wait_result.timed_out() {
            return Err(SnapshotError::QuiesceTimeout {
                device_id: String::new(),
                timeout_ms: timeout.as_millis() as u64,
                detail: Some("vsock muxer thread did not ack quiesce".into()),
            });
        }
        drop(guard);

        // Quiesce the timesync thread (macOS only — not started on Linux).
        #[cfg(target_os = "macos")]
        {
            {
                let (lock, _) = &*self.timesync_quiesce_ack;
                *lock.lock().unwrap() = false;
            }
            let _ = self.timesync_quiesce_fd.write(1);

            let (lock, cvar) = &*self.timesync_quiesce_ack;
            let guard = lock.lock().unwrap();
            let (guard, wait_result) = cvar
                .wait_timeout_while(guard, timeout, |acked| !*acked)
                .unwrap();
            if !*guard || wait_result.timed_out() {
                // Resume the muxer thread since it already quiesced.
                let _ = self.muxer_resume_fd.write(1);
                return Err(SnapshotError::QuiesceTimeout {
                    device_id: String::new(),
                    timeout_ms: timeout.as_millis() as u64,
                    detail: Some("vsock timesync thread did not ack quiesce".into()),
                });
            }
        }

        Ok(())
    }

    fn abort_snapshot_quiesce(&mut self) {
        debug!(
            "vsock: abort_snapshot_quiesce (resume workers), activated={}",
            self.device_state.is_activated()
        );
        if !self.device_state.is_activated() {
            return;
        }
        // Reset ack flags and resume threads.
        {
            let (lock, _) = &*self.muxer_quiesce_ack;
            *lock.lock().unwrap() = false;
        }
        let _ = self.muxer_resume_fd.write(1);
        // Timesync thread is macOS-only — only resume it there.
        #[cfg(target_os = "macos")]
        {
            let (lock, _) = &*self.timesync_quiesce_ack;
            *lock.lock().unwrap() = false;
            let _ = self.timesync_resume_fd.write(1);
        }
    }

    fn sync_queues_for_snapshot(&mut self) {
        if let Some(ref qrx) = self.queue_rx {
            self.queues[RXQ_INDEX] = qrx.lock().unwrap().clone();
        }
        if let Some(ref qtx) = self.queue_tx {
            self.queues[TXQ_INDEX] = qtx.lock().unwrap().clone();
        }
        debug!(
            "vsock: sync_queues_for_snapshot: rx(next_avail={}, next_used={}) tx(next_avail={}, next_used={})",
            self.queues[RXQ_INDEX].next_avail(), self.queues[RXQ_INDEX].next_used(),
            self.queues[TXQ_INDEX].next_avail(), self.queues[TXQ_INDEX].next_used(),
        );
    }

    fn post_snapshot_restore(&mut self) {
        debug!(
            "vsock: post_snapshot_restore called, activated={}",
            self.device_state.is_activated()
        );

        let rx_q = &self.queues[RXQ_INDEX];
        let tx_q = &self.queues[TXQ_INDEX];
        debug!(
            "vsock: restore queues from snapshot: rx(ready={}, size={}, next_avail={}, next_used={}) tx(ready={}, size={}, next_avail={}, next_used={})",
            rx_q.ready, rx_q.size, rx_q.next_avail(), rx_q.next_used(),
            tx_q.ready, tx_q.size, tx_q.next_avail(), tx_q.next_used(),
        );

        if let Some(ref qrx) = self.queue_rx {
            *qrx.lock().unwrap() = self.queues[RXQ_INDEX].clone();
        }
        if let Some(ref qtx) = self.queue_tx {
            *qtx.lock().unwrap() = self.queues[TXQ_INDEX].clone();
        }
    }
}
