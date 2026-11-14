// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.
use crate::virtio::net::{Error, Result};
use crate::virtio::net::{NUM_QUEUES, QUEUE_CONFIG, QUEUE_SIZES};
use crate::virtio::queue::Error as QueueError;
use crate::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, InterruptTransport, Queue,
    QueueConfig, VirtioDevice, TYPE_NET,
};
use crate::Error as DeviceError;

use super::backend::{ReadError, WriteError};
use super::worker::NetWorker;

use log::{debug, error};

use std::cmp;
use std::io::Write;
use std::os::fd::RawFd;
use std::path::PathBuf;
use utils::eventfd::{EventFd, EFD_NONBLOCK};
use virtio_bindings::virtio_net::VIRTIO_NET_F_MAC;
use vm_memory::{ByteValued, GuestMemoryError, GuestMemoryMmap};

const VIRTIO_F_VERSION_1: u32 = 32;

#[derive(Debug)]
pub enum FrontendError {
    DescriptorChainTooSmall,
    EmptyQueue,
    GuestMemory(GuestMemoryError),
    QueueError(QueueError),
    ReadOnlyDescriptor,
}

#[derive(Debug)]
pub enum RxError {
    Backend(ReadError),
    DeviceError(DeviceError),
}

#[derive(Debug)]
pub enum TxError {
    Backend(WriteError),
    DeviceError(DeviceError),
    QueueError(QueueError),
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioNetConfig {
    mac: [u8; 6],
    status: u16,
    max_virtqueue_pairs: u16,
}

// SAFETY: VirtioNetConfig is #[repr(C, packed)] with no padding bytes; all bit patterns are valid for all fields.
unsafe impl ByteValued for VirtioNetConfig {}

use super::async_backend::AsyncNetBackendFactory;
use super::async_worker::AsyncNetWorker;

/// Configuration for virtio-net backends.
///
/// The `Clone` variants (unix stream, unix gram, tap) can be used with the
/// synchronous NetWorker. The `CustomAsyncFactory` variant uses the async
/// worker and cannot be cloned.
pub enum VirtioNetBackend {
    UnixstreamFd(RawFd),
    UnixstreamPath(PathBuf),
    UnixgramFd(RawFd),
    UnixgramPath(PathBuf, bool),
    #[cfg(target_os = "linux")]
    Tap(String),
    /// Custom async backend using factory pattern.
    /// The factory creates the backend inside the worker's tokio runtime.
    CustomAsyncFactory(Box<dyn AsyncNetBackendFactory>),
}

impl Clone for VirtioNetBackend {
    /// # Double-close risk for fd-carrying variants
    ///
    /// `UnixstreamFd` and `UnixgramFd` store a raw `RawFd` integer. Cloning duplicates
    /// the integer without duplicating the underlying OS file description. If both the
    /// original and the clone are independently passed to `NetWorker::new`, each call to
    /// `OwnedFd::from_raw_fd` inside the worker will assume exclusive ownership of the
    /// same fd, causing a double-close (UB) when the second `OwnedFd` is dropped.
    ///
    /// The only safe call site is `Net::activate` (device.rs), where the clone is stored
    /// back in `self.cfg_backend` as a re-activation guard while the original is moved into
    /// `NetWorker::new`. Callers must ensure that at most one copy reaches `NetWorker::new`
    /// at any time. Do not clone these variants for any other purpose.
    fn clone(&self) -> Self {
        match self {
            Self::UnixstreamFd(fd) => Self::UnixstreamFd(*fd),
            Self::UnixstreamPath(p) => Self::UnixstreamPath(p.clone()),
            Self::UnixgramFd(fd) => Self::UnixgramFd(*fd),
            Self::UnixgramPath(p, b) => Self::UnixgramPath(p.clone(), *b),
            #[cfg(target_os = "linux")]
            Self::Tap(s) => Self::Tap(s.clone()),
            Self::CustomAsyncFactory(_) => panic!("CustomAsyncFactory cannot be cloned"),
        }
    }
}

pub struct Net {
    id: String,
    /// Backend configuration. Stored as Option so async factory can be taken.
    cfg_backend: Option<VirtioNetBackend>,

    avail_features: u64,
    acked_features: u64,

    queues: Vec<Queue>,
    queue_evts: Vec<EventFd>,

    pub(crate) device_state: DeviceState,

    config: VirtioNetConfig,

    /// Stop event for async worker shutdown
    worker_stop_fd: EventFd,
}

impl Net {
    /// Create a new virtio network device using the backend
    pub fn new(
        id: String,
        cfg_backend: VirtioNetBackend,
        mac: [u8; 6],
        features: u32,
    ) -> Result<Self> {
        let avail_features = features as u64 | (1 << VIRTIO_NET_F_MAC) | (1 << VIRTIO_F_VERSION_1);

        let mut queue_evts = Vec::new();
        for _ in QUEUE_SIZES.iter() {
            queue_evts.push(EventFd::new(EFD_NONBLOCK).map_err(Error::EventFd)?);
        }

        let queues: Vec<Queue> = QUEUE_SIZES.iter().map(|&s| Queue::new(s)).collect();

        let config = VirtioNetConfig {
            mac,
            status: 0,
            max_virtqueue_pairs: 0,
        };

        let worker_stop_fd = EventFd::new(EFD_NONBLOCK).map_err(Error::EventFd)?;

        Ok(Net {
            id,
            cfg_backend: Some(cfg_backend),

            avail_features,
            acked_features: 0u64,

            queues,
            queue_evts,
            device_state: DeviceState::Inactive,
            config,
            worker_stop_fd,
        })
    }

