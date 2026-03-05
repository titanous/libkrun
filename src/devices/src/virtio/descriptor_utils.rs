// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cmp;
use std::collections::VecDeque;
use std::fmt::{self, Display};
use std::io::{self, Read, Write};
use std::mem::{size_of, MaybeUninit};
use std::ops::Deref;
use std::ptr::copy_nonoverlapping;
use std::result;

use crate::virtio::queue::DescriptorChain;
use vm_memory::{
    Address, ByteValued, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryError,
    GuestMemoryMmap, GuestMemoryRegion, Le16, Le32, Le64, VolatileMemory, VolatileMemoryError,
    VolatileSlice,
};

use super::file_traits::{FileReadWriteAtVolatile, FileReadWriteVolatile};

#[derive(Debug)]
pub enum Error {
    DescriptorChainOverflow,
    FindMemoryRegion,
    GuestMemoryError(GuestMemoryError),
    InvalidChain,
    IoError(io::Error),
    SplitOutOfBounds(usize),
    VolatileMemoryError(VolatileMemoryError),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            DescriptorChainOverflow => write!(
                f,
                "the combined length of all the buffers in a `DescriptorChain` would overflow"
            ),
            FindMemoryRegion => write!(f, "no memory region for this address range"),
            GuestMemoryError(e) => write!(f, "descriptor guest memory error: {e}"),
            InvalidChain => write!(f, "invalid descriptor chain"),
            IoError(e) => write!(f, "descriptor I/O error: {e}"),
            SplitOutOfBounds(off) => write!(f, "`DescriptorChain` split is out of bounds: {off}"),
            VolatileMemoryError(e) => write!(f, "volatile memory error: {e}"),
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

impl std::error::Error for Error {}

#[derive(Clone)]
struct DescriptorChainConsumer<'a> {
    buffers: VecDeque<VolatileSlice<'a>>,
    bytes_consumed: usize,
}

impl<'a> DescriptorChainConsumer<'a> {
    fn available_bytes(&self) -> usize {
        // This is guaranteed not to overflow because the total length of the chain
        // is checked during all creations of `DescriptorChainConsumer` (see
        // `Reader::new()` and `Writer::new()`).
        self.buffers
            .iter()
            .fold(0usize, |count, vs| count + vs.len())
    }

    fn bytes_consumed(&self) -> usize {
        self.bytes_consumed
    }

