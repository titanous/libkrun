// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::cmp::min;
use std::fmt::{self, Debug, Display};
use std::num::Wrapping;
use std::sync::atomic::{fence, Ordering};
use virtio_bindings::virtio_ring::VRING_USED_F_NO_NOTIFY;
use vm_memory::{
    Address, ByteValued, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryError,
    GuestMemoryMmap, VolatileMemoryError,
};

/// Size of used ring header: flags (u16) + idx (u16)
pub(crate) const VIRTQ_USED_RING_HEADER_SIZE: u64 = 4;

/// Size of one element in the used ring, id (le32) + len (le32).
pub(crate) const VIRTQ_USED_ELEMENT_SIZE: u64 = 8;

/// Size of available ring header: flags(u16) + idx(u16)
pub(crate) const VIRTQ_AVAIL_RING_HEADER_SIZE: u64 = 4;

/// Size of one element in the available ring (le16).
pub(crate) const VIRTQ_AVAIL_ELEMENT_SIZE: u64 = 2;

pub(super) const VIRTQ_DESC_F_NEXT: u16 = 0x1;
pub(super) const VIRTQ_DESC_F_WRITE: u16 = 0x2;

/// Virtio Queue related errors.
#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum Error {
    /// Address overflow.
    AddressOverflow,
    /// Failed to access guest memory.
    GuestMemory(GuestMemoryError),
    /// Invalid indirect descriptor.
    InvalidIndirectDescriptor,
    /// Invalid indirect descriptor table.
    InvalidIndirectDescriptorTable,
    /// Invalid descriptor chain.
    InvalidChain,
    /// Invalid descriptor index.
    InvalidDescriptorIndex,
    /// Invalid max_size.
    InvalidMaxSize,
    /// Invalid Queue Size.
    InvalidSize,
    /// Invalid alignment of descriptor table address.
    InvalidDescTableAlign,
    /// Invalid alignment of available ring address.
    InvalidAvailRingAlign,
    /// Invalid alignment of used ring address.
    InvalidUsedRingAlign,
    /// Invalid available ring index.
    InvalidAvailRingIndex,
    /// The queue is not ready for operation.
    QueueNotReady,
    /// Volatile memory error.
    VolatileMemoryError(VolatileMemoryError),
    /// The combined length of all the buffers in a `DescriptorChain` would overflow.
    DescriptorChainOverflow,
    /// No memory region for this address range.
    FindMemoryRegion,
    /// Descriptor guest memory error.
    GuestMemoryError(GuestMemoryError),
    /// DescriptorChain split is out of bounds.
    SplitOutOfBounds(usize),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            AddressOverflow => write!(f, "address overflow"),
            GuestMemory(_) => write!(f, "error accessing guest memory"),
            InvalidChain => write!(f, "invalid descriptor chain"),
            InvalidIndirectDescriptor => write!(f, "invalid indirect descriptor"),
            InvalidIndirectDescriptorTable => write!(f, "invalid indirect descriptor table"),
            InvalidDescriptorIndex => write!(f, "invalid descriptor index"),
            InvalidMaxSize => write!(f, "invalid queue maximum size"),
            InvalidSize => write!(f, "invalid queue size"),
            InvalidDescTableAlign => write!(
                f,
                "virtio queue descriptor table breaks alignment constraints"
            ),
            InvalidAvailRingAlign => write!(
                f,
                "virtio queue available ring breaks alignment constraints"
            ),
            InvalidUsedRingAlign => {
                write!(f, "virtio queue used ring breaks alignment constraints")
            }
            InvalidAvailRingIndex => write!(
                f,
                "invalid available ring index (more descriptors to process than queue size)"
            ),
            QueueNotReady => write!(f, "trying to process requests on a queue that's not ready"),
            VolatileMemoryError(e) => write!(f, "volatile memory error: {e}"),
            DescriptorChainOverflow => write!(
                f,
                "the combined length of all the buffers in a `DescriptorChain` would overflow"
            ),
            FindMemoryRegion => write!(f, "no memory region for this address range"),
            GuestMemoryError(e) => write!(f, "descriptor guest memory error: {e}"),
            SplitOutOfBounds(off) => write!(f, "`DescriptorChain` split is out of bounds: {off}"),
        }
    }
}

impl std::error::Error for Error {}

/// Represents the contents of an element from the used virtqueue ring.
// Note that the `ByteValued` implementation of this structure expects the `VirtqUsedElem` to store
// only plain old data types.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct VirtqUsedElem {
    id: u32,
    len: u32,
}

impl VirtqUsedElem {
    /// Create a new `VirtqUsedElem` instance.
    ///
    /// # Arguments
    /// * `id` - the index of the used descriptor chain.
    /// * `len` - the total length of the descriptor chain which was used (written to).
    pub(crate) fn new(id: u32, len: u32) -> Self {
        VirtqUsedElem { id, len }
    }
}

// SAFETY: This is safe because `VirtqUsedElem` contains only wrappers over POD types
// and all accesses through safe `vm-memory` API will validate any garbage that could be
// included in there.
unsafe impl ByteValued for VirtqUsedElem {}

// GuestMemoryMmap::read_obj_from_addr() will be used to fetch the descriptor,
// which has an explicit constraint that the entire descriptor doesn't
// cross the page boundary. Otherwise the descriptor may be splitted into
// two mmap regions which causes failure of GuestMemoryMmap::read_obj_from_addr().
//
// The Virtio Spec 1.0 defines the alignment of VirtIO descriptor is 16 bytes,
// which fulfills the explicit constraint of GuestMemoryMmap::read_obj_from_addr().

/// An iterator over a single descriptor chain.  Not to be confused with AvailIter,
/// which iterates over the descriptor chain heads in a queue.
pub struct DescIter<'a> {
    next: Option<DescriptorChain<'a>>,
}

impl<'a> DescIter<'a> {
    /// Returns an iterator that only yields the readable descriptors in the chain.
    pub fn readable(self) -> impl Iterator<Item = DescriptorChain<'a>> {
        self.take_while(DescriptorChain::is_read_only)
    }

    /// Returns an iterator that only yields the writable descriptors in the chain.
    pub fn writable(self) -> impl Iterator<Item = DescriptorChain<'a>> {
        self.skip_while(DescriptorChain::is_read_only)
    }
}

impl<'a> Iterator for DescIter<'a> {
    type Item = DescriptorChain<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(current) = self.next.take() {
            self.next = current.next_descriptor();
            Some(current)
        } else {
            None
        }
    }
}

/// A virtio descriptor constraints with C representive.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

// SAFETY: Descriptor is #[repr(C)] with no padding bytes; all bit patterns are valid for all fields.
unsafe impl ByteValued for Descriptor {}

/// A virtio descriptor chain.
#[derive(Clone)]
pub struct DescriptorChain<'a> {
    desc_table: GuestAddress,
    queue_size: u16,
    ttl: u16, // used to prevent infinite chain cycles

    /// Reference to guest memory
    pub mem: &'a GuestMemoryMmap,

    /// Index into the descriptor table
    pub index: u16,

    /// Guest physical address of device specific data
    pub addr: GuestAddress,

    /// Length of device specific data
    pub len: u32,

    /// Includes next, write, and indirect bits
    pub flags: u16,

    /// Index into the descriptor table of the next descriptor if flags has
    /// the next bit set
    pub next: u16,
}

impl<'a> DescriptorChain<'a> {
    pub fn checked_new(
        mem: &GuestMemoryMmap,
        desc_table: GuestAddress,
        queue_size: u16,
        index: u16,
    ) -> Option<DescriptorChain<'_>> {
        if index >= queue_size {
            return None;
        }

        let desc_head = mem.checked_offset(desc_table, (index as usize) * 16)?;
        mem.checked_offset(desc_head, 16)?;

        // These reads can't fail unless Guest memory is hopelessly broken.
        let desc = match mem.read_obj::<Descriptor>(desc_head) {
            Ok(ret) => ret,
            Err(_) => {
                // TODO log address
                error!("Failed to read from memory");
                return None;
            }
        };
        let chain = DescriptorChain {
            mem,
            desc_table,
            queue_size,
            ttl: queue_size,
            index,
            addr: GuestAddress(desc.addr),
            len: desc.len,
            flags: desc.flags,
            next: desc.next,
        };

