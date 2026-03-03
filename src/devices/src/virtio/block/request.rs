// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Pure VIRTIO block request data structures: headers, parsed requests, metrics.
//! No I/O; testable under Miri and with proptest.

#[cfg(loom)]
use loom::sync::atomic::AtomicU64;
#[cfg(not(loom))]
use std::sync::atomic::AtomicU64;

use std::io;
use vm_memory::ByteValued;

use super::VolatileSliceGuard;

/// Metrics for the async block worker.
pub struct AsyncWorkerMetrics {
    /// Number of requests currently in flight
    pub in_flight: AtomicU64,
    /// Total read requests processed
    pub reads: AtomicU64,
    /// Total write requests processed
    pub writes: AtomicU64,
    /// Total flush requests processed
    pub flushes: AtomicU64,
    /// Total bytes read
    pub bytes_read: AtomicU64,
    /// Total bytes written
    pub bytes_written: AtomicU64,
    /// Cumulative read latency in microseconds
    pub read_latency_us: AtomicU64,
    /// Cumulative write latency in microseconds
    pub write_latency_us: AtomicU64,
    /// Number of read tasks currently running
    pub concurrent_reads: AtomicU64,
    /// Peak concurrent reads
    pub peak_concurrent_reads: AtomicU64,
}

impl Default for AsyncWorkerMetrics {
    fn default() -> Self {
        AsyncWorkerMetrics {
            in_flight: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            read_latency_us: AtomicU64::new(0),
            write_latency_us: AtomicU64::new(0),
            concurrent_reads: AtomicU64::new(0),
            peak_concurrent_reads: AtomicU64::new(0),
        }
    }
}

/// Request error types for async block operations.
#[allow(dead_code)]
#[derive(Debug)]
pub enum RequestError {
    Discarding(io::Error),
    FlushingToDisk(io::Error),
    InvalidDataLength,
    ReadingFromDescriptor(io::Error),
    WritingToDescriptor(io::Error),
    WritingZeroes(io::Error),
    UnknownRequest,
}

/// The request header represents the mandatory fields of each block device request.
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct RequestHeader {
    pub request_type: u32,
    pub _reserved: u32,
    pub sector: u64,
}
// Safe because RequestHeader only contains plain data.
unsafe impl ByteValued for RequestHeader {}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct DiscardWriteData {
    pub sector: u64,
    pub num_sectors: u32,
    pub flags: u32,
}
// Safe because DiscardWriteData only contains plain data.
unsafe impl ByteValued for DiscardWriteData {}

/// The type of block request to process.
pub(super) enum Request {
    Read {
        bufs: Vec<VolatileSliceGuard>,
        offset: u64,
    },
    Write {
        bufs: Vec<VolatileSliceGuard>,
        offset: u64,
    },
    Flush,
    GetId {
        buf: VolatileSliceGuard,
    },
    Discard {
        offset: u64,
        len: u64,
    },
    WriteZeroes {
        offset: u64,
        len: u64,
        unmap: bool,
    },
}

/// Result of processing a request.
pub(super) struct RequestResult {
    pub(super) index: u16,
    #[allow(dead_code)]
    pub(super) status: u8,
    pub(super) len: u32,
}

/// A parsed request waiting to be processed.
pub(super) struct ParsedRequest {
    pub(super) request: Request,
    pub(super) index: u16,
    pub(super) status_ptr: *mut u8,
}

// SAFETY: ParsedRequest contains a raw pointer to guest memory which remains valid
// for the lifetime of the request processing. The pointer is only dereferenced
// within the async worker thread.
unsafe impl Send for ParsedRequest {}

/// A queued write request with metadata for batching.
pub(super) struct QueuedWrite {
    pub(super) parsed: ParsedRequest,
    pub(super) offset: u64,
    pub(super) len: usize,
    /// Sequence number for ordering (higher = newer)
    pub(super) seq: u64,
}

/// Result of a batch write operation.
pub(super) struct BatchWriteResult {
    /// Results for each write in the batch (index, status, len)
    pub(super) results: Vec<(u16, u8, u32, *mut u8)>,
    /// Total bytes written
    pub(super) total_bytes: u64,
    /// Total time in microseconds
    pub(super) elapsed_us: u64,
}
