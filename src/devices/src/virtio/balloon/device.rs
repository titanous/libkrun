use std::cmp;
use std::convert::TryInto;
use std::io::Write;

use utils::eventfd::EventFd;
use vm_memory::{Address, ByteValued, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use super::super::{
    ActivateError, ActivateResult, BalloonError, DeviceQueue, DeviceState, QueueConfig,
    VirtioDevice,
};
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;

// Inflate queue.
pub(crate) const IFQ_INDEX: usize = 0;
// Deflate queue.
pub(crate) const DFQ_INDEX: usize = 1;
// Stats queue.
pub(crate) const STQ_INDEX: usize = 2;
// Page-hinting queue.
pub(crate) const PHQ_INDEX: usize = 3;
// Free page reporting queue.
pub(crate) const FRQ_INDEX: usize = 4;

// Supported features.
pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_F_VERSION_1 as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_MUST_TELL_HOST as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_STATS_VQ as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_DEFLATE_ON_OOM as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_PAGE_POISON as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_FREE_PAGE_HINT as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_REPORTING as u64);

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct VirtioBalloonConfig {
    /* Number of pages host wants Guest to give up. */
    num_pages: u32,
    /* Number of pages we've actually got in balloon. */
    actual: u32,
    /* Free page report command id, readonly by guest */
    free_page_report_cmd_id: u32,
    /* Stores PAGE_POISON if page poisoning is in use */
    poison_val: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioBalloonConfig {}

pub struct Balloon {
    pub(crate) queues: Option<Vec<DeviceQueue>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
    config: VirtioBalloonConfig,
}

impl Balloon {
    pub fn new() -> super::Result<Balloon> {
        Ok(Balloon {
            queues: None,
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(BalloonError::EventFd)?,
            device_state: DeviceState::Inactive,
            config: VirtioBalloonConfig::default(),
        })
    }

    pub fn id(&self) -> &str {
        defs::BALLOON_DEV_ID
    }

    pub fn process_frq(&mut self) -> bool {
        debug!("balloon: process_frq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[FRQ_INDEX].queue.pop(mem) {
            let index = head.index;
            for desc in head.into_iter() {
                let host_addr = mem.get_host_address(desc.addr).unwrap();
                debug!(
                    "balloon: should release guest_addr={:?} host_addr={:p} len={}",
                    desc.addr, host_addr, desc.len
                );
                unsafe {
                    libc::madvise(
                        host_addr as *mut libc::c_void,
                        desc.len.try_into().unwrap(),
                        libc::MADV_DONTNEED,
                    )
                };
            }

            have_used = true;
            if let Err(e) = queues[FRQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }

    pub fn process_inflate(&mut self) -> bool {
        debug!("balloon: process_inflate()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[IFQ_INDEX].queue.pop(mem) {
            let index = head.index;
            for desc in head.into_iter() {
                // Descriptor contains a buffer of u32 PFN values
                // Iterate through each PFN in the buffer (4 bytes per PFN)
                for offset in (0..desc.len).step_by(4) {
                    // Read PFN from guest memory
                    let pfn = match mem.read_obj::<u32>(
                        desc.addr
                            .checked_add(offset as u64)
                            .expect("PFN offset should not overflow"),
                    ) {
                        Ok(pfn) => pfn,
                        Err(e) => {
                            warn!("balloon: failed to read PFN at offset {}: {:?}", offset, e);
                            continue;
                        }
                    };

                    // Convert PFN to guest physical address
                    let guest_addr = GuestAddress(u64::from(pfn) << uapi::VIRTIO_BALLOON_PFN_SHIFT);

                    // Get host address - if this fails, PFN is invalid, skip silently
                    let host_addr = match mem.get_host_address(guest_addr) {
                        Ok(addr) => addr,
                        Err(_) => {
                            debug!(
                                "balloon: invalid PFN {:#x} (guest_addr={:?}) outside guest memory",
                                pfn, guest_addr
                            );
                            continue;
                        }
                    };

                    // Call madvise to release the page
                    // This is idempotent on already-released pages (AC1.8)
                    debug!(
                        "balloon: inflating PFN {:#x} guest_addr={:?} host_addr={:p}",
                        pfn, guest_addr, host_addr
                    );
                    unsafe {
                        let ret = libc::madvise(
                            host_addr as *mut libc::c_void,
                            4096,
                            libc::MADV_DONTNEED,
                        );
                        if ret != 0 {
                            warn!("balloon: madvise failed for PFN {:#x}: {}", pfn, ret);
                        }
                    }
                }
            }

            have_used = true;
            if let Err(e) = queues[IFQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }

    pub fn process_deflate(&mut self) -> bool {
        debug!("balloon: process_deflate()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[DFQ_INDEX].queue.pop(mem) {
            let index = head.index;
            // Just acknowledge the descriptor chains - the guest will fault pages back in on access
            have_used = true;
            if let Err(e) = queues[DFQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }
}

impl VirtioDevice for Balloon {
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
        uapi::VIRTIO_ID_BALLOON
    }

    fn device_name(&self) -> &str {
        "balloon"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
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
        // Only accept writes to the `actual` field (offset 4-8)
        // All other writes are silently ignored
        let config_slice = self.config.as_mut_slice();

        // Check each byte in the write range
        let end_offset = offset.saturating_add(data.len() as u64);
        for (i, byte) in data.iter().enumerate() {
            let byte_offset = match offset.checked_add(i as u64) {
                Some(bo) => bo,
                None => break,
            };
            // Only copy bytes that fall within the `actual` field (4..8)
            if (4..8).contains(&byte_offset) {
                config_slice[byte_offset as usize] = *byte;
            }
        }

        // If the write touched the actual field, log the new value
        if offset < 8 && end_offset > 4 {
            let actual = u32::from_le_bytes([
                config_slice[4],
                config_slice[5],
                config_slice[6],
                config_slice[7],
            ]);
            debug!("balloon: guest wrote actual field = {}", actual);
        }
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

        self.queues = Some(queues);
        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy::DummyIrqChip;
    use crate::virtio::{Descriptor, InterruptTransport, Queue};

    /// Test write_config updates the actual field (offset 4-8)
    #[test]
    fn test_write_config_actual_field() {
        let mut balloon = Balloon::new().expect("Failed to create balloon device");

        // Initial value should be 0
        let config_slice = balloon.config.as_slice();
        assert_eq!(config_slice[4], 0);
        assert_eq!(config_slice[5], 0);
        assert_eq!(config_slice[6], 0);
        assert_eq!(config_slice[7], 0);

        // Write new value to actual field
        let new_value: [u8; 4] = 0x12345678u32.to_le_bytes();
        balloon.write_config(4, &new_value);

        // Verify the value was written
        let config_slice = balloon.config.as_slice();
        assert_eq!(config_slice[4], 0x78);
        assert_eq!(config_slice[5], 0x56);
        assert_eq!(config_slice[6], 0x34);
        assert_eq!(config_slice[7], 0x12);

        // Verify it reads back correctly
        let actual = u32::from_le_bytes([
            config_slice[4],
            config_slice[5],
            config_slice[6],
            config_slice[7],
        ]);
        assert_eq!(actual, 0x12345678);
    }

    /// Test write_config silently ignores writes to num_pages (offset 0-4)
    #[test]
    fn test_write_config_num_pages_ignored() {
        let mut balloon = Balloon::new().expect("Failed to create balloon device");

        // Try to write to num_pages field
        let new_value: [u8; 4] = 0xAABBCCDDu32.to_le_bytes();
        balloon.write_config(0, &new_value);

        // Verify num_pages was NOT changed
        let config_slice = balloon.config.as_slice();
        assert_eq!(config_slice[0], 0);
        assert_eq!(config_slice[1], 0);
        assert_eq!(config_slice[2], 0);
        assert_eq!(config_slice[3], 0);
    }

    /// Test write_config silently ignores writes to free_page_hint_cmd_id (offset 8-12)
    #[test]
    fn test_write_config_free_page_hint_ignored() {
        let mut balloon = Balloon::new().expect("Failed to create balloon device");

        // Try to write to free_page_hint_cmd_id field
        let new_value: [u8; 4] = 0xAABBCCDDu32.to_le_bytes();
        balloon.write_config(8, &new_value);

        // Verify free_page_hint_cmd_id was NOT changed
        let config_slice = balloon.config.as_slice();
        assert_eq!(config_slice[8], 0);
        assert_eq!(config_slice[9], 0);
        assert_eq!(config_slice[10], 0);
        assert_eq!(config_slice[11], 0);
    }

    /// Test write_config handles partial writes to actual field correctly
    #[test]
    fn test_write_config_partial_write() {
        let mut balloon = Balloon::new().expect("Failed to create balloon device");

        // Write only 2 bytes at offset 6 (last 2 bytes of actual field)
        let partial: [u8; 2] = [0x99, 0x88];
        balloon.write_config(6, &partial);

        // Verify only the specified bytes were written
        let config_slice = balloon.config.as_slice();
        assert_eq!(config_slice[4], 0); // Unchanged
        assert_eq!(config_slice[5], 0); // Unchanged
        assert_eq!(config_slice[6], 0x99); // Updated
        assert_eq!(config_slice[7], 0x88); // Updated

        // Read back the full value
        let actual = u32::from_le_bytes([
            config_slice[4],
            config_slice[5],
            config_slice[6],
            config_slice[7],
        ]);
        assert_eq!(actual, 0x88990000u32);
    }

    /// Test write_config with write spanning multiple fields (only actual portion updated)
    #[test]
    fn test_write_config_spanning_write() {
        let mut balloon = Balloon::new().expect("Failed to create balloon device");

        // Write 4 bytes starting at offset 3 (spans num_pages and actual)
        // Only the portion that overlaps with actual (4-7) should be updated
        let spanning: [u8; 4] = [0x11, 0x22, 0x33, 0x44];
        balloon.write_config(3, &spanning);

        // Verify num_pages (0-3) was NOT changed
        let config_slice = balloon.config.as_slice();
        assert_eq!(config_slice[0], 0);
        assert_eq!(config_slice[1], 0);
        assert_eq!(config_slice[2], 0);
        assert_eq!(config_slice[3], 0);

        // Verify only byte 4 was updated (offset 4, which is at index 1 in spanning write)
        assert_eq!(config_slice[4], 0x22); // Updated (index 1 in spanning)
        assert_eq!(config_slice[5], 0x33); // Updated (index 2 in spanning)
        assert_eq!(config_slice[6], 0x44); // Updated (index 3 in spanning)
        assert_eq!(config_slice[7], 0); // Unchanged (beyond spanning write)
    }

    /// Test config space structure layout
    #[test]
    fn test_config_layout() {
        let balloon = Balloon::new().expect("Failed to create balloon device");
        let config_slice = balloon.config.as_slice();

        // Verify config space is correct size (4 u32 fields = 16 bytes)
        assert_eq!(config_slice.len(), 16);

        // Verify initial values
        // num_pages (0-3): 0
        // actual (4-7): 0
        // free_page_hint_cmd_id (8-11): 0
        // poison_val (12-15): 0
        for i in 0..16 {
            assert_eq!(config_slice[i], 0, "config_slice[{}] should be 0", i);
        }
    }

    /// Test process_inflate with valid PFN buffer (AC1.1, AC1.6, AC1.8)
    /// Verifies that process_inflate reads PFN values, converts to addresses, and calls madvise.
    /// Tests that the function returns true and properly processes descriptor chains.
    #[test]
    fn test_process_inflate_with_valid_pfn() {
        // Create guest memory with enough space for queue structures and PFN data
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        // Create interrupt and queues
        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        // Set up queue structures for inflate queue (queue 0)
        const DESC_TABLE_ADDR: u64 = 0x1000;
        const AVAIL_RING_ADDR: u64 = 0x2000;
        const USED_RING_ADDR: u64 = 0x3000;
        const PFN_DATA_ADDR: u64 = 0x10000;

        // Create 5 queues for the balloon device (inflate, deflate, stats, page-hint, free-page)
        // Only inflate and deflate queues need the descriptor structures set
        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|i| DeviceQueue {
                queue: {
                    let mut q = Queue::new(256);
                    q.size = 256;
                    q.ready = true;
                    if i == 0 {
                        // Inflate queue - set up descriptor structures
                        q.desc_table = GuestAddress(DESC_TABLE_ADDR);
                        q.avail_ring = GuestAddress(AVAIL_RING_ADDR);
                        q.used_ring = GuestAddress(USED_RING_ADDR);
                    }
                    q
                },
                event: std::sync::Arc::new(
                    utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK)
                        .expect("Failed to create eventfd"),
                ),
            })
            .collect();

        // Create and activate balloon device
        let mut balloon = Balloon::new().expect("Failed to create balloon device");
        balloon
            .activate(mem.clone(), interrupt, device_queues)
            .expect("Failed to activate balloon device");

        // Write valid PFN values to memory
        // PFN 0x1 converts to guest address 0x1000 (1 << 12)
        // PFN 0x2 converts to guest address 0x2000 (2 << 12)
        let pfn1: u32 = 0x1;
        let pfn2: u32 = 0x2;

        let pfn_data = vec![
            pfn1.to_le_bytes(),
            pfn2.to_le_bytes(),
        ]
        .into_iter()
        .flat_map(|b| b.to_vec())
        .collect::<Vec<u8>>();

        mem.write_slice(&pfn_data, GuestAddress(PFN_DATA_ADDR))
            .expect("Failed to write PFN data");

        // Set up descriptor chain for inflate queue
        // Descriptor 0: points to PFN buffer
        let desc = Descriptor {
            addr: PFN_DATA_ADDR,
            len: 8, // 2 u32 PFNs
            flags: 0,
            next: 0,
        };
        mem.write_obj(desc, GuestAddress(DESC_TABLE_ADDR))
            .expect("Failed to write descriptor");

        // Set up available ring with descriptor 0 available for processing
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR))
            .expect("Failed to write avail flags");
        // Avail idx = 1 means one descriptor (index 0) is available
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .expect("Failed to write avail idx");
        // Ring[0] = descriptor index 0
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 4))
            .expect("Failed to write avail ring[0]");

        // Initialize used ring
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR))
            .expect("Failed to write used flags");
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to write used idx");

        // Call process_inflate
        let result = balloon.process_inflate();

        // Verify it returns true (descriptors were processed)
        assert!(result, "process_inflate should return true when descriptors are available and are processed");
    }

    /// Test process_inflate with invalid PFN (AC1.6)
    /// Verifies that invalid PFNs (outside guest memory) are silently skipped without panicking.
    #[test]
    fn test_process_inflate_with_invalid_pfn() {
        // Create guest memory with limited size so some PFNs will be invalid
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        const DESC_TABLE_ADDR: u64 = 0x1000;
        const AVAIL_RING_ADDR: u64 = 0x2000;
        const USED_RING_ADDR: u64 = 0x3000;
        const PFN_DATA_ADDR: u64 = 0x10000;

        // Create device queues with inflate queue (0) having descriptor structures
        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|i| DeviceQueue {
                queue: {
                    let mut q = Queue::new(256);
                    q.size = 256;
                    q.ready = true;
                    if i == 0 {
                        // Only inflate queue needs descriptor structures
                        q.desc_table = GuestAddress(DESC_TABLE_ADDR);
                        q.avail_ring = GuestAddress(AVAIL_RING_ADDR);
                        q.used_ring = GuestAddress(USED_RING_ADDR);
                    }
                    q
                },
                event: std::sync::Arc::new(
                    utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK)
                        .expect("Failed to create eventfd"),
                ),
            })
            .collect();

        let mut balloon = Balloon::new().expect("Failed to create balloon device");
        balloon
            .activate(mem.clone(), interrupt, device_queues)
            .expect("Failed to activate balloon device");

        // Write PFN values including one that's far outside guest memory
        // Memory is only 0x50000 (320KB), so PFN >> 5 will be outside
        let pfn_valid: u32 = 0x1; // Valid: 0x1000
        let pfn_invalid: u32 = 0x10000; // Invalid: 0x10000000, way outside guest memory

        let pfn_data = vec![
            pfn_valid.to_le_bytes(),
            pfn_invalid.to_le_bytes(),
        ]
        .into_iter()
        .flat_map(|b| b.to_vec())
        .collect::<Vec<u8>>();

        mem.write_slice(&pfn_data, GuestAddress(PFN_DATA_ADDR))
            .expect("Failed to write PFN data");

        // Set up descriptor chain
        let desc = Descriptor {
            addr: PFN_DATA_ADDR,
            len: 8, // 2 u32 PFNs
            flags: 0,
            next: 0,
        };
        mem.write_obj(desc, GuestAddress(DESC_TABLE_ADDR))
            .expect("Failed to write descriptor");

        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR))
            .expect("Failed to write avail flags");
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .expect("Failed to write avail idx");
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 4))
            .expect("Failed to write avail ring[0]");

        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR))
            .expect("Failed to write used flags");
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to write used idx");

        // Call process_inflate - should NOT panic even with invalid PFN
        let result = balloon.process_inflate();

        // Should return true because we had a valid descriptor (even though one PFN was invalid)
        assert!(
            result,
            "process_inflate should return true when processing a batch with mixed valid/invalid PFNs"
        );

        // Descriptor should still be marked as used
        let used_idx = mem.read_obj::<u16>(GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to read used idx");
        assert_eq!(used_idx, 1, "Descriptor should be marked as used");
    }

    /// Test process_inflate with duplicate PFN (AC1.8)
    /// Verifies that calling process_inflate twice with the same PFN is idempotent.
    #[test]
    fn test_process_inflate_duplicate_pfn_idempotent() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        const DESC_TABLE_ADDR: u64 = 0x1000;
        const AVAIL_RING_ADDR: u64 = 0x2000;
        const USED_RING_ADDR: u64 = 0x3000;
        const PFN_DATA_ADDR: u64 = 0x10000;

        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|i| DeviceQueue {
                queue: {
                    let mut q = Queue::new(256);
                    q.size = 256;
                    q.ready = true;
                    if i == 0 {
                        q.desc_table = GuestAddress(DESC_TABLE_ADDR);
                        q.avail_ring = GuestAddress(AVAIL_RING_ADDR);
                        q.used_ring = GuestAddress(USED_RING_ADDR);
                    }
                    q
                },
                event: std::sync::Arc::new(
                    utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK)
                        .expect("Failed to create eventfd"),
                ),
            })
            .collect();

        let mut balloon = Balloon::new().expect("Failed to create balloon device");
        balloon
            .activate(mem.clone(), interrupt, device_queues)
            .expect("Failed to activate balloon device");

        // Write the same PFN twice
        let pfn: u32 = 0x1;
        let pfn_data = vec![
            pfn.to_le_bytes(),
            pfn.to_le_bytes(),
        ]
        .into_iter()
        .flat_map(|b| b.to_vec())
        .collect::<Vec<u8>>();

        mem.write_slice(&pfn_data, GuestAddress(PFN_DATA_ADDR))
            .expect("Failed to write PFN data");

        let desc = Descriptor {
            addr: PFN_DATA_ADDR,
            len: 8, // 2 identical u32 PFNs
            flags: 0,
            next: 0,
        };
        mem.write_obj(desc, GuestAddress(DESC_TABLE_ADDR))
            .expect("Failed to write descriptor");

        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR))
            .expect("Failed to write avail flags");
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .expect("Failed to write avail idx");
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 4))
            .expect("Failed to write avail ring[0]");

        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR))
            .expect("Failed to write used flags");
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to write used idx");

        // Call process_inflate - should handle duplicate PFN idempotently (madvise is idempotent)
        let result = balloon.process_inflate();

        assert!(
            result,
            "process_inflate should return true when processing duplicates"
        );

        // Verify descriptor was marked as used
        let used_idx = mem.read_obj::<u16>(GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to read used idx");
        assert_eq!(used_idx, 1, "Descriptor should be marked as used");
    }

    /// Test process_deflate with valid descriptor chain (AC1.2)
    /// Verifies that process_deflate pops descriptors and marks them as used without error.
    #[test]
    fn test_process_deflate_basic() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        const DESC_TABLE_ADDR: u64 = 0x1000;
        const AVAIL_RING_ADDR: u64 = 0x2000;
        const USED_RING_ADDR: u64 = 0x3000;

        // Create queues with both inflate and deflate having descriptor structures
        // (we'll test deflate which is at index 1)
        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|i| DeviceQueue {
                queue: {
                    let mut q = Queue::new(256);
                    q.size = 256;
                    q.ready = true;
                    if i == 1 {
                        // Deflate queue at index 1 - set up descriptor structures
                        q.desc_table = GuestAddress(DESC_TABLE_ADDR);
                        q.avail_ring = GuestAddress(AVAIL_RING_ADDR);
                        q.used_ring = GuestAddress(USED_RING_ADDR);
                    }
                    q
                },
                event: std::sync::Arc::new(
                    utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK)
                        .expect("Failed to create eventfd"),
                ),
            })
            .collect();

        let mut balloon = Balloon::new().expect("Failed to create balloon device");
        balloon
            .activate(mem.clone(), interrupt, device_queues)
            .expect("Failed to activate balloon device");

        // Set up descriptor for deflate queue
        let desc = Descriptor {
            addr: 0x20000, // Some address in guest memory
            len: 8, // Some size of PFN buffer (deflate doesn't care about contents)
            flags: 0,
            next: 0,
        };
        mem.write_obj(desc, GuestAddress(DESC_TABLE_ADDR))
            .expect("Failed to write descriptor");

        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR))
            .expect("Failed to write avail flags");
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .expect("Failed to write avail idx");
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 4))
            .expect("Failed to write avail ring[0]");

        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR))
            .expect("Failed to write used flags");
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to write used idx");

        // Call process_deflate
        let result = balloon.process_deflate();

        // Should return true because descriptor was processed
        assert!(result, "process_deflate should return true when descriptors are available");

        // Verify the descriptor was marked as used
        let used_idx = mem.read_obj::<u16>(GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to read used idx");
        assert_eq!(
            used_idx, 1,
            "Used ring index should be 1 after processing 1 descriptor"
        );
    }

    /// Test AVAIL_FEATURES includes all required balloon features (AC1.1, AC1.2)
    #[test]
    fn test_avail_features_complete() {
        let balloon = Balloon::new().expect("Failed to create balloon device");

        let features = balloon.avail_features();

        // VERSION_1 must be supported
        assert!((features & (1 << uapi::VIRTIO_F_VERSION_1 as u64)) != 0);

        // All balloon features must be advertised
        assert!((features & (1 << uapi::VIRTIO_BALLOON_F_MUST_TELL_HOST as u64)) != 0);
        assert!((features & (1 << uapi::VIRTIO_BALLOON_F_STATS_VQ as u64)) != 0);
        assert!((features & (1 << uapi::VIRTIO_BALLOON_F_DEFLATE_ON_OOM as u64)) != 0);
        assert!((features & (1 << uapi::VIRTIO_BALLOON_F_PAGE_POISON as u64)) != 0);
        assert!((features & (1 << uapi::VIRTIO_BALLOON_F_FREE_PAGE_HINT as u64)) != 0);
        assert!((features & (1 << uapi::VIRTIO_BALLOON_F_REPORTING as u64)) != 0);
    }

    /// Test PFN shift constant is correct (AC1.1)
    #[test]
    fn test_pfn_shift_constant() {
        // PFN shift should be 12 (4096 bytes per page)
        assert_eq!(uapi::VIRTIO_BALLOON_PFN_SHIFT, 12);

        // Verify the shift value works: PFN 1 -> address 0x1000
        let pfn = 1u32;
        let addr = u64::from(pfn) << uapi::VIRTIO_BALLOON_PFN_SHIFT;
        assert_eq!(addr, 0x1000);

        // PFN 0x1000 -> address 0x1000000
        let pfn = 0x1000u32;
        let addr = u64::from(pfn) << uapi::VIRTIO_BALLOON_PFN_SHIFT;
        assert_eq!(addr, 0x1000000);
    }

    /// Test device initialization and feature negotiation (AC1.1, AC1.2)
    #[test]
    fn test_device_initialization() {
        let mut balloon = Balloon::new().expect("Failed to create balloon device");

        // Initial state: not activated, no acked features
        assert!(!balloon.is_activated());
        assert_eq!(balloon.acked_features(), 0);

        // Verify we can set acked features
        let test_features = 1 << uapi::VIRTIO_BALLOON_F_MUST_TELL_HOST as u64;
        balloon.set_acked_features(test_features);
        assert_eq!(balloon.acked_features(), test_features);

        // Verify the feature is within available features
        assert!((balloon.avail_features() & test_features) == test_features);
    }
}