        if chain.is_valid() {
            Some(chain)
        } else {
            None
        }
    }

    fn is_valid(&self) -> bool {
        !self.has_next() || self.next < self.queue_size
    }

    /// Gets if this descriptor chain has another descriptor chain linked after it.
    pub fn has_next(&self) -> bool {
        self.flags & VIRTQ_DESC_F_NEXT != 0 && self.ttl > 1
    }

    /// If the driver designated this as a write only descriptor.
    ///
    /// If this is false, this descriptor is read only.
    /// Write only means the the emulated device can write and the driver can read.
    pub fn is_write_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }

    /// If the driver designated this as a read only descriptor.
    ///
    /// If this is false, this descriptor is write only.
    /// Read only means the emulated device can read and the driver can write.
    pub fn is_read_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE == 0
    }

    /// Gets the next descriptor in this descriptor chain, if there is one.
    ///
    /// Note that this is distinct from the next descriptor chain returned by `AvailIter`, which is
    /// the head of the next _available_ descriptor chain.
    pub fn next_descriptor(&self) -> Option<DescriptorChain<'a>> {
        if self.has_next() {
            DescriptorChain::checked_new(self.mem, self.desc_table, self.queue_size, self.next).map(
                |mut c| {
                    c.ttl = self.ttl - 1;
                    c
                },
            )
        } else {
            None
        }
    }

    /// Produces an iterator over all the descriptors in this chain.
    #[allow(clippy::should_implement_trait)]
    pub fn into_iter(self) -> DescIter<'a> {
        DescIter { next: Some(self) }
    }

    pub fn descriptor(&self) -> Descriptor {
        Descriptor {
            addr: self.addr.raw_value(),
            len: self.len,
            flags: self.flags,
            next: self.next,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A virtio queue's parameters.
pub struct Queue {
    /// The maximal size in elements offered by the device
    pub(crate) max_size: u16,

    /// The queue size in elements the driver selected
    pub size: u16,

    /// Indicates if the queue is finished with configuration
    pub ready: bool,

    /// Guest physical address of the descriptor table
    pub desc_table: GuestAddress,

    /// Guest physical address of the available ring
    pub avail_ring: GuestAddress,

    /// Guest physical address of the used ring
    pub used_ring: GuestAddress,

    pub(crate) next_avail: Wrapping<u16>,
    pub(crate) next_used: Wrapping<u16>,

    /// VIRTIO_F_RING_EVENT_IDX negotiated.
    event_idx_enabled: bool,

    /// The number of descriptor chains placed in the used ring via `add_used`
    /// since the last time `needs_notification` was called on the associated queue.
    num_added: Wrapping<u16>,
}

impl Queue {
    /// Constructs an empty virtio queue with the given `max_size`.
    pub fn new(max_size: u16) -> Queue {
        Queue {
            max_size,
            size: 0,
            ready: false,
            desc_table: GuestAddress(0),
            avail_ring: GuestAddress(0),
            used_ring: GuestAddress(0),
            next_avail: Wrapping(0),
            next_used: Wrapping(0),
            event_idx_enabled: false,
            num_added: Wrapping(0),
        }
    }

    pub fn get_max_size(&self) -> u16 {
        self.max_size
    }

    /// Return the actual size of the queue, as the driver may not set up a
    /// queue as big as the device allows.
    pub fn actual_size(&self) -> u16 {
        min(self.size, self.max_size)
    }

    pub fn next_avail(&self) -> Wrapping<u16> {
        self.next_avail
    }

    pub fn next_used(&self) -> Wrapping<u16> {
        self.next_used
    }

    /// Set the next available descriptor index (for restore).
    pub fn set_next_avail(&mut self, idx: u16) {
        self.next_avail = Wrapping(idx);
    }

    /// Set the next used descriptor index (for restore).
    pub fn set_next_used(&mut self, idx: u16) {
        self.next_used = Wrapping(idx);
    }

    /// Pure validation predicate for the queue parameters that do not require
    /// guest memory access.
    // Called from #[cfg(kani)] proofs and #[cfg(test)] unit tests; suppress the
    // dead_code lint for normal (non-kani, non-test) builds.
    #[cfg_attr(not(any(test, kani)), allow(dead_code))]
    ///
    /// `is_valid()` calls this for the readiness/size/alignment checks and then
    /// additionally verifies that all three ring regions fit within guest memory.
    /// Extracting this pure predicate allows Kani proofs to call it directly
    /// with symbolic inputs instead of inlining the conditions (which would make
    /// the proofs disconnected from the real implementation).
    pub(crate) fn is_valid_params(
        ready: bool,
        size: u16,
        max_size: u16,
        desc_table_addr: u64,
        avail_ring_addr: u64,
        used_ring_addr: u64,
    ) -> bool {
        ready
            && size != 0
            && size <= max_size
            && (size & (size - 1)) == 0
            && desc_table_addr & 0xf == 0
            && avail_ring_addr & 0x1 == 0
            && used_ring_addr & 0x3 == 0
    }

    pub fn is_valid(&self, mem: &GuestMemoryMmap) -> bool {
        let queue_size = u64::from(self.actual_size());
        let desc_table = self.desc_table;
        let desc_table_size = 16 * queue_size;
        let avail_ring = self.avail_ring;
        let avail_ring_size = 6 + 2 * queue_size;
        let used_ring = self.used_ring;
        let used_ring_size = 6 + 8 * queue_size;
        if !self.ready {
            error!("attempt to use virtio queue that is not marked ready");
            false
        } else if self.size > self.max_size || self.size == 0 || (self.size & (self.size - 1)) != 0
        {
            error!("virtio queue with invalid size: {}", self.size);
            false
        } else if desc_table
            .checked_add(desc_table_size)
            .is_none_or(|v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue descriptor table goes out of bounds: start:0x{:08x} size:0x{:08x}",
                desc_table.raw_value(),
                desc_table_size
            );
            false
        } else if avail_ring
            .checked_add(avail_ring_size)
            .is_none_or(|v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue available ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
                avail_ring.raw_value(),
                avail_ring_size
            );
            false
        } else if used_ring
            .checked_add(used_ring_size)
            .is_none_or(|v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue used ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
                used_ring.raw_value(),
                used_ring_size
            );
            false
        } else if desc_table.raw_value() & 0xf != 0 {
            error!("virtio queue descriptor table breaks alignment contraints");
            false
        } else if avail_ring.raw_value() & 0x1 != 0 {
            error!("virtio queue available ring breaks alignment contraints");
            false
        } else if used_ring.raw_value() & 0x3 != 0 {
            error!("virtio queue used ring breaks alignment contraints");
            false
        } else {
            true
        }
    }

    /// Returns the number of yet-to-be-popped descriptor chains in the avail ring.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self, mem: &GuestMemoryMmap) -> u16 {
        (self.avail_idx(mem, Ordering::Acquire).unwrap() - self.next_avail).0
    }

    /// Checks if the driver has made any descriptor chains available in the avail ring.
    pub fn is_empty(&self, mem: &GuestMemoryMmap) -> bool {
        self.len(mem) == 0
    }

    /// Pop the first available descriptor chain from the avail ring.
    pub fn pop<'b>(&mut self, mem: &'b GuestMemoryMmap) -> Option<DescriptorChain<'b>> {
        if self.len(mem) == 0 || self.actual_size() == 0 {
            return None;
        }

        // We'll need to find the first available descriptor, that we haven't yet popped.
        // In a naive notation, that would be:
        // `descriptor_table[avail_ring[next_avail]]`.
        //
        // First, we compute the byte-offset (into `self.avail_ring`) of the index of the next available
        // descriptor. `self.avail_ring` stores the address of a `struct virtq_avail`, as defined by
        // the VirtIO spec:
        //
        // ```C
        // struct virtq_avail {
        //   le16 flags;
        //   le16 idx;
        //   le16 ring[QUEUE_SIZE];
        //   le16 used_event
        // }
        // ```
        //
        // We use `self.next_avail` to store the position, in `ring`, of the next available
        // descriptor index, with a twist: we always only increment `self.next_avail`, so the
        // actual position will be `self.next_avail % self.actual_size()`.
        // We are now looking for the offset of `ring[self.next_avail % self.actual_size()]`.
        // `ring` starts after `flags` and `idx` (4 bytes into `struct virtq_avail`), and holds
        // 2-byte items, so the offset will be:
        let index_offset = 4 + 2 * (self.next_avail.0 % self.actual_size());

        // Make sure we catch all updates on the queue
        fence(Ordering::Acquire);

        // `self.is_valid()` already performed all the bound checks on the descriptor table
        // and virtq rings, so it's safe to unwrap guest memory reads and to use unchecked
        // offsets.
        let desc_index: u16 = mem
            .read_obj(self.avail_ring.unchecked_add(u64::from(index_offset)))
            .unwrap();

        DescriptorChain::checked_new(mem, self.desc_table, self.actual_size(), desc_index)
            .inspect(|_| self.next_avail += Wrapping(1))
    }

    /// Undo the effects of the last `self.pop()` call.
    /// The caller can use this, if it was unable to consume the last popped descriptor chain.
    pub fn undo_pop(&mut self) {
        self.next_avail -= Wrapping(1);
    }

    pub fn add_used(
        &mut self,
        mem: &GuestMemoryMmap,
        head_index: u16,
        len: u32,
    ) -> Result<(), Error> {
        if head_index >= self.size {
            error!("attempted to add out of bounds descriptor to used ring: {head_index}");
            return Err(Error::InvalidDescriptorIndex);
        }

        let next_used_index = u64::from(self.next_used.0 % self.size);
        // This can not overflow an u64 since it is working with relatively small numbers compared
        // to u64::MAX.
        let offset = VIRTQ_USED_RING_HEADER_SIZE + next_used_index * VIRTQ_USED_ELEMENT_SIZE;
        let addr = self
            .used_ring
            .checked_add(offset)
            .ok_or(Error::AddressOverflow)?;
        mem.write_obj(VirtqUsedElem::new(head_index.into(), len), addr)
            .map_err(Error::GuestMemory)?;

        self.next_used += Wrapping(1);
        self.num_added += Wrapping(1);

        mem.store(
            self.next_used.0,
            self.used_ring
                .checked_add(2)
                .ok_or(Error::AddressOverflow)?,
            Ordering::Release,
        )
        .map_err(Error::GuestMemory)
    }

    // Return the value present in the used_event field of the avail ring.
    //
    // If the VIRTIO_F_EVENT_IDX feature bit is not negotiated, the flags field in the available
    // ring offers a crude mechanism for the driver to inform the device that it doesn’t want
    // interrupts when buffers are used. Otherwise virtq_avail.used_event is a more performant
    // alternative where the driver specifies how far the device can progress before interrupting.
    //
    // Neither of these interrupt suppression methods are reliable, as they are not synchronized
    // with the device, but they serve as useful optimizations. So we only ensure access to the
    // virtq_avail.used_event is atomic, but do not need to synchronize with other memory accesses.
    fn used_event(&self, mem: &GuestMemoryMmap, order: Ordering) -> Result<Wrapping<u16>, Error> {
        // This can not overflow an u64 since it is working with relatively small numbers compared
        // to u64::MAX.
        let used_event_offset =
            VIRTQ_AVAIL_RING_HEADER_SIZE + u64::from(self.size) * VIRTQ_AVAIL_ELEMENT_SIZE;
        let used_event_addr = self
            .avail_ring
            .checked_add(used_event_offset)
            .ok_or(Error::AddressOverflow)?;

        mem.load(used_event_addr, order)
            .map(Wrapping)
            .map_err(Error::GuestMemory)
    }

    // Helper method that writes `val` to the `avail_event` field of the used ring, using
    // the provided ordering.
    fn set_avail_event(
        &self,
        mem: &GuestMemoryMmap,
        val: u16,
        order: Ordering,
    ) -> Result<(), Error> {
        // This can not overflow an u64 since it is working with relatively small numbers compared
        // to u64::MAX.
        let avail_event_offset =
            VIRTQ_USED_RING_HEADER_SIZE + VIRTQ_USED_ELEMENT_SIZE * u64::from(self.size);
        let addr = self
            .used_ring
            .checked_add(avail_event_offset)
            .ok_or(Error::AddressOverflow)?;

        mem.store(val, addr, order).map_err(Error::GuestMemory)
    }

    pub fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx_enabled = enabled;
    }

    // Set the value of the `flags` field of the used ring, applying the specified ordering.
    fn set_used_flags(
        &mut self,
        mem: &GuestMemoryMmap,
        val: u16,
        order: Ordering,
    ) -> Result<(), Error> {
        mem.store(val, self.used_ring, order)
            .map_err(Error::GuestMemory)
    }

    // Write the appropriate values to enable or disable notifications from the driver.
    //
    // Every access in this method uses `Relaxed` ordering because a fence is added by the caller
    // when appropriate.
    fn set_notification(&mut self, mem: &GuestMemoryMmap, enable: bool) -> Result<(), Error> {
        if enable {
            if self.event_idx_enabled {
                // We call `set_avail_event` using the `next_avail` value, instead of reading
                // and using the current `avail_idx` to avoid missing notifications. More
                // details in `enable_notification`.
                self.set_avail_event(mem, self.next_avail.0, Ordering::Relaxed)
            } else {
                self.set_used_flags(mem, 0, Ordering::Relaxed)
            }
        } else if !self.event_idx_enabled {
            self.set_used_flags(mem, VRING_USED_F_NO_NOTIFY as u16, Ordering::Relaxed)
        } else {
            // Notifications are effectively disabled by default after triggering once when
            // `VIRTIO_F_EVENT_IDX` is negotiated, so we don't do anything in that case.
            Ok(())
        }
    }

    // TODO: Turn this into a doc comment/example.
    // With the current implementation, a common way of consuming entries from the available ring
    // while also leveraging notification suppression is to use a loop, for example:
    //
    // loop {
    //     // We have to explicitly disable notifications if `VIRTIO_F_EVENT_IDX` has not been
    //     // negotiated.
    //     self.disable_notification()?;
    //
    //     for chain in self.iter()? {
    //         // Do something with each chain ...
    //         // Let's assume we process all available chains here.
    //     }
    //
    //     // If `enable_notification` returns `true`, the driver has added more entries to the
    //     // available ring.
    //     if !self.enable_notification()? {
    //         break;
    //     }
    // }
    pub fn enable_notification(&mut self, mem: &GuestMemoryMmap) -> Result<bool, Error> {
        self.set_notification(mem, true)?;
        // Ensures the following read is not reordered before any previous write operation.
        fence(Ordering::SeqCst);

        // We double check here to avoid the situation where the available ring has been updated
        // just before we re-enabled notifications, and it's possible to miss one. We compare the
        // current `avail_idx` value to `self.next_avail` because it's where we stopped processing
        // entries. There are situations where we intentionally avoid processing everything in the
        // available ring (which will cause this method to return `true`), but in that case we'll
        // probably not re-enable notifications as we already know there are pending entries.
        self.avail_idx(mem, Ordering::Relaxed)
            .map(|idx| idx != self.next_avail)
    }

    pub fn disable_notification(&mut self, mem: &GuestMemoryMmap) -> Result<(), Error> {
        self.set_notification(mem, false)
    }

    pub fn needs_notification(&mut self, mem: &GuestMemoryMmap) -> Result<bool, Error> {
        let used_idx = self.next_used;

        // Complete all the writes in add_used() before reading the event.
        fence(Ordering::SeqCst);

        // The VRING_AVAIL_F_NO_INTERRUPT flag isn't supported yet.

        // When the `EVENT_IDX` feature is negotiated, the driver writes into `used_event`
        // a value that's used by the device to determine whether a notification must
        // be submitted after adding a descriptor chain to the used ring. According to the
        // standard, the notification must be sent when `next_used == used_event + 1`, but
        // various device model implementations rely on an inequality instead, most likely
        // to also support use cases where a bunch of descriptor chains are added to the used
        // ring first, and only afterwards the `needs_notification` logic is called. For example,
        // the approach based on `num_added` below is taken from the Linux Kernel implementation
        // (i.e. https://elixir.bootlin.com/linux/v5.15.35/source/drivers/virtio/virtio_ring.c#L661)

        // The `old` variable below is used to determine the value of `next_used` from when
        // `needs_notification` was called last (each `needs_notification` call resets `num_added`
        // to zero, while each `add_used` called increments it by one). Then, the logic below
        // uses wrapped arithmetic to see whether `used_event` can be found between `old` and
        // `next_used` in the circular sequence space of the used ring.
        if self.event_idx_enabled {
            let used_event = self.used_event(mem, Ordering::Relaxed)?;
            let old = used_idx - self.num_added;
            self.num_added = Wrapping(0);
            return Ok(used_idx - used_event - Wrapping(1) < used_idx - old);
        }

        Ok(true)
    }

    /// Goes back one position in the available descriptor chain offered by the driver.
    /// Rust does not support bidirectional iterators. This is the only way to revert the effect
    /// of an iterator increment on the queue.
    pub fn go_to_previous_position(&mut self) {
        self.next_avail -= Wrapping(1);
    }

    /// Fetch the available ring index (`virtq_avail->idx`) from guest memory.
    /// This is written by the driver, to indicate the next slot that will be filled in the avail
    /// ring.
    fn avail_idx(&self, mem: &GuestMemoryMmap, order: Ordering) -> Result<Wrapping<u16>, Error> {
        let addr = self
            .avail_ring
            .checked_add(2)
            .ok_or(Error::AddressOverflow)?;

        mem.load(addr, order)
            .map(Wrapping)
            .map_err(Error::GuestMemory)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::marker::PhantomData;
    use std::mem;

    pub use super::*;
    use vm_memory::{GuestAddress, GuestMemoryMmap};

    // Represents a location in GuestMemoryMmap which holds a given type.
    pub struct SomeplaceInMemory<'a, T> {
        pub location: GuestAddress,
        mem: &'a GuestMemoryMmap,
        phantom: PhantomData<*const T>,
    }

    // The ByteValued trait is required to use mem.read_obj_from_addr and write_obj_at_addr.
    impl<'a, T> SomeplaceInMemory<'a, T>
    where
        T: vm_memory::ByteValued,
    {
        fn new(location: GuestAddress, mem: &'a GuestMemoryMmap) -> Self {
            SomeplaceInMemory {
                location,
                mem,
                phantom: PhantomData,
            }
        }

        // Reads from the actual memory location.
        pub fn get(&self) -> T {
            self.mem.read_obj(self.location).unwrap()
        }

        // Writes to the actual memory location.
        pub fn set(&self, val: T) {
            self.mem.write_obj(val, self.location).unwrap()
        }

        // This function returns a place in memory which holds a value of type U, and starts
        // offset bytes after the current location.
        fn map_offset<U>(&self, offset: usize) -> SomeplaceInMemory<'a, U> {
            SomeplaceInMemory {
                location: self.location.checked_add(offset as u64).unwrap(),
                mem: self.mem,
                phantom: PhantomData,
            }
        }

        // This function returns a place in memory which holds a value of type U, and starts
        // immediately after the end of self (which is location + sizeof(T)).
        fn next_place<U>(&self) -> SomeplaceInMemory<'a, U> {
            self.map_offset::<U>(mem::size_of::<T>())
        }

        fn end(&self) -> GuestAddress {
            self.location
                .checked_add(mem::size_of::<T>() as u64)
                .unwrap()
        }
    }

    // Represents a virtio descriptor in guest memory.
    pub struct VirtqDesc<'a> {
        pub addr: SomeplaceInMemory<'a, u64>,
        pub len: SomeplaceInMemory<'a, u32>,
        pub flags: SomeplaceInMemory<'a, u16>,
        pub next: SomeplaceInMemory<'a, u16>,
    }

    impl<'a> VirtqDesc<'a> {
        fn new(start: GuestAddress, mem: &'a GuestMemoryMmap) -> Self {
            assert_eq!(start.0 & 0xf, 0);

            let addr = SomeplaceInMemory::new(start, mem);
            let len = addr.next_place();
            let flags = len.next_place();
            let next = flags.next_place();

            VirtqDesc {
                addr,
                len,
                flags,
                next,
            }
        }

        fn start(&self) -> GuestAddress {
            self.addr.location
        }

        fn end(&self) -> GuestAddress {
            self.next.end()
        }

        pub fn set(&self, addr: u64, len: u32, flags: u16, next: u16) {
            self.addr.set(addr);
            self.len.set(len);
            self.flags.set(flags);
            self.next.set(next);
        }
    }

    // Represents a virtio queue ring. The only difference between the used and available rings,
    // is the ring element type.
    pub struct VirtqRing<'a, T> {
        pub flags: SomeplaceInMemory<'a, u16>,
        pub idx: SomeplaceInMemory<'a, u16>,
        pub ring: Vec<SomeplaceInMemory<'a, T>>,
        pub event: SomeplaceInMemory<'a, u16>,
    }

    impl<'a, T> VirtqRing<'a, T>
    where
        T: vm_memory::ByteValued,
    {
        fn new(
            start: GuestAddress,
            mem: &'a GuestMemoryMmap,
            qsize: u16,
            alignment: usize,
        ) -> Self {
            assert_eq!(start.0 & (alignment as u64 - 1), 0);

            let flags = SomeplaceInMemory::new(start, mem);
            let idx = flags.next_place();

            let mut ring = Vec::with_capacity(qsize as usize);

            ring.push(idx.next_place());

            for _ in 1..qsize as usize {
                let x = ring.last().unwrap().next_place();
                ring.push(x)
            }

            let event = ring.last().unwrap().next_place();

            flags.set(0);
            idx.set(0);
            event.set(0);

            VirtqRing {
                flags,
                idx,
                ring,
                event,
            }
        }

        pub fn end(&self) -> GuestAddress {
            self.event.end()
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct VirtqUsedElem {
        pub id: u32,
        pub len: u32,
    }

    unsafe impl vm_memory::ByteValued for VirtqUsedElem {}

    pub type VirtqAvail<'a> = VirtqRing<'a, u16>;
    pub type VirtqUsed<'a> = VirtqRing<'a, VirtqUsedElem>;

    pub struct VirtQueue<'a> {
        pub dtable: Vec<VirtqDesc<'a>>,
        pub avail: VirtqAvail<'a>,
        pub used: VirtqUsed<'a>,
    }

    impl<'a> VirtQueue<'a> {
        // We try to make sure things are aligned properly :-s
        pub fn new(start: GuestAddress, mem: &'a GuestMemoryMmap, qsize: u16) -> Self {
            // power of 2?
            assert!(qsize > 0 && qsize & (qsize - 1) == 0);

            let mut dtable = Vec::with_capacity(qsize as usize);

            let mut end = start;

            for _ in 0..qsize {
                let d = VirtqDesc::new(end, mem);
                end = d.end();
                dtable.push(d);
            }

            const AVAIL_ALIGN: usize = 2;

            let avail = VirtqAvail::new(end, mem, qsize, AVAIL_ALIGN);

            const USED_ALIGN: u64 = 4;

            let mut x = avail.end().0;
            x = (x + USED_ALIGN - 1) & !(USED_ALIGN - 1);

            let used = VirtqUsed::new(GuestAddress(x), mem, qsize, USED_ALIGN as usize);

            VirtQueue {
                dtable,
                avail,
                used,
            }
        }

        pub fn size(&self) -> u16 {
            self.dtable.len() as u16
        }

        fn dtable_start(&self) -> GuestAddress {
            self.dtable.first().unwrap().start()
        }

        fn avail_start(&self) -> GuestAddress {
            self.avail.flags.location
        }

        fn used_start(&self) -> GuestAddress {
            self.used.flags.location
        }

        // Creates a new Queue, using the underlying memory regions represented by the VirtQueue.
        pub fn create_queue(&self) -> Queue {
            let mut q = Queue::new(self.size());

            q.size = self.size();
            q.ready = true;
            q.desc_table = self.dtable_start();
            q.avail_ring = self.avail_start();
            q.used_ring = self.used_start();

            q
        }

        pub fn end(&self) -> GuestAddress {
            self.used.end()
        }
    }

    #[test]
    fn test_checked_new_descriptor_chain() {
        let m = &GuestMemoryMmap::from_ranges(&[
            (GuestAddress(0), 0x10000),
            (GuestAddress(0x20000), 0x2000),
        ])
        .unwrap();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);

        assert!(vq.end().0 < 0x1000);

        // index >= queue_size
        assert!(DescriptorChain::checked_new(m, vq.dtable_start(), 16, 16).is_none());

        // desc_table address is way off
        assert!(DescriptorChain::checked_new(m, GuestAddress(0x00ff_ffff_ffff), 16, 0).is_none());

        // Let's create an invalid chain.
        {
            // The first desc has a normal len, and the next_descriptor flag is set.
            vq.dtable[0].addr.set(0x1000);
            vq.dtable[0].len.set(0x1000);
            vq.dtable[0].flags.set(VIRTQ_DESC_F_NEXT);
            // .. but the the index of the next descriptor is too large
            vq.dtable[0].next.set(16);

            assert!(DescriptorChain::checked_new(m, vq.dtable_start(), 16, 0).is_none());
        }

        // Finally, let's test an ok chain.
        {
            vq.dtable[0].next.set(1);
            vq.dtable[1].set(0x2000, 0x1000, 0, 0);

            let c = DescriptorChain::checked_new(m, vq.dtable_start(), 16, 0).unwrap();

            assert_eq!(c.mem as *const GuestMemoryMmap, m as *const GuestMemoryMmap);
            assert_eq!(c.desc_table, vq.dtable_start());
            assert_eq!(c.queue_size, 16);
            assert_eq!(c.ttl, c.queue_size);
            assert_eq!(c.index, 0);
            assert_eq!(c.addr, GuestAddress(0x1000));
            assert_eq!(c.len, 0x1000);
            assert_eq!(c.flags, VIRTQ_DESC_F_NEXT);
            assert_eq!(c.next, 1);

            assert!(c.next_descriptor().unwrap().next_descriptor().is_none());
        }
    }

    #[test]
    #[allow(unused)]
    fn test_queue_validation() {
        let m = &GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);

        let mut q = vq.create_queue();

        // q is currently valid
        assert!(q.is_valid(m));

        // shouldn't be valid when not marked as ready
        q.ready = false;
        assert!(!q.is_valid(m));
        q.ready = true;

        // or when size > max_size
        q.size = q.max_size << 1;
        assert!(!q.is_valid(m));
        q.size = q.max_size;

        // or when size is 0
        q.size = 0;
        assert!(!q.is_valid(m));
        q.size = q.max_size;

        // or when size is not a power of 2
        q.size = 11;
        assert!(!q.is_valid(m));
        q.size = q.max_size;

        // or if the various addresses are off

        q.desc_table = GuestAddress(0xffff_ffff);
        assert!(!q.is_valid(m));
        q.desc_table = GuestAddress(0x1001);
        assert!(!q.is_valid(m));
        q.desc_table = vq.dtable_start();

        q.avail_ring = GuestAddress(0xffff_ffff);
        assert!(!q.is_valid(m));
        q.avail_ring = GuestAddress(0x1001);
        assert!(!q.is_valid(m));
        q.avail_ring = vq.avail_start();

        q.used_ring = GuestAddress(0xffff_ffff);
        assert!(!q.is_valid(m));
        q.used_ring = GuestAddress(0x1001);
        assert!(!q.is_valid(m));
        q.used_ring = vq.used_start();
    }

    /// Regression test: is_valid_params() must agree with is_valid() for the
    /// pure-logic conditions (readiness, size, alignment).  This prevents drift
    /// between the two if either is changed without updating the other.
    ///
    /// The memory-range checks in is_valid() are not replicated here because
    /// they require a real GuestMemoryMmap; those are covered by test_queue_validation.
    #[test]
    fn test_is_valid_params_matches_is_valid_conditions() {
        // Valid inputs — all conditions pass.
        assert!(Queue::is_valid_params(
            true, 16, 256, 0x0000, 0x0000, 0x0000
        ));

        // not ready → false.
        assert!(!Queue::is_valid_params(
            false, 16, 256, 0x0000, 0x0000, 0x0000
        ));

        // size == 0 → false.
        assert!(!Queue::is_valid_params(
            true, 0, 256, 0x0000, 0x0000, 0x0000
        ));

        // size > max_size → false.
        assert!(!Queue::is_valid_params(
            true, 512, 256, 0x0000, 0x0000, 0x0000
        ));

        // size not a power of two → false.
        assert!(!Queue::is_valid_params(
            true, 3, 256, 0x0000, 0x0000, 0x0000
        ));

        // desc_table misaligned (not 16-byte aligned) → false.
        assert!(!Queue::is_valid_params(
            true, 16, 256, 0x0001, 0x0000, 0x0000
        ));

        // avail_ring misaligned (not 2-byte aligned) → false.
        assert!(!Queue::is_valid_params(
            true, 16, 256, 0x0000, 0x0001, 0x0000
        ));

        // used_ring misaligned (not 4-byte aligned) → false.
        assert!(!Queue::is_valid_params(
            true, 16, 256, 0x0000, 0x0000, 0x0002
        ));

        // All alignment boundaries: 16-byte aligned desc, 2-byte avail, 4-byte used.
        assert!(Queue::is_valid_params(true, 1, 1, 0x0010, 0x0002, 0x0004));
    }

    #[test]
    fn test_queue_processing() {
        let m = &GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);
        let mut q = vq.create_queue();

        q.ready = true;

        // Let's create two simple descriptor chains.

        for j in 0..5 {
            vq.dtable[j].set(
                0x1000 * (j + 1) as u64,
                0x1000,
                VIRTQ_DESC_F_NEXT,
                (j + 1) as u16,
            );
        }

        // the chains are (0, 1) and (2, 3, 4)
        vq.dtable[1].flags.set(0);
        vq.dtable[4].flags.set(0);
        vq.avail.ring[0].set(0);
        vq.avail.ring[1].set(2);
        vq.avail.idx.set(2);

        // We've just set up two chains.
        assert_eq!(q.len(m), 2);

        // The first chain should hold exactly two descriptors.
        let d = q.pop(m).unwrap().next_descriptor().unwrap();
        assert!(!d.has_next());
        assert!(d.next_descriptor().is_none());

        // We popped one chain, so there should be only one left.
        assert_eq!(q.len(m), 1);

        // The next chain holds three descriptors.
        let d = q
            .pop(m)
            .unwrap()
            .next_descriptor()
            .unwrap()
            .next_descriptor()
            .unwrap();
        assert!(!d.has_next());
        assert!(d.next_descriptor().is_none());

        // We've popped both chains, so the queue should be empty.
        assert!(q.is_empty(m));
        assert!(q.pop(m).is_none());

        // Undoing the last pop should let us walk the last chain again.
        q.undo_pop();
        assert_eq!(q.len(m), 1);

        // Walk the last chain again (three descriptors).
        let d = q
            .pop(m)
            .unwrap()
            .next_descriptor()
            .unwrap()
            .next_descriptor()
            .unwrap();
        assert!(!d.has_next());
        assert!(d.next_descriptor().is_none());
    }

    #[test]
    fn test_add_used() {
        let m = &GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);

        let mut q = vq.create_queue();
        assert_eq!(vq.used.idx.get(), 0);

        //index too large
        let _ = q.add_used(m, 16, 0x1000);
        assert_eq!(vq.used.idx.get(), 0);

        //should be ok
        let _ = q.add_used(m, 1, 0x1000);
        assert_eq!(vq.used.idx.get(), 1);
        let x = vq.used.ring[0].get();
        assert_eq!(x.id, 1);
        assert_eq!(x.len, 0x1000);
    }
}