    /// Provides the ID of this net device.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Provides the virtio-net backend of this net device.
    /// Returns None if the backend has been consumed (e.g., async factory activated).
    pub fn backend(&self) -> Option<&VirtioNetBackend> {
        self.cfg_backend.as_ref()
    }
}

impl VirtioDevice for Net {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn device_type(&self) -> u32 {
        TYPE_NET
    }

    fn device_name(&self) -> &str {
        "net"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &QUEUE_CONFIG
    }

    fn queues(&self) -> &[Queue] {
        &self.queues
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }

    fn queue_events(&self) -> &[EventFd] {
        &self.queue_evts
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        log::warn!(
            "Net: guest driver attempted to write device config (offset={:x}, len={:x})",
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
        let [rx_q, tx_q]: [_; NUM_QUEUES] = queues.try_into().map_err(|_| {
            error!("Cannot perform activate. Expected {} queue(s)", NUM_QUEUES);
            ActivateError::BadActivate
        })?;

        // Take ownership of the backend - for async factory we need to move it to the worker
        let backend = self.cfg_backend.take().ok_or(ActivateError::BadActivate)?;

        match backend {
            VirtioNetBackend::CustomAsyncFactory(factory) => {
                debug!("virtio-net ({}): starting async worker", self.id());
                // AsyncNetWorker still uses old-style Vec<Queue> + Vec<EventFd>
                let queue_list = vec![rx_q.queue.clone(), tx_q.queue.clone()];
                let queue_evts = vec![
                    rx_q.event.try_clone().unwrap(),
                    tx_q.event.try_clone().unwrap(),
                ];
                let worker = AsyncNetWorker::new(
                    queue_list,
                    queue_evts,
                    interrupt.clone(),
                    mem.clone(),
                    factory,
                    self.worker_stop_fd.try_clone().unwrap(),
                );
                worker.run();
                self.device_state = DeviceState::Activated(mem, interrupt);
                Ok(())
            }
            sync_backend => {
                // Put the backend back for sync path (it's cloneable)
                self.cfg_backend = Some(sync_backend.clone());

                match NetWorker::new(
                    rx_q,
                    tx_q,
                    interrupt.clone(),
                    mem.clone(),
                    self.acked_features,
                    sync_backend,
                ) {
                    Ok(worker) => {
                        worker.run();
                        self.device_state = DeviceState::Activated(mem, interrupt);
                        Ok(())
                    }
                    Err(err) => {
                        error!(
                            "Error activating virtio-net ({}) backend: {err:?}",
                            self.id()
                        );
                        Err(ActivateError::BadActivate)
                    }
                }
            }
        }
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    // ---------------------------------------------------------------------------
    // ByteValued round-trip for VirtioNetConfig
    //
    // VirtioNetConfig is `#[repr(C, packed)]` (no padding) and is written to
    // guest config space via ByteValued::as_slice.  A size or layout mismatch
    // would corrupt network MAC, status, or queue-pair count configuration.
    // ---------------------------------------------------------------------------

    /// Proof: any bit pattern is a valid VirtioNetConfig (ByteValued correctness).
    ///
    /// VirtioNetConfig is `#[repr(C, packed)]` with fields:
    ///   mac([u8; 6]) + status(u16) + max_virtqueue_pairs(u16) = 10 bytes total.
    /// The packed repr eliminates all padding; all bit patterns are valid (POD).
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_byte_valued_virtio_net_config_roundtrip() {
        let bytes: [u8; 10] = kani::any();
        // from_slice must succeed for any 10-byte input — no invalid bit patterns.
        let val = VirtioNetConfig::from_slice(&bytes)
            .expect("VirtioNetConfig: from_slice must succeed for any 10 bytes");
        // as_slice must produce exactly size_of::<VirtioNetConfig>() bytes.
        kani::assert(
            val.as_slice().len() == std::mem::size_of::<VirtioNetConfig>(),
            "VirtioNetConfig: as_slice length must equal size_of",
        );
        // Bytes are preserved identically (identity round-trip).
        kani::assert(
            val.as_slice() == bytes,
            "VirtioNetConfig: byte round-trip must be identity",
        );
        kani::cover!(bytes == [0u8; 10], "all-zero VirtioNetConfig exercised");
        kani::cover!(bytes == [0xFFu8; 10], "all-ones VirtioNetConfig exercised");
    }

    /// Verify: VirtioNetConfig size is exactly 10 bytes.
    ///
    /// mac(6) + status(2) + max_virtqueue_pairs(2) = 10 bytes.
    /// `#[repr(C, packed)]` eliminates any trailing or inter-field padding.
    /// This is a compile-time assertion (const usize at compile time).
    const _: () = {
        let _ = [(); 1][if std::mem::size_of::<VirtioNetConfig>() == 10 {
            0
        } else {
            1
        }];
    };
}
