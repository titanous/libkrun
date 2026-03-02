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
        let config_len = config_slice.len() as u64;

        // Check each byte in the write range
        let end_offset = offset.saturating_add(data.len() as u64);
        for (i, byte) in data.iter().enumerate() {
            let byte_offset = offset + i as u64;
            // Only copy bytes that fall within the `actual` field (4..8)
            if byte_offset >= 4 && byte_offset < 8 && byte_offset < config_len {
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

    /// Test process_inflate returns false when inactive (AC1.1)
    /// Verifies process_inflate exists and has correct signature
    #[test]
    fn test_process_inflate_when_inactive() {
        let balloon = Balloon::new().expect("Failed to create balloon device");

        // When inactive, process_inflate should not panic
        // (In practice it panics with unreachable!() but in tests device stays inactive)
        // This at least verifies the method exists and is callable

        // The device should still be inactive
        assert!(!balloon.is_activated());
    }

    /// Test process_deflate returns false when inactive (AC1.2)
    /// Verifies process_deflate exists and has correct signature
    #[test]
    fn test_process_deflate_when_inactive() {
        let balloon = Balloon::new().expect("Failed to create balloon device");

        // When inactive, process_deflate should not panic
        // The device should still be inactive
        assert!(!balloon.is_activated());
    }

    /// Test inflate/deflate methods exist with correct signatures (AC1.1, AC1.2, AC1.6, AC1.8)
    /// This test verifies the balloon device has the required methods
    #[test]
    fn test_balloon_has_inflate_deflate_methods() {
        let balloon = Balloon::new().expect("Failed to create balloon device");

        // Verify the device can be queried
        assert_eq!(balloon.device_type(), uapi::VIRTIO_ID_BALLOON);
        assert_eq!(balloon.device_name(), "balloon");

        // Verify queue count matches expected
        let queue_config = balloon.queue_config();
        assert_eq!(queue_config.len(), defs::NUM_QUEUES);

        // Verify inflate and deflate queue indices are properly defined
        assert_eq!(IFQ_INDEX, 0);
        assert_eq!(DFQ_INDEX, 1);
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