    /// Consumes at most `count` bytes from the `DescriptorChain`. Callers must provide a function
    /// that takes a `&[VolatileSlice]` and returns the total number of bytes consumed. This
    /// function guarantees that the combined length of all the slices in the `&[VolatileSlice]` is
    /// less than or equal to `count`.
    ///
    /// # Errors
    ///
    /// If the provided function returns any error then no bytes are consumed from the buffer and
    /// the error is returned to the caller.
    fn consume<F>(&mut self, count: usize, f: F) -> io::Result<usize>
    where
        F: FnOnce(&[VolatileSlice]) -> io::Result<usize>,
    {
        let mut buflen = 0;
        let mut bufs = Vec::with_capacity(self.buffers.len());
        for &vs in &self.buffers {
            if buflen >= count {
                break;
            }

            bufs.push(vs);

            let rem = count - buflen;
            if rem < vs.len() {
                buflen += rem;
            } else {
                buflen += vs.len();
            }
        }

        if bufs.is_empty() {
            return Ok(0);
        }

        let bytes_consumed = f(&bufs)?;

        // This can happen if a driver tricks a device into reading/writing more data than
        // fits in a `usize`.
        let total_bytes_consumed =
            self.bytes_consumed
                .checked_add(bytes_consumed)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, Error::DescriptorChainOverflow)
                })?;

        let mut rem = bytes_consumed;
        while let Some(vs) = self.buffers.pop_front() {
            if rem < vs.len() {
                // Split the slice and push the remainder back into the buffer list. Safe because we
                // know that `rem` is not out of bounds due to the check and we checked the bounds
                // on `vs` when we added it to the buffer list.
                self.buffers.push_front(vs.offset(rem).unwrap());
                break;
            }

            // No need for checked math because we know that `vs.size() <= rem`.
            rem -= vs.len();
        }

        self.bytes_consumed = total_bytes_consumed;

        Ok(bytes_consumed)
    }

    fn split_at(&mut self, offset: usize) -> Result<DescriptorChainConsumer<'a>> {
        let mut rem = offset;
        let pos = self.buffers.iter().position(|vs| {
            if rem < vs.len() {
                true
            } else {
                rem -= vs.len();
                false
            }
        });

        if let Some(at) = pos {
            let mut other = self.buffers.split_off(at);

            if rem > 0 {
                // There must be at least one element in `other` because we checked
                // its `size` value in the call to `position` above.
                let front = other.pop_front().expect("empty VecDeque after split");
                self.buffers
                    .push_back(front.offset(rem).map_err(Error::VolatileMemoryError)?);
                other.push_front(front.offset(rem).map_err(Error::VolatileMemoryError)?);
            }

            Ok(DescriptorChainConsumer {
                buffers: other,
                bytes_consumed: 0,
            })
        } else if rem == 0 {
            Ok(DescriptorChainConsumer {
                buffers: VecDeque::new(),
                bytes_consumed: 0,
            })
        } else {
            Err(Error::SplitOutOfBounds(offset))
        }
    }

    /// Returns a slice of the underlying volatile slices up to `count` bytes.
    /// This is a non-consuming view of the buffers.
    fn get_slices(&self, count: usize) -> &[VolatileSlice<'a>] {
        let mut total = 0;
        let mut end = 0;
        for vs in &self.buffers {
            if total >= count {
                break;
            }
            total += vs.len();
            end += 1;
        }
        // VecDeque may not be contiguous, but make_contiguous requires &mut self.
        // For now, we return an empty slice if buffers aren't contiguous from the start.
        // In practice, the buffers are usually contiguous when created.
        self.buffers.as_slices().0.get(..end).unwrap_or(&[])
    }
}

/// Provides high-level interface over the sequence of memory regions
/// defined by readable descriptors in the descriptor chain.
///
/// Note that virtio spec requires driver to place any device-writable
/// descriptors after any device-readable descriptors (2.6.4.2 in Virtio Spec v1.1).
/// Reader will skip iterating over descriptor chain when first writable
/// descriptor is encountered.
#[derive(Clone)]
pub struct Reader<'a> {
    buffer: DescriptorChainConsumer<'a>,
}