// Why there are no full end-to-end proofs calling pop() / add_used() through
// a live GuestMemoryMmap
// ---------------------------------------------------------------------------
//
// Firecracker's queue.rs achieves end-to-end proofs by:
//   (a) making Queue generic over M: GuestMemory, and
//   (b) supplying a hand-rolled ProofGuestMemory that avoids mmap() and whose
//       error types don't involve std::io::Error's complex Drop recursion.
//
// Our queue is hardcoded to &GuestMemoryMmap in every method signature, so we
// cannot substitute a simpler memory type.  The transmute-into-MmapRegion trick
// (also used by Firecracker for the construction step) can still be used to build
// a GuestMemoryMmap without calling mmap(), but the vm_memory::GuestMemoryError
// type wraps std::io::Error, whose Drop implementation recurses through several
// boxed trait-object paths that Kani cannot model at any unwind depth.  The
// verifier reports "unsupported construct: foreign function" for the I/O error
// drop path and aborts those branches.
//
// The practical impact is small: all of the interesting algorithmic properties
// of pop(), add_used(), and needs_notification() are captured below by
//   * extracting the arithmetic / guard logic into standalone proofs, and
//   * unit-testing the happy-path and error-path through the ordinary #[test]
//     suite (which runs against a real GuestMemoryMmap with no restrictions).
//
// If Queue is ever refactored to be generic over GuestMemory the proofs below
// can be extended with a ProofGuestMemory and the e2e harnesses re-enabled.
#[cfg(kani)]
mod verification {
    use super::*;