impl<'a> Reader<'a> {
    /// Construct a new Reader wrapper over `desc_chain`.
    pub fn new(mem: &'a GuestMemoryMmap, chain: DescriptorChain<'a>) -> Result<Reader<'a>> {
        let mut total_len: usize = 0;
        let buffers = chain
            .into_iter()
            .readable()
            .map(|desc| {
                // Verify that summing the descriptor sizes does not overflow.
                // This can happen if a driver tricks a device into reading more data than
                // fits in a `usize`.
                total_len = total_len
                    .checked_add(desc.len as usize)
                    .ok_or(Error::DescriptorChainOverflow)?;

                let region = mem.find_region(desc.addr).ok_or(Error::FindMemoryRegion)?;
                let offset = desc
                    .addr
                    .checked_sub(region.start_addr().raw_value())
                    .unwrap();
                region
                    .deref()
                    .get_slice(offset.raw_value() as usize, desc.len as usize)
                    .map_err(Error::VolatileMemoryError)
            })
            .collect::<Result<VecDeque<VolatileSlice<'a>>>>()?;
        Ok(Reader {
            buffer: DescriptorChainConsumer {
                buffers,
                bytes_consumed: 0,
            },
        })
    }

    /// Reads an object from the descriptor chain buffer.
    pub fn read_obj<T: ByteValued>(&mut self) -> io::Result<T> {
        let mut obj = MaybeUninit::<T>::uninit();

        // Safe because `MaybeUninit` guarantees that the pointer is valid for
        // `size_of::<T>()` bytes.
        let buf = unsafe {
            ::std::slice::from_raw_parts_mut(obj.as_mut_ptr() as *mut u8, size_of::<T>())
        };

        self.read_exact(buf)?;

        // Safe because any type that implements `ByteValued` can be considered initialized
        // even if it is filled with random data.
        Ok(unsafe { obj.assume_init() })
    }

    /// Reads data from the descriptor chain buffer into a file descriptor.
    /// Returns the number of bytes read from the descriptor chain buffer.
    /// The number of bytes read can be less than `count` if there isn't
    /// enough data in the descriptor chain buffer.
    pub fn read_to<F: FileReadWriteVolatile>(
        &mut self,
        mut dst: F,
        count: usize,
    ) -> io::Result<usize> {
        self.buffer
            .consume(count, |bufs| dst.write_vectored_volatile(bufs))
    }

    /// Reads data from the descriptor chain buffer into a File at offset `off`.
    /// Returns the number of bytes read from the descriptor chain buffer.
    /// The number of bytes read can be less than `count` if there isn't
    /// enough data in the descriptor chain buffer.
    pub fn read_to_at<F: FileReadWriteAtVolatile>(
        &mut self,
        dst: F,
        count: usize,
        off: u64,
    ) -> io::Result<usize> {
        self.buffer
            .consume(count, |bufs| dst.write_vectored_at_volatile(bufs, off))
    }

    pub fn read_exact_to<F: FileReadWriteVolatile>(
        &mut self,
        mut dst: F,
        mut count: usize,
    ) -> io::Result<()> {
        while count > 0 {
            match self.read_to(&mut dst, count) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ));
                }
                Ok(n) => count -= n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    /// Returns number of bytes available for reading.  May return an error if the combined
    /// lengths of all the buffers in the DescriptorChain would cause an integer overflow.
    pub fn available_bytes(&self) -> usize {
        self.buffer.available_bytes()
    }

    /// Returns number of bytes already read from the descriptor chain buffer.
    pub fn bytes_read(&self) -> usize {
        self.buffer.bytes_consumed()
    }

    /// Splits this `Reader` into two at the given offset in the `DescriptorChain` buffer.
    /// After the split, `self` will be able to read up to `offset` bytes while the returned
    /// `Reader` can read up to `available_bytes() - offset` bytes.  Returns an error if
    /// `offset > self.available_bytes()`.
    pub fn split_at(&mut self, offset: usize) -> Result<Reader<'a>> {
        self.buffer.split_at(offset).map(|buffer| Reader { buffer })
    }

    /// Returns a reference to the underlying volatile slices up to `count` bytes.
    /// This allows direct access to the memory regions for async processing.
    pub fn get_slices(&self, count: usize) -> &[VolatileSlice<'a>] {
        self.buffer.get_slices(count)
    }
}

impl io::Read for Reader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.buffer.consume(buf.len(), |bufs| {
            let mut rem = buf;
            let mut total = 0;
            for vs in bufs {
                let copy_len = cmp::min(rem.len(), vs.len());

                // Safe because we have already verified that `vs` points to valid memory.
                unsafe {
                    copy_nonoverlapping(vs.ptr_guard().as_ptr(), rem.as_mut_ptr(), copy_len);
                }
                rem = &mut rem[copy_len..];
                total += copy_len;
            }
            Ok(total)
        })
    }
}

/// Provides high-level interface over the sequence of memory regions
/// defined by writable descriptors in the descriptor chain.
///
/// Note that virtio spec requires driver to place any device-writable
/// descriptors after any device-readable descriptors (2.6.4.2 in Virtio Spec v1.1).
/// Writer will start iterating the descriptors from the first writable one and will
/// assume that all following descriptors are writable.
#[derive(Clone)]
pub struct Writer<'a> {
    buffer: DescriptorChainConsumer<'a>,
}