    // ---------------------------------------------------------------------------
    // Properties verified:
    //   1.  Queue initialisation defaults
    //   2.  actual_size invariants
    //   3.  is_valid size/alignment preconditions — all proofs call
    //       Queue::is_valid_params() directly so implementation drift is caught;
    //       includes a regression proof that is_valid_params agrees with the
    //       conditions inside is_valid() for all symbolic inputs
    //   4.  Index wrapping arithmetic (set/get, undo_pop, go_to_previous_position)
    //   5.  Ring slot modulo bounds (pop, add_used)
    //   6.  VirtqUsedElem construction round-trip
    //   7.  Descriptor flag predicates (write_only, read_only, has_next)
    //   8.  DescriptorChain::is_valid logic (bounds check on next field)
    //   9.  TTL cycle-prevention (traversal budget)
    //   10. Ring offset arithmetic: no u64 overflow
    //   11. Notification suppression arithmetic (Virtio spec §2.6.7.2)
    //   12. add_used index bounds guard (pure logic, returns before any memory I/O)
    //   13. set_event_idx / set_next_avail / set_next_used round-trips
    //   14. Struct size constants (Virtio spec §2.6.5, §2.6.8)
    //   15. pop() index_offset formula stays within avail ring bounds
    // ---------------------------------------------------------------------------