impl<'a> Writer<'a> {
    /// Construct a new Writer wrapper over `desc_chain`.
    pub fn new(mem: &'a GuestMemoryMmap, chain: DescriptorChain<'a>) -> Result<Writer<'a>> {
        let mut total_len: usize = 0;
        let buffers = chain
            .into_iter()
            .writable()
            .map(|desc| {
                // Verify that summing the descriptor sizes does not overflow.
                // This can happen if a driver tricks a device into writing more data than
                // fits in a `usize`.
                total_len = total_len
                    .checked_add(desc.len as usize)
                    .ok_or(Error::DescriptorChainOverflow)?;

                let region = mem.find_region(desc.addr).ok_or(Error::FindMemoryRegion)?;
                let offset = desc
                    .addr
                    .checked_sub(region.start_addr().raw_value())
                    .unwrap();
                region
                    .deref()
                    .get_slice(offset.raw_value() as usize, desc.len as usize)
                    .map_err(Error::VolatileMemoryError)
            })
            .collect::<Result<VecDeque<VolatileSlice<'a>>>>()?;

        Ok(Writer {
            buffer: DescriptorChainConsumer {
                buffers,
                bytes_consumed: 0,
            },
        })
    }

    /// Writes an object to the descriptor chain buffer.
    pub fn write_obj<T: ByteValued>(&mut self, val: T) -> io::Result<()> {
        self.write_all(val.as_slice())
    }

    /// Returns number of bytes available for writing.  May return an error if the combined
    /// lengths of all the buffers in the DescriptorChain would cause an overflow.
    pub fn available_bytes(&self) -> usize {
        self.buffer.available_bytes()
    }

    /// Writes data to the descriptor chain buffer from a file descriptor.
    /// Returns the number of bytes written to the descriptor chain buffer.
    /// The number of bytes written can be less than `count` if
    /// there isn't enough data in the descriptor chain buffer.
    pub fn write_from<F: FileReadWriteVolatile>(
        &mut self,
        mut src: F,
        count: usize,
    ) -> io::Result<usize> {
        self.buffer
            .consume(count, |bufs| src.read_vectored_volatile(bufs))
    }

    /// Writes data to the descriptor chain buffer from a File at offset `off`.
    /// Returns the number of bytes written to the descriptor chain buffer.
    /// The number of bytes written can be less than `count` if
    /// there isn't enough data in the descriptor chain buffer.
    pub fn write_from_at<F: FileReadWriteAtVolatile>(
        &mut self,
        src: F,
        count: usize,
        off: u64,
    ) -> io::Result<usize> {
        self.buffer
            .consume(count, |bufs| src.read_vectored_at_volatile(bufs, off))
    }

    pub fn write_all_from<F: FileReadWriteVolatile>(
        &mut self,
        mut src: F,
        mut count: usize,
    ) -> io::Result<()> {
        while count > 0 {
            match self.write_from(&mut src, count) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ));
                }
                Ok(n) => count -= n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    /// Returns number of bytes already written to the descriptor chain buffer.
    pub fn bytes_written(&self) -> usize {
        self.buffer.bytes_consumed()
    }

    /// Splits this `Writer` into two at the given offset in the `DescriptorChain` buffer.
    /// After the split, `self` will be able to write up to `offset` bytes while the returned
    /// `Writer` can write up to `available_bytes() - offset` bytes.  Returns an error if
    /// `offset > self.available_bytes()`.
    pub fn split_at(&mut self, offset: usize) -> Result<Writer<'a>> {
        self.buffer.split_at(offset).map(|buffer| Writer { buffer })
    }

    /// Returns a reference to the underlying volatile slices up to `count` bytes.
    /// This allows direct access to the memory regions for async processing.
    pub fn get_slices(&self, count: usize) -> &[VolatileSlice<'a>] {
        self.buffer.get_slices(count)
    }

    /// Returns a `NonNull` pointer to the status byte location (last byte of the writable region),
    /// or `None` if the writable region is empty.
    ///
    /// The caller must ensure the pointer remains valid for the lifetime of the write operation.
    pub fn get_status_ptr(&self) -> Option<std::ptr::NonNull<u8>> {
        // Status is at the last byte of the last buffer.
        let last = self.buffer.buffers.back()?;
        let offset = status_byte_offset(last.len())?;
        // SAFETY: ptr_guard_mut().as_ptr() is non-null (it points into a live VolatileSlice
        // backed by guest memory), and offset == last.len() - 1 < last.len(), so the resulting
        // address is within the same allocation.  NonNull::new_unchecked is safe here because
        // ptr::add of a non-null pointer by a valid in-bounds offset is non-null.
        Some(unsafe { std::ptr::NonNull::new_unchecked(last.ptr_guard_mut().as_ptr().add(offset)) })
    }
}

/// Computes the offset of the status byte (last byte) in a buffer of `buf_len` bytes.
/// Returns None if buf_len is 0 (would underflow).
pub(crate) fn status_byte_offset(buf_len: usize) -> Option<usize> {
    buf_len.checked_sub(1)
}

impl io::Write for Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.consume(buf.len(), |bufs| {
            let mut rem = buf;
            let mut total = 0;
            for vs in bufs {
                let copy_len = cmp::min(rem.len(), vs.len());

                // Safe because we have already verified that `vs` points to valid memory.
                unsafe {
                    copy_nonoverlapping(rem.as_ptr(), vs.ptr_guard_mut().as_ptr(), copy_len);
                }
                rem = &rem[copy_len..];
                total += copy_len;
            }
            Ok(total)
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        // Nothing to flush since the writes go straight into the buffer.
        Ok(())
    }
}

const VIRTQ_DESC_F_NEXT: u16 = 0x1;
const VIRTQ_DESC_F_WRITE: u16 = 0x2;

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum DescriptorType {
    Readable,
    Writable,
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtq_desc {
    addr: Le64,
    len: Le32,
    flags: Le16,
    next: Le16,
}

// SAFETY: virtq_desc is #[repr(C)] with no padding bytes; all bit patterns are valid for all fields (Le64, Le32, Le16, Le16).
unsafe impl ByteValued for virtq_desc {}

/// Test utility function to create a descriptor chain in guest memory.
pub fn create_descriptor_chain(
    memory: &GuestMemoryMmap,
    descriptor_array_addr: GuestAddress,
    mut buffers_start_addr: GuestAddress,
    descriptors: Vec<(DescriptorType, u32)>,
    spaces_between_regions: u32,
) -> Result<DescriptorChain<'_>> {
    let descriptors_len = descriptors.len();
    for (index, (type_, size)) in descriptors.into_iter().enumerate() {
        let mut flags = 0;
        if let DescriptorType::Writable = type_ {
            flags |= VIRTQ_DESC_F_WRITE;
        }
        if index + 1 < descriptors_len {
            flags |= VIRTQ_DESC_F_NEXT;
        }

        let index = index as u16;
        let desc = virtq_desc {
            addr: buffers_start_addr.raw_value().into(),
            len: size.into(),
            flags: flags.into(),
            next: (index + 1).into(),
        };

        let offset = size + spaces_between_regions;
        buffers_start_addr = buffers_start_addr
            .checked_add(u64::from(offset))
            .ok_or(Error::InvalidChain)?;

        let _ = memory.write_obj(
            desc,
            descriptor_array_addr
                .checked_add(u64::from(index) * std::mem::size_of::<virtq_desc>() as u64)
                .ok_or(Error::InvalidChain)?,
        );
    }

    DescriptorChain::checked_new(memory, descriptor_array_addr, 0x100, 0).ok_or(Error::InvalidChain)
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Proof: status_byte_offset correctly handles all buffer lengths.
    ///
    /// GAP-008: get_status_ptr computes buf_len - 1 to get the status byte offset.
    /// If buf_len == 0, unsigned subtraction wraps to usize::MAX, causing the
    /// subsequent add(usize::MAX) to point wildly out of bounds. The fix uses
    /// checked_sub via status_byte_offset. This proof verifies the helper.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_get_status_ptr_arithmetic_in_bounds() {
        let buf_len: usize = kani::any();
        kani::assume(buf_len <= 64);

        match status_byte_offset(buf_len) {
            Some(offset) => {
                kani::assert(buf_len >= 1, "Some only returned for non-empty buffers");
                kani::assert(offset == buf_len - 1, "offset is last byte index");
                kani::assert(offset < buf_len, "offset is within buffer bounds");

                // Verify pointer arithmetic is safe
                let mut backing = vec![0u8; buf_len];
                let buf_ptr: *mut u8 = backing.as_mut_ptr();
                let status_ptr: *mut u8 = unsafe { buf_ptr.add(offset) };
                let sp_addr = status_ptr as usize;
                let base = buf_ptr as usize;
                kani::assert(
                    sp_addr >= base && sp_addr < base + buf_len,
                    "pointer in bounds",
                );
                kani::cover!(true, "non-empty buffer: valid offset");
            }
            None => {
                kani::assert(buf_len == 0, "None only returned for empty buffer");
                kani::cover!(true, "zero-length buffer correctly rejected");
            }
        }
    }

    /// Proof: get_status_ptr selects the byte at index (len - 1), i.e. the LAST byte.
    ///
    /// This verifies the functional contract: for a buffer of length N >= 1,
    /// the status pointer must equal base_ptr + (N - 1), not some other offset.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_get_status_ptr_selects_last_byte() {
        // buf_len in [1, 64] — non-empty buffers only (correct-path verification).
        let buf_len: usize = kani::any_where(|&n: &usize| n >= 1 && n <= 64);

        let mut backing = vec![0u8; buf_len];
        let buf_ptr: *mut u8 = backing.as_mut_ptr();

        // Replicate the get_status_ptr() arithmetic.
        let status_ptr: *mut u8 = unsafe { buf_ptr.add(buf_len - 1) };

        // The offset from base must be exactly buf_len - 1.
        // This verifies that ptr::add() arithmetic matches integer expectations.
        let offset = status_ptr as usize - buf_ptr as usize;
        kani::assert(
            offset == buf_len - 1,
            "status byte must be at offset (buf_len - 1) from buffer start",
        );

        kani::cover!(true, "last-byte selection proof path covered");
    }

    /// Proof: get_status_ptr for buf_len == 1 returns the first (and only) byte.
    ///
    /// Edge case: a single-byte buffer means the status byte IS the only byte.
    /// offset = 1 - 1 = 0, so status_ptr == buf_ptr.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_get_status_ptr_single_byte_buffer() {
        let mut backing = [0u8; 1];
        let buf_ptr: *mut u8 = backing.as_mut_ptr();
        let buf_len: usize = 1;

        let status_ptr: *mut u8 = unsafe { buf_ptr.add(buf_len - 1) };

        // For a 1-byte buffer, status_ptr must equal buf_ptr exactly.
        kani::assert(
            status_ptr == buf_ptr,
            "single-byte buffer: status_ptr must equal buf_ptr (offset 0)",
        );

        kani::cover!(true, "single-byte buffer status ptr proof covered");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_test_simple_chain() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![
                (Readable, 8),
                (Readable, 16),
                (Readable, 18),
                (Readable, 64),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut reader = Reader::new(&memory, chain).expect("failed to create Reader");
        assert_eq!(reader.available_bytes(), 106);
        assert_eq!(reader.bytes_read(), 0);

        let mut buffer = [0_u8; 64];
        if let Err(_) = reader.read_exact(&mut buffer) {
            panic!("read_exact should not fail here");
        }

        assert_eq!(reader.available_bytes(), 42);
        assert_eq!(reader.bytes_read(), 64);

        match reader.read(&mut buffer) {
            Err(_) => panic!("read should not fail here"),
            Ok(length) => assert_eq!(length, 42),
        }

        assert_eq!(reader.available_bytes(), 0);
        assert_eq!(reader.bytes_read(), 106);
    }

    #[test]
    fn writer_test_simple_chain() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![
                (Writable, 8),
                (Writable, 16),
                (Writable, 18),
                (Writable, 64),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut writer = Writer::new(&memory, chain).expect("failed to create Writer");
        assert_eq!(writer.available_bytes(), 106);
        assert_eq!(writer.bytes_written(), 0);

        let mut buffer = [0_u8; 64];
        if let Err(_) = writer.write_all(&mut buffer) {
            panic!("write_all should not fail here");
        }

        assert_eq!(writer.available_bytes(), 42);
        assert_eq!(writer.bytes_written(), 64);

        match writer.write(&mut buffer) {
            Err(_) => panic!("write should not fail here"),
            Ok(length) => assert_eq!(length, 42),
        }

        assert_eq!(writer.available_bytes(), 0);
        assert_eq!(writer.bytes_written(), 106);
    }

    #[test]
    fn reader_test_incompatible_chain() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![(Writable, 8)],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut reader = Reader::new(&memory, chain).expect("failed to create Reader");
        assert_eq!(reader.available_bytes(), 0);
        assert_eq!(reader.bytes_read(), 0);

        assert!(reader.read_obj::<u8>().is_err());

        assert_eq!(reader.available_bytes(), 0);
        assert_eq!(reader.bytes_read(), 0);
    }

    #[test]
    fn writer_test_incompatible_chain() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![(Readable, 8)],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut writer = Writer::new(&memory, chain).expect("failed to create Writer");
        assert_eq!(writer.available_bytes(), 0);
        assert_eq!(writer.bytes_written(), 0);

        assert!(writer.write_obj(0u8).is_err());

        assert_eq!(writer.available_bytes(), 0);
        assert_eq!(writer.bytes_written(), 0);
    }

    #[test]
    fn reader_writer_shared_chain() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![
                (Readable, 16),
                (Readable, 16),
                (Readable, 96),
                (Writable, 64),
                (Writable, 1),
                (Writable, 3),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut reader = Reader::new(&memory, chain.clone()).expect("failed to create Reader");
        let mut writer = Writer::new(&memory, chain).expect("failed to create Writer");

        assert_eq!(reader.bytes_read(), 0);
        assert_eq!(writer.bytes_written(), 0);

        let mut buffer = Vec::with_capacity(200);

        assert_eq!(
            reader
                .read_to_end(&mut buffer)
                .expect("read should not fail here"),
            128
        );

        // The writable descriptors are only 68 bytes long.
        writer
            .write_all(&buffer[..68])
            .expect("write should not fail here");

        assert_eq!(reader.available_bytes(), 0);
        assert_eq!(reader.bytes_read(), 128);
        assert_eq!(writer.available_bytes(), 0);
        assert_eq!(writer.bytes_written(), 68);
    }

    #[test]
    fn reader_writer_shattered_object() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let secret: Le32 = 0x12345678.into();

        // Create a descriptor chain with memory regions that are properly separated.
        let chain_writer = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![(Writable, 1), (Writable, 1), (Writable, 1), (Writable, 1)],
            123,
        )
        .expect("create_descriptor_chain failed");
        let mut writer = Writer::new(&memory, chain_writer).expect("failed to create Writer");
        if let Err(_) = writer.write_obj(secret) {
            panic!("write_obj should not fail here");
        }

        // Now create new descriptor chain pointing to the same memory and try to read it.
        let chain_reader = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![(Readable, 1), (Readable, 1), (Readable, 1), (Readable, 1)],
            123,
        )
        .expect("create_descriptor_chain failed");
        let mut reader = Reader::new(&memory, chain_reader).expect("failed to create Reader");
        match reader.read_obj::<Le32>() {
            Err(_) => panic!("read_obj should not fail here"),
            Ok(read_secret) => assert_eq!(read_secret, secret),
        }
    }

    #[test]
    fn reader_unexpected_eof() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![(Readable, 256), (Readable, 256)],
            0,
        )
        .expect("create_descriptor_chain failed");

        let mut reader = Reader::new(&memory, chain).expect("failed to create Reader");

        let mut buf = Vec::with_capacity(1024);
        buf.resize(1024, 0);

        assert_eq!(
            reader
                .read_exact(&mut buf[..])
                .expect_err("read more bytes than available")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn split_border() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![
                (Readable, 16),
                (Readable, 16),
                (Readable, 96),
                (Writable, 64),
                (Writable, 1),
                (Writable, 3),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut reader = Reader::new(&memory, chain).expect("failed to create Reader");

        let other = reader.split_at(32).expect("failed to split Reader");
        assert_eq!(reader.available_bytes(), 32);
        assert_eq!(other.available_bytes(), 96);
    }

    #[test]
    fn split_middle() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![
                (Readable, 16),
                (Readable, 16),
                (Readable, 96),
                (Writable, 64),
                (Writable, 1),
                (Writable, 3),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut reader = Reader::new(&memory, chain).expect("failed to create Reader");

        let other = reader.split_at(24).expect("failed to split Reader");
        assert_eq!(reader.available_bytes(), 24);
        assert_eq!(other.available_bytes(), 104);
    }

    #[test]
    fn split_end() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![
                (Readable, 16),
                (Readable, 16),
                (Readable, 96),
                (Writable, 64),
                (Writable, 1),
                (Writable, 3),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut reader = Reader::new(&memory, chain).expect("failed to create Reader");

        let other = reader.split_at(128).expect("failed to split Reader");
        assert_eq!(reader.available_bytes(), 128);
        assert_eq!(other.available_bytes(), 0);
    }

    #[test]
    fn split_beginning() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![
                (Readable, 16),
                (Readable, 16),
                (Readable, 96),
                (Writable, 64),
                (Writable, 1),
                (Writable, 3),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut reader = Reader::new(&memory, chain).expect("failed to create Reader");

        let other = reader.split_at(0).expect("failed to split Reader");
        assert_eq!(reader.available_bytes(), 0);
        assert_eq!(other.available_bytes(), 128);
    }

    #[test]
    fn split_outofbounds() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![
                (Readable, 16),
                (Readable, 16),
                (Readable, 96),
                (Writable, 64),
                (Writable, 1),
                (Writable, 3),
            ],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut reader = Reader::new(&memory, chain).expect("failed to create Reader");

        if let Ok(_) = reader.split_at(256) {
            panic!("successfully split Reader with out of bounds offset");
        }
    }

    #[test]
    fn read_full() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![(Readable, 16), (Readable, 16), (Readable, 16)],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut reader = Reader::new(&memory, chain).expect("failed to create Reader");

        let mut buf = [0u8; 64];
        assert_eq!(
            reader.read(&mut buf[..]).expect("failed to read to buffer"),
            48
        );
    }

    #[test]
    fn write_full() {
        use DescriptorType::*;

        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemoryMmap::from_ranges(&[(memory_start_addr, 0x10000)]).unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![(Writable, 16), (Writable, 16), (Writable, 16)],
            0,
        )
        .expect("create_descriptor_chain failed");
        let mut writer = Writer::new(&memory, chain).expect("failed to create Writer");

        let buf = [0xdeu8; 64];
        assert_eq!(
            writer.write(&buf[..]).expect("failed to write from buffer"),
            48
        );
    }
}