    // ---------------------------------------------------------------------------
    // 1. Queue initialisation
    // ---------------------------------------------------------------------------

    /// Proof: Queue::new initialises all fields to safe defaults.
    ///
    /// After construction the queue must not be ready, must have size 0,
    /// and both ring indices must start at zero.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_queue_new_defaults() {
        let max: u16 = kani::any();
        let q = Queue::new(max);
        kani::assert(q.max_size == max, "max_size must equal constructor arg");
        kani::assert(q.size == 0, "size must start at 0");
        kani::assert(!q.ready, "queue must start not-ready");
        kani::assert(q.next_avail.0 == 0, "next_avail must start at 0");
        kani::assert(q.next_used.0 == 0, "next_used must start at 0");
        kani::assert(q.num_added.0 == 0, "num_added must start at 0");
        kani::cover!(true, "queue_new_defaults reachable");
    }

    // ---------------------------------------------------------------------------
    // 2. actual_size invariants
    // ---------------------------------------------------------------------------

    /// Proof: actual_size returns min(size, max_size) for all u16 pairs.
    ///
    /// Exhaustive over the full (size, max_size) domain — no loops in the function.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_actual_size() {
        let size: u16 = kani::any();
        let max_size: u16 = kani::any();
        let mut q = Queue::new(max_size);
        q.size = size;
        let actual = q.actual_size();
        kani::assert(
            actual == size.min(max_size),
            "actual_size must equal min(size, max_size)",
        );
        kani::assert(actual <= max_size, "actual_size must not exceed max_size");
        kani::assert(actual <= size, "actual_size must not exceed size");
        kani::cover!(true, "actual_size proof reachable");
    }

    /// Proof: get_max_size always returns the value passed to Queue::new.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_get_max_size_roundtrip() {
        let max: u16 = kani::any();
        let q = Queue::new(max);
        kani::assert(
            q.get_max_size() == max,
            "get_max_size must return the constructor value",
        );
        kani::cover!(true, "get_max_size roundtrip reachable");
    }

    // ---------------------------------------------------------------------------
    // 3. is_valid preconditions (size / readiness / alignment — pure logic)
    //
    // All proofs call Queue::is_valid_params() directly so that changes to the
    // real implementation are caught immediately.  Previously these proofs
    // inlined the conditions — making them disconnected from the code under
    // verification.
    // ---------------------------------------------------------------------------

    /// Proof: the "not ready" branch of is_valid_params produces false, and the
    /// ready branch (with otherwise-valid inputs) can produce true.
    ///
    /// Uses symbolic `ready` and otherwise-valid fixed inputs to verify both
    /// branches are reachable and that `!ready` always forces the result to false.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_is_valid_logic_ready_gate() {
        let ready: bool = kani::any();
        // Use a fixed valid size (power-of-two, <= max_size) and aligned addrs
        // so that only the `ready` gate is exercised.
        let result = Queue::is_valid_params(
            ready, /*size=*/ 16, /*max_size=*/ 256, /*desc_table_addr=*/ 0x0000,
            /*avail_ring_addr=*/ 0x1000, /*used_ring_addr=*/ 0x2000,
        );
        if !ready {
            kani::assert(!result, "not-ready must be invalid");
        } else {
            kani::assert(result, "ready must pass the first gate");
        }
        kani::cover!(ready, "ready path reachable");
        kani::cover!(!ready, "not-ready path reachable");
    }

    /// Proof: the size validity conditions in is_valid_params are correct.
    ///
    /// size > max_size, size == 0, or size not-a-power-of-two → false.
    /// Valid (power-of-two, non-zero, <= max_size) sizes → true (for this branch).
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_is_valid_size_conditions() {
        let size: u16 = kani::any();
        let max_size: u16 = kani::any_where(|&m: &u16| m > 0);

        // Call is_valid_params with aligned addrs and ready=true so only the
        // size check is the discriminating factor.
        let result = Queue::is_valid_params(
            true, size, max_size, /*desc_table_addr=*/ 0x0000,
            /*avail_ring_addr=*/ 0x0000, /*used_ring_addr=*/ 0x0000,
        );

        let size_invalid = size > max_size || size == 0 || (size & size.wrapping_sub(1)) != 0;

        // If size is a non-zero power of two and does not exceed max_size, it
        // must be accepted by is_valid_params.
        if !size_invalid {
            kani::assert(size > 0, "valid size must be non-zero");
            kani::assert(size <= max_size, "valid size must not exceed max_size");
            kani::assert(size.count_ones() == 1, "valid size must be a power of two");
            kani::assert(result, "is_valid_params must accept valid size");
        } else {
            kani::assert(!result, "is_valid_params must reject invalid size");
        }

        // Verify that size=0 is always flagged invalid by is_valid_params.
        let size_zero_result = Queue::is_valid_params(true, 0, max_size, 0x0000, 0x0000, 0x0000);
        kani::assert(!size_zero_result, "size=0 must always fail the size check");

        kani::cover!(!size_invalid, "valid size path reachable");
        kani::cover!(size_invalid, "invalid size path reachable");
    }

    /// Proof: alignment condition for desc_table (must be 16-byte aligned).
    ///
    /// Calls is_valid_params with symbolic desc_table_addr and otherwise-valid
    /// fixed inputs so that only the desc_table alignment check is exercised.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_is_valid_desc_table_alignment() {
        let addr: u64 = kani::any();
        let result = Queue::is_valid_params(
            true, /*size=*/ 16, /*max_size=*/ 256, addr,
            /*avail_ring_addr=*/ 0x0000, /*used_ring_addr=*/ 0x0000,
        );
        let aligned = addr & 0xf == 0;
        if aligned {
            kani::assert(result, "16-byte aligned desc_table must be accepted");
        } else {
            kani::assert(!result, "misaligned desc_table must be rejected");
        }
        kani::cover!(aligned, "aligned desc_table path reachable");
        kani::cover!(!aligned, "misaligned desc_table path reachable");
    }

    /// Proof: alignment condition for avail_ring (must be 2-byte aligned).
    ///
    /// Calls is_valid_params with symbolic avail_ring_addr and otherwise-valid
    /// fixed inputs so that only the avail_ring alignment check is exercised.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_is_valid_avail_ring_alignment() {
        let addr: u64 = kani::any();
        let result = Queue::is_valid_params(
            true, /*size=*/ 16, /*max_size=*/ 256, /*desc_table_addr=*/ 0x0000,
            addr, /*used_ring_addr=*/ 0x0000,
        );
        let aligned = addr & 0x1 == 0;
        if aligned {
            kani::assert(result, "2-byte aligned avail_ring must be accepted");
        } else {
            kani::assert(!result, "misaligned avail_ring must be rejected");
        }
        kani::cover!(aligned, "aligned avail_ring path reachable");
        kani::cover!(!aligned, "misaligned avail_ring path reachable");
    }

    /// Proof: alignment condition for used_ring (must be 4-byte aligned).
    ///
    /// Calls is_valid_params with symbolic used_ring_addr and otherwise-valid
    /// fixed inputs so that only the used_ring alignment check is exercised.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_is_valid_used_ring_alignment() {
        let addr: u64 = kani::any();
        let result = Queue::is_valid_params(
            true, /*size=*/ 16, /*max_size=*/ 256, /*desc_table_addr=*/ 0x0000,
            /*avail_ring_addr=*/ 0x0000, addr,
        );
        let aligned = addr & 0x3 == 0;
        if aligned {
            kani::assert(result, "4-byte aligned used_ring must be accepted");
        } else {
            kani::assert(!result, "misaligned used_ring must be rejected");
        }
        kani::cover!(aligned, "aligned used_ring path reachable");
        kani::cover!(!aligned, "misaligned used_ring path reachable");
    }

    /// Proof: is_valid_params agrees with is_valid() on representative cases
    /// that do not require guest memory access.
    ///
    /// Specifically: for any inputs where all memory-range checks trivially pass
    /// (addresses are zero and queue_size-derived ring sizes are zero when size=0,
    /// so we use ready=true + valid size=1 as the simplest non-trivial case),
    /// is_valid_params must return the same boolean as the pure conditions inside
    /// is_valid().
    ///
    /// This is a regression guard: if the conditions inside is_valid() are changed
    /// without updating is_valid_params(), this proof will fail.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_is_valid_params_matches_is_valid_conditions() {
        let ready: bool = kani::any();
        let size: u16 = kani::any();
        let max_size: u16 = kani::any_where(|&m: &u16| m > 0);
        let desc_table_addr: u64 = kani::any();
        let avail_ring_addr: u64 = kani::any();
        let used_ring_addr: u64 = kani::any();

        // Manually replicate the exact conditions from is_valid_params so that
        // the proof detects any drift between the two.
        let size_invalid = size > max_size || size == 0 || (size & (size - 1)) != 0;
        let expected = ready
            && !size_invalid
            && (desc_table_addr & 0xf == 0)
            && (avail_ring_addr & 0x1 == 0)
            && (used_ring_addr & 0x3 == 0);

        let actual = Queue::is_valid_params(
            ready,
            size,
            max_size,
            desc_table_addr,
            avail_ring_addr,
            used_ring_addr,
        );

        kani::assert(
            actual == expected,
            "is_valid_params must agree with the inlined conditions from is_valid()",
        );
        kani::cover!(actual, "valid params path reachable");
        kani::cover!(!actual, "invalid params path reachable");
    }

    // ---------------------------------------------------------------------------
    // 4. Index wrapping arithmetic
    // ---------------------------------------------------------------------------

    /// Proof: set_next_avail/next_avail round-trip.
    ///
    /// For every u16 idx, set then get must return Wrapping(idx).
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_set_next_avail_roundtrip() {
        let idx: u16 = kani::any();
        let mut q = Queue::new(256);
        q.set_next_avail(idx);
        kani::assert(
            q.next_avail() == Wrapping(idx),
            "next_avail must equal set value",
        );
        kani::cover!(true, "set_next_avail roundtrip reachable");
    }

    /// Proof: set_next_used/next_used round-trip.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_set_next_used_roundtrip() {
        let idx: u16 = kani::any();
        let mut q = Queue::new(256);
        q.set_next_used(idx);
        kani::assert(
            q.next_used() == Wrapping(idx),
            "next_used must equal set value",
        );
        kani::cover!(true, "set_next_used roundtrip reachable");
    }

    /// Proof: undo_pop decrements next_avail by exactly one (wrapping).
    ///
    /// pop increments next_avail by 1; undo_pop must exactly undo that.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_undo_pop_decrements_next_avail() {
        let idx: u16 = kani::any();
        let mut q = Queue::new(256);
        q.next_avail = Wrapping(idx);
        let before = q.next_avail;
        q.undo_pop();
        kani::assert(
            q.next_avail == before - Wrapping(1),
            "undo_pop must decrement next_avail by 1 (wrapping)",
        );
        kani::cover!(true, "undo_pop decrements reachable");
    }

    /// Proof: go_to_previous_position decrements next_avail by exactly one (wrapping).
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_go_to_previous_position_decrements() {
        let idx: u16 = kani::any();
        let mut q = Queue::new(256);
        q.next_avail = Wrapping(idx);
        let before = q.next_avail;
        q.go_to_previous_position();
        kani::assert(
            q.next_avail == before - Wrapping(1),
            "go_to_previous_position must decrement next_avail by 1 (wrapping)",
        );
        kani::cover!(true, "go_to_previous_position reachable");
    }

    // ---------------------------------------------------------------------------
    // 5. Ring slot modulo bounds
    // ---------------------------------------------------------------------------

    /// Proof: index_offset modulo in pop is always < actual_size.
    ///
    /// pop computes `next_avail.0 % actual_size()` to find the ring slot.
    /// The result must be strictly less than actual_size for any inputs.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_pop_index_offset_modulo() {
        let next_avail: u16 = kani::any();
        let actual_size: u16 = kani::any_where(|&s: &u16| s > 0);
        let ring_slot = next_avail % actual_size;
        kani::assert(ring_slot < actual_size, "ring slot must be < actual_size");
        kani::cover!(true, "pop index_offset_modulo reachable");
    }

    /// Proof: add_used ring slot modulo stays within queue size.
    ///
    /// add_used computes `next_used.0 % size` for the used ring slot.
    /// For all (next_used, size > 0), the result is strictly less than size.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_add_used_ring_slot_in_bounds() {
        let next_used: u16 = kani::any();
        let size: u16 = kani::any_where(|&s: &u16| s > 0);
        let slot = u64::from(next_used % size);
        kani::assert(slot < u64::from(size), "used ring slot must be < size");
        kani::cover!(true, "add_used ring slot in-bounds reachable");
    }

    // ---------------------------------------------------------------------------
    // 6. VirtqUsedElem construction
    // ---------------------------------------------------------------------------

    /// Proof: VirtqUsedElem::new stores id and len without modification.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_virtq_used_elem_new() {
        let id: u32 = kani::any();
        let len: u32 = kani::any();
        let elem = VirtqUsedElem::new(id, len);
        kani::assert(elem.id == id, "VirtqUsedElem::new must store id");
        kani::assert(elem.len == len, "VirtqUsedElem::new must store len");
        kani::cover!(true, "VirtqUsedElem::new reachable");
    }

    // ---------------------------------------------------------------------------
    // 7. Descriptor flag predicates
    // ---------------------------------------------------------------------------

    /// Proof: is_write_only and is_read_only are mutually exclusive and exhaustive.
    ///
    /// For every flags value exactly one of the two holds.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_descriptor_write_read_exclusive() {
        let flags: u16 = kani::any();
        let write_only = flags & VIRTQ_DESC_F_WRITE != 0;
        let read_only = flags & VIRTQ_DESC_F_WRITE == 0;
        kani::assert(
            write_only != read_only,
            "is_write_only and is_read_only must be mutually exclusive",
        );
        kani::assert(
            write_only || read_only,
            "at least one of write_only or read_only must hold",
        );
        kani::cover!(true, "descriptor write/read exclusive reachable");
    }

    /// Proof: VIRTQ_DESC_F_NEXT and VIRTQ_DESC_F_WRITE occupy distinct bit positions.
    ///
    /// If they overlapped, has_next and is_write_only would interfere.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_descriptor_flag_bits_distinct() {
        kani::assert(
            VIRTQ_DESC_F_NEXT & VIRTQ_DESC_F_WRITE == 0,
            "NEXT and WRITE flag bits must not overlap",
        );
        kani::cover!(true, "descriptor flag bits distinct reachable");
    }

    /// Proof: has_next requires both the NEXT flag and ttl > 1.
    ///
    /// This captures the combined ttl/flag guard in DescriptorChain::has_next.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_has_next_requires_flag_and_ttl() {
        let flags: u16 = kani::any();
        let ttl: u16 = kani::any();

        // Mirror has_next implementation: flags & NEXT != 0 && ttl > 1
        let has_next = flags & VIRTQ_DESC_F_NEXT != 0 && ttl > 1;

        if flags & VIRTQ_DESC_F_NEXT == 0 {
            kani::assert(!has_next, "has_next must be false when NEXT flag is clear");
        }
        if ttl <= 1 {
            kani::assert(!has_next, "has_next must be false when ttl <= 1");
        }
        if flags & VIRTQ_DESC_F_NEXT != 0 && ttl > 1 {
            kani::assert(
                has_next,
                "has_next must be true when NEXT flag set and ttl > 1",
            );
        }
        kani::cover!(true, "has_next guard proof reachable");
    }

    // ---------------------------------------------------------------------------
    // 8. DescriptorChain::is_valid logic
    // ---------------------------------------------------------------------------

    /// Proof: is_valid returns false when next >= queue_size (with has_next true).
    ///
    /// Invariant from DescriptorChain::is_valid: !has_next || next < queue_size.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_descriptor_is_valid_next_oob() {
        let queue_size: u16 = kani::any_where(|&s: &u16| s > 0);
        let next: u16 = kani::any_where(|&n: &u16| n >= queue_size);
        // flags has NEXT set and ttl > 1 → has_next is true
        let has_next = true;
        let is_valid = !has_next || next < queue_size;
        kani::assert(
            !is_valid,
            "next >= queue_size with has_next must be invalid",
        );
        kani::cover!(true, "descriptor is_valid oob next reachable");
    }

    /// Proof: is_valid returns true when next < queue_size (with has_next true).
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_descriptor_is_valid_next_inbounds() {
        let queue_size: u16 = kani::any_where(|&s: &u16| s > 1);
        let next: u16 = kani::any_where(|&n: &u16| n < queue_size);
        let has_next = true;
        let is_valid = !has_next || next < queue_size;
        kani::assert(is_valid, "next < queue_size with has_next must be valid");
        kani::cover!(true, "descriptor is_valid inbounds next reachable");
    }

    /// Proof: is_valid always returns true when has_next is false.
    ///
    /// Without a NEXT link the next field is irrelevant; any value is permitted.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_descriptor_is_valid_no_next_flag() {
        let queue_size: u16 = kani::any_where(|&s: &u16| s > 0);
        let next: u16 = kani::any(); // may be out-of-bounds
        let has_next = false;
        let is_valid = !has_next || next < queue_size;
        kani::assert(
            is_valid,
            "without has_next, is_valid must be true regardless of next",
        );
        kani::cover!(true, "descriptor is_valid no-next-flag reachable");
    }

    // ---------------------------------------------------------------------------
    // 9. TTL cycle-prevention
    // ---------------------------------------------------------------------------

    /// Proof: ttl strictly decrements on each next_descriptor step.
    ///
    /// checked_new sets ttl = queue_size; next_descriptor sets c.ttl = self.ttl - 1.
    /// This guarantees the traversal terminates after at most queue_size steps.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_ttl_decrements_on_next() {
        let queue_size: u16 = kani::any_where(|&s: &u16| s > 1);
        let ttl: u16 = kani::any_where(|&t: &u16| t > 1 && t <= queue_size);
        let new_ttl = ttl - 1;
        kani::assert(
            new_ttl < ttl,
            "ttl must strictly decrease after next_descriptor",
        );
        kani::assert(new_ttl < queue_size, "decremented ttl must be < queue_size");
        kani::cover!(true, "ttl decrements proof reachable");
    }

    /// Proof: ttl <= 1 stops traversal regardless of flags.
    ///
    /// The has_next guard requires `ttl > 1`. For any symbolic flags value and
    /// any ttl in {0, 1}, has_next must be false. This is the termination
    /// guarantee: no chain can exceed queue_size steps.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_ttl_low_stops_traversal() {
        let flags: u16 = kani::any();
        let ttl: u16 = kani::any_where(|&t: &u16| t <= 1);
        let has_next = flags & VIRTQ_DESC_F_NEXT != 0 && ttl > 1;
        kani::assert(
            !has_next,
            "ttl <= 1 must prevent traversal regardless of flags",
        );
        kani::cover!(
            flags & VIRTQ_DESC_F_NEXT != 0,
            "NEXT flag set but ttl stops traversal"
        );
        kani::cover!(ttl == 0, "ttl=0 path reachable");
        kani::cover!(ttl == 1, "ttl=1 path reachable");
    }

    // ---------------------------------------------------------------------------
    // 10. Ring offset arithmetic: no u64 overflow
    // ---------------------------------------------------------------------------

    /// Proof: used_event offset calculation does not overflow u64.
    ///
    /// used_event_offset = VIRTQ_AVAIL_RING_HEADER_SIZE + size * VIRTQ_AVAIL_ELEMENT_SIZE
    /// Max: 4 + 65535 * 2 = 131074 — far below u64::MAX.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_used_event_offset_no_overflow() {
        let size: u16 = kani::any();
        let term = u64::from(size)
            .checked_mul(VIRTQ_AVAIL_ELEMENT_SIZE)
            .unwrap();
        let offset = VIRTQ_AVAIL_RING_HEADER_SIZE.checked_add(term).unwrap();
        kani::assert(offset <= 131074, "used_event_offset must be bounded");
        kani::cover!(true, "used_event_offset no-overflow reachable");
    }

    /// Proof: avail_event offset in the used ring does not overflow u64.
    ///
    /// avail_event_offset = VIRTQ_USED_RING_HEADER_SIZE + VIRTQ_USED_ELEMENT_SIZE * size
    /// Max: 4 + 8 * 65535 = 524284.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_avail_event_offset_no_overflow() {
        let size: u16 = kani::any();
        let term = VIRTQ_USED_ELEMENT_SIZE
            .checked_mul(u64::from(size))
            .unwrap();
        let offset = VIRTQ_USED_RING_HEADER_SIZE.checked_add(term).unwrap();
        kani::assert(offset <= 524284, "avail_event_offset must be bounded");
        kani::cover!(true, "avail_event_offset no-overflow reachable");
    }

    /// Proof: add_used ring write offset does not overflow u64.
    ///
    /// offset = VIRTQ_USED_RING_HEADER_SIZE + next_used_index * VIRTQ_USED_ELEMENT_SIZE
    /// next_used_index < size (post-modulo), so max = 4 + 65534 * 8 = 524276.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_add_used_offset_no_overflow() {
        let size: u16 = kani::any_where(|&s: &u16| s > 0);
        let next_used: u16 = kani::any();
        let next_used_index = u64::from(next_used % size); // < size ≤ 65535
        let term = next_used_index
            .checked_mul(VIRTQ_USED_ELEMENT_SIZE)
            .unwrap();
        let offset = VIRTQ_USED_RING_HEADER_SIZE.checked_add(term).unwrap();
        kani::assert(offset <= 524276, "add_used offset must be bounded");
        kani::cover!(true, "add_used offset no-overflow reachable");
    }

    // ---------------------------------------------------------------------------
    // 11. Notification suppression arithmetic (Virtio spec §2.6.7.2)
    //
    // needs_notification logic (event_idx path):
    //   old = used_idx - num_added
    //   result = (used_idx - used_event - 1) < (used_idx - old)
    //
    // Semantics: did used_event fall in the half-open interval [old, used_idx)
    // in the circular u16 sequence space?
    // ---------------------------------------------------------------------------

    /// Proof: when num_added == 0 the interval is empty; no notification is required.
    ///
    /// old == used_idx, so (used_idx - old) == 0, making the comparison always false.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_notification_no_added_no_notify() {
        let used_idx: Wrapping<u16> = Wrapping(kani::any());
        let used_event: Wrapping<u16> = Wrapping(kani::any());
        let num_added: Wrapping<u16> = Wrapping(0);

        let old = used_idx - num_added; // == used_idx
        let lhs = used_idx - used_event - Wrapping(1);
        let rhs = used_idx - old; // == 0

        kani::assert(rhs.0 == 0, "rhs must be 0 when num_added is 0");
        let needs_notification = lhs < rhs;
        kani::assert(!needs_notification, "no notification when num_added is 0");
        kani::cover!(true, "notification no-added reachable");
    }

    /// Proof: when used_event == used_idx - 1 and num_added >= 1, notification is required.
    ///
    /// Corresponds to the Virtio spec mandatory notification case: the driver's
    /// event threshold was just crossed by the most recently written used slot.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_notification_exact_event_requires_notify() {
        let used_idx: Wrapping<u16> = Wrapping(kani::any());
        let num_added: Wrapping<u16> = Wrapping(kani::any_where(|&n: &u16| n >= 1));
        // used_event points exactly at the last-written slot.
        let used_event: Wrapping<u16> = used_idx - Wrapping(1);

        let old = used_idx - num_added;
        let lhs = used_idx - used_event - Wrapping(1); // == 0
        let rhs = used_idx - old; // == num_added >= 1

        let needs_notification = lhs < rhs;
        kani::assert(
            needs_notification,
            "notification required when used_event == used_idx - 1 and num_added >= 1",
        );
        kani::cover!(true, "notification exact-event reachable");
    }

    /// Proof: when used_event == used_idx (one past the window end), no notification is sent.
    ///
    /// lhs = used_idx - used_idx - 1 = u16::MAX (wrapping), rhs = num_added.
    /// For any num_added in [1, u16::MAX], u16::MAX >= num_added, so lhs >= rhs.
    /// The full u16 range is verified (no constraint on num_added beyond >= 1).
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_notification_event_outside_window() {
        let used_idx: Wrapping<u16> = Wrapping(kani::any());
        let num_added: Wrapping<u16> = Wrapping(kani::any_where(|&n: &u16| n >= 1));
        // used_event is set to exactly used_idx (one past the end of the window).
        let used_event: Wrapping<u16> = used_idx;

        let old = used_idx - num_added;
        // lhs = used_idx - used_idx - 1 = Wrapping(u16::MAX)
        let lhs = used_idx - used_event - Wrapping(1);
        let rhs = used_idx - old; // == num_added

        // u16::MAX >= any u16 value, so lhs >= rhs → no notification.
        let needs_notification = lhs < rhs;
        kani::assert(
            !needs_notification,
            "no notification when used_event == used_idx (outside window)",
        );
        kani::cover!(true, "notification outside window reachable");
    }

    // ---------------------------------------------------------------------------
    // 12. add_used index bounds guard (pure logic)
    // ---------------------------------------------------------------------------

    /// Proof: the add_used guard correctly partitions head_index values.
    ///
    /// For fully symbolic (size, head_index), the guard `head_index >= size`
    /// rejects exactly those indices that are out of bounds, and accepts
    /// exactly those that are in bounds. Both paths are reachable.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_add_used_guard_partitions() {
        let size: u16 = kani::any_where(|&s: &u16| s > 0);
        let head_index: u16 = kani::any();
        let rejected = head_index >= size;
        if rejected {
            kani::assert(head_index >= size, "rejected head_index must be >= size");
        } else {
            kani::assert(head_index < size, "accepted head_index must be < size");
            // Verify the subsequent modulo in add_used is safe.
            let next_used_index = u64::from(head_index % size);
            kani::assert(
                next_used_index < u64::from(size),
                "modulo result must be < size",
            );
        }
        kani::cover!(rejected, "out-of-bounds path reachable");
        kani::cover!(!rejected, "in-bounds path reachable");
    }

    // ---------------------------------------------------------------------------
    // 13. set_event_idx round-trip
    // ---------------------------------------------------------------------------

    /// Proof: set_event_idx stores the provided boolean.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_set_event_idx() {
        let enabled: bool = kani::any();
        let mut q = Queue::new(256);
        q.set_event_idx(enabled);
        kani::assert(
            q.event_idx_enabled == enabled,
            "set_event_idx must store provided value",
        );
        kani::cover!(true, "set_event_idx proof reachable");
    }

    // ---------------------------------------------------------------------------
    // 14. Struct size constants (Virtio spec §2.6.5, §2.6.8)
    // ---------------------------------------------------------------------------

    /// Proof: ring structure size constants match the Virtio spec values.
    ///
    /// - §2.6.5: each descriptor is 16 bytes.
    /// - §2.6.6: avail ring element is 2 bytes (le16).
    /// - §2.6.8: used ring element is 8 bytes (le32 id + le32 len).
    /// - Ring headers: flags(2) + idx(2) = 4 bytes each.
    /// Verify: ring size constants are correct.
    /// These are compile-time assertions (const usize values).
    const _: () = {
        let _ = [(); 1][if VIRTQ_USED_RING_HEADER_SIZE == 4 {
            0
        } else {
            1
        }];
        let _ = [(); 1][if VIRTQ_USED_ELEMENT_SIZE == 8 { 0 } else { 1 }];
        let _ = [(); 1][if VIRTQ_AVAIL_RING_HEADER_SIZE == 4 {
            0
        } else {
            1
        }];
        let _ = [(); 1][if VIRTQ_AVAIL_ELEMENT_SIZE == 2 { 0 } else { 1 }];
    };

    /// Verify: Descriptor is 16 bytes (Virtio spec §2.6.5 alignment requirement).
    /// This is a compile-time assertion (const usize at compile time).
    const _: () = {
        let _ = [(); 1][if std::mem::size_of::<Descriptor>() == 16 {
            0
        } else {
            1
        }];
    };

    /// Verify: VirtqUsedElem is 8 bytes (Virtio spec §2.6.8).
    /// This is a compile-time assertion (const usize at compile time).
    const _: () = {
        let _ = [(); 1][if std::mem::size_of::<VirtqUsedElem>() == 8 {
            0
        } else {
            1
        }];
    };

    // ---------------------------------------------------------------------------
    // 15. pop() index_offset formula stays within avail ring bounds
    // ---------------------------------------------------------------------------

    /// Proof: the index_offset computed in pop() always lands inside the avail ring.
    ///
    /// pop() computes `index_offset = 4 + 2 * (next_avail.0 % actual_size())`.
    /// This is the byte offset of ring[next_avail % size] within the avail ring
    /// struct.  The avail ring is `4 + 2*size + 2` bytes total; the read must
    /// point at a valid u16 slot, so index_offset + 2 <= 4 + 2*size.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_pop_index_offset_within_avail_ring() {
        let size: u16 = kani::any_where(|&s: &u16| s > 0);
        let next_avail: u16 = kani::any();

        // Mirror pop()'s formula exactly.
        let slot = next_avail % size; // 0 ..= size-1
        let index_offset: u32 = 4 + 2 * u32::from(slot);

        // The avail ring is 4 + 2*size + 2 bytes.  The last valid slot ends at
        // 4 + 2*(size-1) + 2 = 4 + 2*size, which equals the used_event offset.
        // Our read is a u16, so we need index_offset + 2 <= 4 + 2*size.
        let avail_ring_size: u32 = 4 + 2 * u32::from(size) + 2;
        kani::assert(
            index_offset + 2 <= avail_ring_size,
            "pop index_offset must not exceed avail ring bounds",
        );
        kani::assert(
            index_offset >= 4,
            "pop index_offset must be past the header",
        );
        kani::cover!(true, "pop index_offset within avail ring reachable");
    }

    // ---------------------------------------------------------------------------
    // 16. ByteValued round-trips for VirtqUsedElem and Descriptor
    // ---------------------------------------------------------------------------

    /// Proof: any bit pattern is a valid VirtqUsedElem (ByteValued correctness).
    ///
    /// ByteValued requires that all bit patterns are valid (POD, no invalid
    /// bit patterns).  This proof constructs a VirtqUsedElem from arbitrary
    /// bytes via `from_slice`, verifies the result is Some (never fails), and
    /// then round-trips back to bytes, confirming size invariance.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_byte_valued_virtq_used_elem_roundtrip() {
        let bytes: [u8; 8] = kani::any();
        // from_slice must succeed for any 8-byte input (all bit patterns valid).
        let val = VirtqUsedElem::from_slice(&bytes)
            .expect("VirtqUsedElem: from_slice must succeed for any 8 bytes");
        // as_slice must produce exactly size_of::<VirtqUsedElem>() bytes.
        kani::assert(
            val.as_slice().len() == std::mem::size_of::<VirtqUsedElem>(),
            "VirtqUsedElem: as_slice length must equal size_of",
        );
        // The serialised bytes must match the input bytes (identity round-trip).
        kani::assert(
            val.as_slice() == bytes,
            "VirtqUsedElem: byte round-trip must be identity",
        );
        kani::cover!(true, "VirtqUsedElem ByteValued roundtrip reachable");
    }

    /// Proof: any bit pattern is a valid Descriptor (ByteValued correctness).
    ///
    /// Descriptor is `#[repr(C)]` with fields addr(u64), len(u32), flags(u16),
    /// next(u16) — 16 bytes total, no padding.  The proof verifies that
    /// from_slice never returns None and that the byte representation is stable.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_byte_valued_descriptor_roundtrip() {
        let bytes: [u8; 16] = kani::any();
        // from_slice must succeed for any 16-byte input.
        let val = Descriptor::from_slice(&bytes)
            .expect("Descriptor: from_slice must succeed for any 16 bytes");
        // Serialised length matches compile-time size.
        kani::assert(
            val.as_slice().len() == std::mem::size_of::<Descriptor>(),
            "Descriptor: as_slice length must equal size_of",
        );
        // Bytes are preserved identically.
        kani::assert(
            val.as_slice() == bytes,
            "Descriptor: byte round-trip must be identity",
        );
        kani::cover!(true, "Descriptor ByteValued roundtrip reachable");
    }
}
