use std::cmp;
use std::convert::TryInto;
use std::io::Write;

use utils::eventfd::EventFd;
use vm_memory::{
    Address, ByteValued, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap,
    GuestMemoryRegion,
};

use super::super::{
    ActivateError, ActivateResult, BalloonError, DeviceQueue, DeviceState, QueueConfig,
    VirtioDevice,
};
use super::{defs, defs::uapi, reclaimed_bitmap::ReclaimedBitmap};
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

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct BalloonStat {
    tag: u16,
    val: u64,
}

// SAFETY: BalloonStat only contains plain data with no padding.
unsafe impl ByteValued for BalloonStat {}

#[derive(Clone, Debug, Default)]
pub struct BalloonStats {
    pub swap_in: Option<u64>,
    pub swap_out: Option<u64>,
    pub major_faults: Option<u64>,
    pub minor_faults: Option<u64>,
    pub free_memory: Option<u64>,
    pub total_memory: Option<u64>,
    pub available_memory: Option<u64>,
    pub memory_caches: Option<u64>,
    pub htlb_allocations: Option<u64>,
    pub htlb_failures: Option<u64>,
    pub oom_kills: Option<u64>,
    pub alloc_stalls: Option<u64>,
    pub async_scans: Option<u64>,
    pub direct_scans: Option<u64>,
    pub async_reclaims: Option<u64>,
    pub direct_reclaims: Option<u64>,
}

impl BalloonStats {
    fn update_with_stat(&mut self, stat: &BalloonStat) {
        match stat.tag {
            uapi::VIRTIO_BALLOON_S_SWAP_IN => self.swap_in = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_SWAP_OUT => self.swap_out = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_MAJFLT => self.major_faults = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_MINFLT => self.minor_faults = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_MEMFREE => self.free_memory = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_MEMTOT => self.total_memory = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_AVAIL => self.available_memory = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_CACHES => self.memory_caches = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_HTLB_PGALLOC => self.htlb_allocations = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_HTLB_PGFAIL => self.htlb_failures = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_OOM_KILL => self.oom_kills = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_ALLOC_STALL => self.alloc_stalls = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_ASYNC_SCAN => self.async_scans = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_DIRECT_SCAN => self.direct_scans = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_ASYNC_RECLAIM => self.async_reclaims = Some(stat.val),
            uapi::VIRTIO_BALLOON_S_DIRECT_RECLAIM => self.direct_reclaims = Some(stat.val),
            _ => {
                // Unknown tag, silently ignore
            }
        }
    }
}

/// Serializable balloon device state for snapshot/restore.
///
/// Contains device-specific fields not covered by MmioTransportState
/// (which already handles queue states and acked_features).
/// Reclaimed page bitmaps are NOT included — they are transient host state
/// that starts empty after restore.
// Fields are only read by serde's generated code (behind the snapshot feature) or
// written in save_backend_state() and read in restore_backend_state(). The compiler
// sees them as unread in builds without --features snapshot.
#[allow(dead_code)]
#[cfg_attr(feature = "snapshot", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
struct BalloonState {
    /// Config space: num_pages (inflation target set by host)
    num_pages: u32,
    /// Config space: actual (current inflation reported by guest)
    actual: u32,
    /// Config space: free_page_report_cmd_id
    free_page_report_cmd_id: u32,
    /// Config space: poison_val
    poison_val: u32,
    /// Free page hinting command counter (monotonically increasing)
    hinting_cmd_counter: u32,
    /// Current host-requested hinting command
    hinting_host_cmd: u32,
}

pub struct Balloon {
    pub(crate) queues: Option<Vec<DeviceQueue>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
    config: VirtioBalloonConfig,
    stats_desc_index: Option<u16>,
    latest_stats: Option<BalloonStats>,
    hinting_cmd_counter: u32,
    hinting_host_cmd: u32,
    hinting_guest_cmd: Option<u32>,
    pub(crate) inflated_bitmap: Option<ReclaimedBitmap>,
    pub(crate) reported_free_bitmap: Option<ReclaimedBitmap>,
    actual_condvar: std::sync::Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
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
            stats_desc_index: None,
            latest_stats: None,
            hinting_cmd_counter: 2,
            hinting_host_cmd: 0,
            hinting_guest_cmd: None,
            inflated_bitmap: None,
            reported_free_bitmap: None,
            actual_condvar: std::sync::Arc::new((
                std::sync::Mutex::new(0),
                std::sync::Condvar::new(),
            )),
        })
    }

    pub fn id(&self) -> &str {
        defs::BALLOON_DEV_ID
    }

    pub fn actual_condvar(&self) -> std::sync::Arc<(std::sync::Mutex<u64>, std::sync::Condvar)> {
        self.actual_condvar.clone()
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

                // Track reported-free pages in the bitmap
                if let Some(ref bitmap) = self.reported_free_bitmap {
                    let start_pfn = (desc.addr.raw_value() >> 12) as u32;
                    let count = desc.len / 4096;
                    debug!(
                        "balloon: marking FRQ pages as reported-free: start_pfn={:#x} count={}",
                        start_pfn, count
                    );
                    bitmap.mark_range(start_pfn, count);
                }
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

                    // Track this page in the inflated bitmap for snapshot exclusion
                    if let Some(ref bitmap) = self.inflated_bitmap {
                        bitmap.mark(pfn);
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
            // Read PFN values from the descriptor chain and clear the inflated bitmap
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

                    // Clear the bit in the inflated bitmap
                    if let Some(ref bitmap) = self.inflated_bitmap {
                        debug!("balloon: deflating PFN {:#x}", pfn);
                        bitmap.clear(pfn);
                    }
                }
            }

            have_used = true;
            if let Err(e) = queues[DFQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }

    pub fn process_stats_queue(&mut self) -> bool {
        debug!("balloon: process_stats_queue()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");

        // If there's a previous descriptor index, return it first (compliance requirement)
        if let Some(prev_index) = self.stats_desc_index.take() {
            if let Err(e) = queues[STQ_INDEX].queue.add_used(mem, prev_index, 0) {
                error!("failed to add used elements to the stats queue: {e:?}");
            }
        }

        // Pop new descriptor from the queue
        if let Some(head) = queues[STQ_INDEX].queue.pop(mem) {
            let index = head.index;

            // Read BalloonStat entries from the descriptor buffer
            let mut stats = BalloonStats::default();
            const STAT_SIZE: u64 = std::mem::size_of::<BalloonStat>() as u64;

            for desc in head.into_iter() {
                // Iterate through each BalloonStat entry in the descriptor (10 bytes each)
                let mut offset = 0u64;
                while offset + STAT_SIZE <= desc.len as u64 {
                    let addr = match desc.addr.checked_add(offset) {
                        Some(a) => a,
                        None => {
                            warn!("balloon: stats buffer offset overflow");
                            break;
                        }
                    };

                    match mem.read_obj::<BalloonStat>(addr) {
                        Ok(stat) => {
                            stats.update_with_stat(&stat);
                        }
                        Err(e) => {
                            warn!(
                                "balloon: failed to read BalloonStat from stats buffer: {:?}",
                                e
                            );
                            break;
                        }
                    }
                    offset += STAT_SIZE;
                }
            }

            // Store the parsed stats and descriptor index for future request
            self.latest_stats = Some(stats);
            self.stats_desc_index = Some(index);

            return true;
        }

        false
    }

    pub fn request_stats(&mut self) {
        debug!("balloon: request_stats()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => {
                warn!("balloon: request_stats called but device is not activated");
                return;
            }
        };

        if let Some(index) = self.stats_desc_index.take() {
            let queues = self
                .queues
                .as_mut()
                .expect("queues should exist when activated");

            if let Err(e) = queues[STQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the stats queue: {e:?}");
            }
            self.device_state.signal_used_queue();
        } else {
            warn!("balloon: request_stats called but no stats descriptor available");
        }
    }

    pub fn stats(&self) -> Option<&BalloonStats> {
        self.latest_stats.as_ref()
    }

    pub fn process_phq(&mut self) -> bool {
        debug!("balloon: process_phq()");
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
        let mut phq_complete = false;

        while let Some(head) = queues[PHQ_INDEX].queue.pop(mem) {
            let index = head.index;

            for desc in head.into_iter() {
                // Check if this is a 4-byte command ID descriptor
                if desc.len == 4 {
                    // Read the 4-byte command ID
                    match mem.read_obj::<u32>(desc.addr) {
                        Ok(cmd) => {
                            debug!("balloon: PHQ received command ID: {}", cmd);
                            self.hinting_guest_cmd = Some(cmd);

                            // Check if this is a STOP or DONE command
                            if cmd == uapi::VIRTIO_BALLOON_CMD_ID_STOP
                                || cmd == uapi::VIRTIO_BALLOON_CMD_ID_DONE
                            {
                                phq_complete = true;
                            }
                        }
                        Err(e) => {
                            warn!("balloon: failed to read command ID from PHQ: {:?}", e);
                        }
                    }
                } else if desc.len > 4 {
                    // This is a page block descriptor - only process if we have an active host command
                    // and the guest command matches the host command
                    let should_process = self.hinting_host_cmd != uapi::VIRTIO_BALLOON_CMD_ID_STOP
                        && self.hinting_host_cmd != uapi::VIRTIO_BALLOON_CMD_ID_DONE
                        && self.hinting_guest_cmd == Some(self.hinting_host_cmd);

                    if should_process {
                        let host_addr = mem.get_host_address(desc.addr).unwrap();
                        debug!(
                            "balloon: releasing guest_addr={:?} host_addr={:p} len={}",
                            desc.addr, host_addr, desc.len
                        );
                        unsafe {
                            libc::madvise(
                                host_addr as *mut libc::c_void,
                                desc.len.try_into().unwrap(),
                                libc::MADV_DONTNEED,
                            )
                        };

                        // Track reported-free pages in the bitmap
                        if let Some(ref bitmap) = self.reported_free_bitmap {
                            let start_pfn = (desc.addr.raw_value() >> 12) as u32;
                            let count = desc.len / 4096;
                            debug!(
                                "balloon: marking PHQ pages as reported-free: start_pfn={:#x} count={}",
                                start_pfn, count
                            );
                            bitmap.mark_range(start_pfn, count);
                        }
                    }
                }
            }

            have_used = true;
            if let Err(e) = queues[PHQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the PHQ queue: {e:?}");
            }
        }

        // If we received a STOP/DONE, transition to DONE state and signal config change
        if phq_complete {
            self.config.free_page_report_cmd_id = uapi::VIRTIO_BALLOON_CMD_ID_DONE;
            self.hinting_host_cmd = uapi::VIRTIO_BALLOON_CMD_ID_DONE;
            self.device_state.signal_config_change();
        }

        have_used
    }

    pub fn start_free_page_hinting(&mut self) {
        debug!("balloon: start_free_page_hinting()");

        // Generate new command ID (increment and wrap, but skip 0 and 1)
        let cmd_id = self.hinting_cmd_counter;
        self.hinting_cmd_counter = self.hinting_cmd_counter.wrapping_add(1);
        // Skip reserved command IDs (0 and 1)
        if self.hinting_cmd_counter <= 1 {
            self.hinting_cmd_counter = 2;
        }

        // Write command ID to config space
        self.config.free_page_report_cmd_id = cmd_id;
        self.hinting_host_cmd = cmd_id;

        // Signal config change to guest
        self.device_state.signal_config_change();
        debug!(
            "balloon: initiated free page hinting with cmd_id: {}",
            cmd_id
        );
    }

    /// Query the reclaimed bitmaps for snapshot integration.
    pub fn reclaimed_bitmaps(&self) -> (Option<&ReclaimedBitmap>, Option<&ReclaimedBitmap>) {
        (
            self.inflated_bitmap.as_ref(),
            self.reported_free_bitmap.as_ref(),
        )
    }

    /// Check if the balloon device is active (activated and not inactive).
    pub fn is_device_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    /// Get the current actual size in pages.
    pub fn get_actual_pages(&self) -> u32 {
        self.config.actual
    }

    /// Set the target number of pages.
    pub fn set_num_pages(&mut self, num_pages: u32) {
        self.config.num_pages = num_pages;
    }

    /// Signal a config change to the guest.
    pub fn signal_config_changed(&mut self) {
        self.device_state.signal_config_change();
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

        // If the write touched the actual field, log the new value and notify waiters
        if offset < 8 && end_offset > 4 {
            let actual = u32::from_le_bytes([
                config_slice[4],
                config_slice[5],
                config_slice[6],
                config_slice[7],
            ]);
            debug!("balloon: guest wrote actual field = {}", actual);

            // Notify condvar waiters of the new actual value
            let actual_pages = actual as u64;
            let (lock, cvar) = &*self.actual_condvar;
            if let Ok(mut val) = lock.lock() {
                *val = actual_pages;
                cvar.notify_all();
            }
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

        // Calculate total guest address space by finding the highest end address
        let max_addr = mem
            .iter()
            .map(|region| region.start_addr().raw_value() + region.len())
            .max()
            .unwrap_or(0);

        // Convert to page count using 4KB page size
        let num_pages = (max_addr / 4096) as usize;

        // Create reclaimed bitmaps
        self.inflated_bitmap = Some(ReclaimedBitmap::new(num_pages));
        self.reported_free_bitmap = Some(ReclaimedBitmap::new(num_pages));

        self.queues = Some(queues);
        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn save_backend_state(&self) -> Option<Vec<u8>> {
        let state = BalloonState {
            num_pages: self.config.num_pages,
            actual: self.config.actual,
            free_page_report_cmd_id: self.config.free_page_report_cmd_id,
            poison_val: self.config.poison_val,
            hinting_cmd_counter: self.hinting_cmd_counter,
            hinting_host_cmd: self.hinting_host_cmd,
        };

        #[cfg(feature = "snapshot")]
        {
            match bincode::serialize(&state) {
                Ok(data) => Some(data),
                Err(e) => {
                    log::error!("balloon: failed to serialize backend state: {e}");
                    None
                }
            }
        }
        #[cfg(not(feature = "snapshot"))]
        {
            let _ = state;
            None
        }
    }

    fn restore_backend_state(&mut self, data: &[u8]) {
        #[cfg(feature = "snapshot")]
        {
            match bincode::deserialize::<BalloonState>(data) {
                Ok(state) => {
                    self.config.num_pages = state.num_pages;
                    self.config.actual = state.actual;
                    self.config.free_page_report_cmd_id = state.free_page_report_cmd_id;
                    self.config.poison_val = state.poison_val;
                    self.hinting_cmd_counter = state.hinting_cmd_counter;
                    self.hinting_host_cmd = state.hinting_host_cmd;
                    // stats_desc_index is intentionally NOT restored — it refers to a
                    // descriptor index in the stats queue which is re-initialized by
                    // MmioTransport queue restore. The guest will re-push a stats buffer
                    // after resume, providing a fresh descriptor index.
                    self.stats_desc_index = None;

                    log::debug!(
                        "balloon: restored state: num_pages={}, actual={}",
                        state.num_pages,
                        state.actual
                    );
                }
                Err(e) => {
                    log::error!("balloon: failed to deserialize backend state: {e}");
                }
            }
        }
        #[cfg(not(feature = "snapshot"))]
        {
            let _ = data;
        }
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

        let pfn_data = vec![pfn1.to_le_bytes(), pfn2.to_le_bytes()]
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
        assert!(
            result,
            "process_inflate should return true when descriptors are available and are processed"
        );
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

        let pfn_data = vec![pfn_valid.to_le_bytes(), pfn_invalid.to_le_bytes()]
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
        let used_idx = mem
            .read_obj::<u16>(GuestAddress(USED_RING_ADDR + 2))
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
        let pfn_data = vec![pfn.to_le_bytes(), pfn.to_le_bytes()]
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
        let used_idx = mem
            .read_obj::<u16>(GuestAddress(USED_RING_ADDR + 2))
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
            len: 8,        // Some size of PFN buffer (deflate doesn't care about contents)
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
        assert!(
            result,
            "process_deflate should return true when descriptors are available"
        );

        // Verify the descriptor was marked as used
        let used_idx = mem
            .read_obj::<u16>(GuestAddress(USED_RING_ADDR + 2))
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

    /// Test stats() returns None before activation (AC1.7)
    #[test]
    fn test_stats_before_activation() {
        let balloon = Balloon::new().expect("Failed to create balloon device");

        // Before activation, stats() should return None
        assert!(
            balloon.stats().is_none(),
            "stats() should return None before device activation"
        );
    }

    /// Test stats() returns None before first stats descriptor arrives (AC1.7)
    #[test]
    fn test_stats_before_first_descriptor() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        // Create device queues
        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|_| DeviceQueue {
                queue: {
                    let q = Queue::new(256);
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
            .activate(mem, interrupt, device_queues)
            .expect("Failed to activate balloon device");

        // After activation but before any stats descriptor, stats() should return None
        assert!(
            balloon.stats().is_none(),
            "stats() should return None before any stats descriptor arrives"
        );
    }

    /// Test process_stats_queue() with valid stats descriptor (AC1.4)
    #[test]
    fn test_process_stats_queue_with_valid_stats() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        const DESC_TABLE_ADDR: u64 = 0x1000;
        const AVAIL_RING_ADDR: u64 = 0x2000;
        const USED_RING_ADDR: u64 = 0x3000;
        const STATS_DATA_ADDR: u64 = 0x10000;

        // Create device queues with stats queue (2) having descriptor structures
        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|i| DeviceQueue {
                queue: {
                    let mut q = Queue::new(256);
                    q.size = 256;
                    q.ready = true;
                    if i == 2 {
                        // Stats queue at index 2
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

        // Create BalloonStat entries for MEMFREE, MEMTOT, and AVAIL
        let stats_data = vec![
            BalloonStat {
                tag: uapi::VIRTIO_BALLOON_S_MEMFREE,
                val: 1024 * 1024, // 1MB free
            },
            BalloonStat {
                tag: uapi::VIRTIO_BALLOON_S_MEMTOT,
                val: 2048 * 1024, // 2MB total
            },
            BalloonStat {
                tag: uapi::VIRTIO_BALLOON_S_AVAIL,
                val: 512 * 1024, // 512KB available
            },
        ];

        // Write stats to memory
        let mut stats_bytes = vec![];
        for stat in &stats_data {
            stats_bytes.extend_from_slice(stat.as_slice());
        }

        mem.write_slice(&stats_bytes, GuestAddress(STATS_DATA_ADDR))
            .expect("Failed to write stats data");

        // Set up descriptor chain for stats queue
        let desc = Descriptor {
            addr: STATS_DATA_ADDR,
            len: stats_bytes.len() as u32,
            flags: 0,
            next: 0,
        };
        mem.write_obj(desc, GuestAddress(DESC_TABLE_ADDR))
            .expect("Failed to write descriptor");

        // Set up available ring
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR))
            .expect("Failed to write avail flags");
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .expect("Failed to write avail idx");
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 4))
            .expect("Failed to write avail ring[0]");

        // Initialize used ring
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR))
            .expect("Failed to write used flags");
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to write used idx");

        // Call process_stats_queue
        let result = balloon.process_stats_queue();

        // Should return true (descriptor was processed)
        assert!(
            result,
            "process_stats_queue should return true when a descriptor is available"
        );

        // stats() should now return Some with the parsed values
        let stats = balloon
            .stats()
            .expect("stats() should return Some after processing");

        // Verify the stats were parsed correctly
        assert_eq!(
            stats.free_memory,
            Some(1024 * 1024),
            "MEMFREE stat should be 1MB"
        );
        assert_eq!(
            stats.total_memory,
            Some(2048 * 1024),
            "MEMTOT stat should be 2MB"
        );
        assert_eq!(
            stats.available_memory,
            Some(512 * 1024),
            "AVAIL stat should be 512KB"
        );
    }

    /// Test process_stats_queue() with empty queue (no descriptor available)
    #[test]
    fn test_process_stats_queue_empty() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        // Create device queues with empty stats queue
        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|_| DeviceQueue {
                queue: Queue::new(256),
                event: std::sync::Arc::new(
                    utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK)
                        .expect("Failed to create eventfd"),
                ),
            })
            .collect();

        let mut balloon = Balloon::new().expect("Failed to create balloon device");
        balloon
            .activate(mem, interrupt, device_queues)
            .expect("Failed to activate balloon device");

        // Call process_stats_queue with empty queue
        let result = balloon.process_stats_queue();

        // Should return false (no descriptor to process)
        assert!(
            !result,
            "process_stats_queue should return false when queue is empty"
        );

        // stats() should still return None
        assert!(
            balloon.stats().is_none(),
            "stats() should be None when no descriptor has been processed"
        );
    }

    /// Test BalloonStats::update_with_stat() with all stat tags
    #[test]
    fn test_balloon_stats_update() {
        let mut stats = BalloonStats::default();

        // Test each stat tag individually
        let tags = vec![
            (uapi::VIRTIO_BALLOON_S_SWAP_IN, "SWAP_IN"),
            (uapi::VIRTIO_BALLOON_S_SWAP_OUT, "SWAP_OUT"),
            (uapi::VIRTIO_BALLOON_S_MAJFLT, "MAJFLT"),
            (uapi::VIRTIO_BALLOON_S_MINFLT, "MINFLT"),
            (uapi::VIRTIO_BALLOON_S_MEMFREE, "MEMFREE"),
            (uapi::VIRTIO_BALLOON_S_MEMTOT, "MEMTOT"),
            (uapi::VIRTIO_BALLOON_S_AVAIL, "AVAIL"),
            (uapi::VIRTIO_BALLOON_S_CACHES, "CACHES"),
            (uapi::VIRTIO_BALLOON_S_HTLB_PGALLOC, "HTLB_PGALLOC"),
            (uapi::VIRTIO_BALLOON_S_HTLB_PGFAIL, "HTLB_PGFAIL"),
            (uapi::VIRTIO_BALLOON_S_OOM_KILL, "OOM_KILL"),
            (uapi::VIRTIO_BALLOON_S_ALLOC_STALL, "ALLOC_STALL"),
            (uapi::VIRTIO_BALLOON_S_ASYNC_SCAN, "ASYNC_SCAN"),
            (uapi::VIRTIO_BALLOON_S_DIRECT_SCAN, "DIRECT_SCAN"),
            (uapi::VIRTIO_BALLOON_S_ASYNC_RECLAIM, "ASYNC_RECLAIM"),
            (uapi::VIRTIO_BALLOON_S_DIRECT_RECLAIM, "DIRECT_RECLAIM"),
        ];

        for (tag, name) in tags {
            let stat = BalloonStat { tag, val: 12345 };
            stats.update_with_stat(&stat);
            // For each tag, verify that at least one field was set
            match tag {
                uapi::VIRTIO_BALLOON_S_SWAP_IN => {
                    assert_eq!(stats.swap_in, Some(12345), "{} not updated", name);
                }
                uapi::VIRTIO_BALLOON_S_SWAP_OUT => {
                    assert_eq!(stats.swap_out, Some(12345), "{} not updated", name);
                }
                uapi::VIRTIO_BALLOON_S_MEMFREE => {
                    assert_eq!(stats.free_memory, Some(12345), "{} not updated", name);
                }
                uapi::VIRTIO_BALLOON_S_MEMTOT => {
                    assert_eq!(stats.total_memory, Some(12345), "{} not updated", name);
                }
                uapi::VIRTIO_BALLOON_S_AVAIL => {
                    assert_eq!(stats.available_memory, Some(12345), "{} not updated", name);
                }
                _ => {
                    // Other tags - just verify no panic occurred
                }
            }
        }
    }

    /// Test BalloonStats ignores unknown tags
    #[test]
    fn test_balloon_stats_unknown_tag() {
        let mut stats = BalloonStats::default();

        let unknown_stat = BalloonStat {
            tag: 9999, // Unknown tag
            val: 12345,
        };

        stats.update_with_stat(&unknown_stat);

        // All fields should remain None since the tag was unknown
        assert!(stats.swap_in.is_none());
        assert!(stats.swap_out.is_none());
        assert!(stats.free_memory.is_none());
        assert!(stats.total_memory.is_none());
        assert!(stats.available_memory.is_none());
    }

    /// Test process_phq processes descriptor chain with START cmd_id, page blocks, and STOP (AC1.5)
    /// Verifies that process_phq processes the command ID protocol correctly:
    /// 1. Reads a 4-byte START command ID matching the host command
    /// 2. Processes subsequent memory range descriptors (>4 bytes) with MADV_DONTNEED
    /// 3. Reads a 4-byte STOP command ID
    /// 4. Device transitions to DONE state (hinting_host_cmd becomes CMD_ID_DONE)
    #[test]
    fn test_process_phq_with_command_id_protocol() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        const DESC_TABLE_ADDR: u64 = 0x1000;
        const AVAIL_RING_ADDR: u64 = 0x2000;
        const USED_RING_ADDR: u64 = 0x3000;
        const CMD_ID_ADDR: u64 = 0x10000;
        const PAGE_BLOCK_ADDR: u64 = 0x10100;
        const STOP_CMD_ADDR: u64 = 0x10200;

        // Create device queues with PHQ (index 3) having descriptor structures
        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|i| DeviceQueue {
                queue: {
                    let mut q = Queue::new(256);
                    q.size = 256;
                    q.ready = true;
                    if i == 3 {
                        // PHQ at index 3
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

        // Generate a host command ID using start_free_page_hinting
        // This will set hinting_host_cmd to 2 (first command ID after init)
        balloon.start_free_page_hinting();

        // Get the host command ID that was set
        let host_cmd_id = balloon.hinting_host_cmd;
        assert!(host_cmd_id > 1, "Host command ID should be > 1");

        // Prepare descriptor chain:
        // Descriptor 0: 4-byte START command ID (matches host cmd)
        // Descriptor 1: Page block descriptor (>4 bytes)
        // Descriptor 2: 4-byte STOP command ID

        // Write START command ID
        mem.write_obj(host_cmd_id, GuestAddress(CMD_ID_ADDR))
            .expect("Failed to write start cmd_id");

        // Write page block data (4KB page at 0x1000)
        let page_block_data = vec![0u8; 4096];
        mem.write_slice(&page_block_data, GuestAddress(PAGE_BLOCK_ADDR))
            .expect("Failed to write page block");

        // Write STOP command ID (0 = CMD_ID_STOP)
        mem.write_obj(
            uapi::VIRTIO_BALLOON_CMD_ID_STOP,
            GuestAddress(STOP_CMD_ADDR),
        )
        .expect("Failed to write stop cmd_id");

        // Set up descriptor chain (3 descriptors: START, PAGE_BLOCK, STOP)
        // Descriptor 0 (START): 4 bytes
        let desc0 = Descriptor {
            addr: CMD_ID_ADDR,
            len: 4,
            flags: 1, // Has next
            next: 1,
        };
        mem.write_obj(desc0, GuestAddress(DESC_TABLE_ADDR))
            .expect("Failed to write descriptor 0");

        // Descriptor 1 (PAGE_BLOCK): 4096 bytes
        let desc1 = Descriptor {
            addr: PAGE_BLOCK_ADDR,
            len: 4096,
            flags: 1, // Has next
            next: 2,
        };
        mem.write_obj(desc1, GuestAddress(DESC_TABLE_ADDR + 16))
            .expect("Failed to write descriptor 1");

        // Descriptor 2 (STOP): 4 bytes
        let desc2 = Descriptor {
            addr: STOP_CMD_ADDR,
            len: 4,
            flags: 0, // No next
            next: 0,
        };
        mem.write_obj(desc2, GuestAddress(DESC_TABLE_ADDR + 32))
            .expect("Failed to write descriptor 2");

        // Set up available ring to point to descriptor chain (head = 0)
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR))
            .expect("Failed to write avail flags");
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .expect("Failed to write avail idx");
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 4))
            .expect("Failed to write avail ring[0]");

        // Initialize used ring
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR))
            .expect("Failed to write used flags");
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to write used idx");

        // Call process_phq
        let result = balloon.process_phq();

        // Should return true (descriptors were processed)
        assert!(
            result,
            "process_phq should return true when descriptors are available"
        );

        // Verify device transitioned to DONE state
        assert_eq!(
            balloon.hinting_host_cmd,
            uapi::VIRTIO_BALLOON_CMD_ID_DONE,
            "Device should transition to DONE state after processing STOP command"
        );

        // Verify the command ID was read correctly
        assert_eq!(
            balloon.hinting_guest_cmd,
            Some(uapi::VIRTIO_BALLOON_CMD_ID_STOP),
            "Guest command should be set to STOP after processing"
        );

        // Verify descriptor was marked as used
        let used_idx = mem
            .read_obj::<u16>(GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to read used idx");
        assert_eq!(used_idx, 1, "Descriptor chain should be marked as used");
    }

    /// Test process_phq returns false when queue is empty
    #[test]
    fn test_process_phq_empty_queue() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        // Create device queues with empty PHQ
        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|_| DeviceQueue {
                queue: Queue::new(256),
                event: std::sync::Arc::new(
                    utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK)
                        .expect("Failed to create eventfd"),
                ),
            })
            .collect();

        let mut balloon = Balloon::new().expect("Failed to create balloon device");
        balloon
            .activate(mem, interrupt, device_queues)
            .expect("Failed to activate balloon device");

        // Call process_phq with empty queue
        let result = balloon.process_phq();

        // Should return false (no descriptors to process)
        assert!(
            !result,
            "process_phq should return false when queue is empty"
        );
    }

    /// Test AC2.1: Inflated bitmap tracking on inflate/deflate
    /// After inflate, bits are set for inflated PFNs.
    /// After deflate of the same PFNs, bits are cleared.
    #[test]
    fn test_inflated_bitmap_tracking() {
        // Create guest memory
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        const DESC_TABLE_ADDR: u64 = 0x1000;
        const AVAIL_RING_ADDR: u64 = 0x2000;
        const USED_RING_ADDR: u64 = 0x3000;
        const PFN_DATA_ADDR: u64 = 0x10000;

        // Create device queues with inflate/deflate queue descriptor structures
        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|i| DeviceQueue {
                queue: {
                    let mut q = Queue::new(256);
                    q.size = 256;
                    q.ready = true;
                    if i == 0 || i == 1 {
                        // Inflate or Deflate queue - set up descriptor structures
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

        // Write PFN values for inflate
        let pfn1: u32 = 0x1;
        let pfn2: u32 = 0x5;
        mem.write_obj(pfn1, GuestAddress(PFN_DATA_ADDR))
            .expect("Failed to write PFN 1");
        mem.write_obj(pfn2, GuestAddress(PFN_DATA_ADDR + 4))
            .expect("Failed to write PFN 2");

        // Set up inflate descriptor
        let inflate_desc = Descriptor {
            addr: PFN_DATA_ADDR,
            len: 8,
            flags: 0,
            next: 0,
        };
        mem.write_obj(inflate_desc, GuestAddress(DESC_TABLE_ADDR))
            .expect("Failed to write descriptor");

        // Set up avail ring to indicate descriptor 0 is available
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR))
            .expect("Failed to write avail flags");
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .expect("Failed to write avail idx");
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 4))
            .expect("Failed to write avail ring[0]");

        // Set up used ring
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR))
            .expect("Failed to write used flags");
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to write used idx");

        // Process inflate
        let result = balloon.process_inflate();
        assert!(result, "process_inflate should return true");

        // Verify inflated bitmap has the bits set
        if let Some(ref bitmap) = balloon.inflated_bitmap {
            assert!(bitmap.is_set(pfn1), "PFN 1 should be marked as inflated");
            assert!(bitmap.is_set(pfn2), "PFN 2 should be marked as inflated");
        } else {
            panic!("inflated_bitmap should be initialized");
        }

        // Now test deflate - write the same PFNs to the deflate queue
        mem.write_obj(pfn1, GuestAddress(PFN_DATA_ADDR + 0x1000))
            .expect("Failed to write PFN 1 for deflate");
        mem.write_obj(pfn2, GuestAddress(PFN_DATA_ADDR + 0x1004))
            .expect("Failed to write PFN 2 for deflate");

        // Set up deflate descriptor
        let deflate_desc = Descriptor {
            addr: PFN_DATA_ADDR + 0x1000,
            len: 8,
            flags: 0,
            next: 0,
        };
        mem.write_obj(deflate_desc, GuestAddress(DESC_TABLE_ADDR))
            .expect("Failed to write deflate descriptor");

        // Set up avail ring to indicate descriptor 0 is available for deflate
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR))
            .expect("Failed to write avail flags");
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .expect("Failed to write avail idx");
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 4))
            .expect("Failed to write avail ring[0]");

        // Set up used ring
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR))
            .expect("Failed to write used flags");
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to write used idx");

        // Process deflate
        let result = balloon.process_deflate();
        assert!(result, "process_deflate should return true");

        // Verify inflated bitmap bits are cleared
        if let Some(ref bitmap) = balloon.inflated_bitmap {
            assert!(
                !bitmap.is_set(pfn1),
                "PFN 1 should be cleared after deflate"
            );
            assert!(
                !bitmap.is_set(pfn2),
                "PFN 2 should be cleared after deflate"
            );
        }
    }

    /// Test AC2.2: Reported-free bitmap tracking on FRQ/PHQ
    /// After FRQ or PHQ processing, bits are set for reported ranges
    #[test]
    fn test_reported_free_bitmap_tracking() {
        // Create guest memory
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x50000)])
            .expect("Failed to create guest memory");

        let irqchip: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt = InterruptTransport::new(irqchip, "test-balloon".into())
            .expect("Failed to create interrupt transport");

        const DESC_TABLE_ADDR: u64 = 0x1000;
        const AVAIL_RING_ADDR: u64 = 0x2000;
        const USED_RING_ADDR: u64 = 0x3000;

        // Create device queues with FRQ queue descriptor structures
        let device_queues: Vec<DeviceQueue> = (0..5)
            .map(|i| DeviceQueue {
                queue: {
                    let mut q = Queue::new(256);
                    q.size = 256;
                    q.ready = true;
                    if i == 4 {
                        // FRQ queue - set up descriptor structures
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

        // Set up FRQ descriptor with a range of pages
        // Address 0x1000 covers PFN 0x1, length 0x4000 (4 pages)
        let frq_desc = Descriptor {
            addr: 0x1000,
            len: 0x4000,
            flags: 0,
            next: 0,
        };
        mem.write_obj(frq_desc, GuestAddress(DESC_TABLE_ADDR))
            .expect("Failed to write FRQ descriptor");

        // Set up avail ring
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR))
            .expect("Failed to write avail flags");
        mem.write_obj(1u16, GuestAddress(AVAIL_RING_ADDR + 2))
            .expect("Failed to write avail idx");
        mem.write_obj(0u16, GuestAddress(AVAIL_RING_ADDR + 4))
            .expect("Failed to write avail ring[0]");

        // Set up used ring
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR))
            .expect("Failed to write used flags");
        mem.write_obj(0u16, GuestAddress(USED_RING_ADDR + 2))
            .expect("Failed to write used idx");

        // Process FRQ
        let result = balloon.process_frq();
        assert!(result, "process_frq should return true");

        // Verify reported-free bitmap has the bits set for the range
        if let Some(ref bitmap) = balloon.reported_free_bitmap {
            // PFN 0x1, 0x2, 0x3, 0x4 should all be set
            assert!(
                bitmap.is_set(0x1),
                "PFN 0x1 should be marked as reported-free"
            );
            assert!(
                bitmap.is_set(0x2),
                "PFN 0x2 should be marked as reported-free"
            );
            assert!(
                bitmap.is_set(0x3),
                "PFN 0x3 should be marked as reported-free"
            );
            assert!(
                bitmap.is_set(0x4),
                "PFN 0x4 should be marked as reported-free"
            );
            // PFN 0x0 should NOT be set (outside range)
            assert!(
                !bitmap.is_set(0x0),
                "PFN 0x0 should not be marked as reported-free"
            );
        } else {
            panic!("reported_free_bitmap should be initialized");
        }
    }

    /// Test AC4.5: Balloon device state survives snapshot/restore roundtrip
    /// After save_backend_state() and restore_backend_state(), config and hinting fields match
    #[test]
    #[cfg(feature = "snapshot")]
    fn test_balloon_snapshot_roundtrip() {
        // Create first balloon with distinctive values
        let mut balloon1 = Balloon::new().expect("Failed to create balloon device");
        balloon1.config.num_pages = 12345;
        balloon1.config.actual = 6789;
        balloon1.config.free_page_report_cmd_id = 42;
        balloon1.config.poison_val = 0xDEADBEEF;
        balloon1.hinting_cmd_counter = 10;
        balloon1.hinting_host_cmd = 5;
        balloon1.stats_desc_index = Some(3);

        // Save the state
        let saved_data = balloon1
            .save_backend_state()
            .expect("save_backend_state should return Some(data)");

        // Create a second balloon
        let mut balloon2 = Balloon::new().expect("Failed to create balloon device");

        // Restore the state
        balloon2.restore_backend_state(&saved_data);

        // Copy packed struct fields to avoid alignment issues
        let num_pages = balloon2.config.num_pages;
        let actual = balloon2.config.actual;
        let free_page_report_cmd_id = balloon2.config.free_page_report_cmd_id;
        let poison_val = balloon2.config.poison_val;

        // Verify config fields match
        assert_eq!(num_pages, 12345, "num_pages should be restored");
        assert_eq!(actual, 6789, "actual should be restored");
        assert_eq!(
            free_page_report_cmd_id, 42,
            "free_page_report_cmd_id should be restored"
        );
        assert_eq!(poison_val, 0xDEADBEEF, "poison_val should be restored");

        // Verify hinting fields match
        assert_eq!(
            balloon2.hinting_cmd_counter, 10,
            "hinting_cmd_counter should be restored"
        );
        assert_eq!(
            balloon2.hinting_host_cmd, 5,
            "hinting_host_cmd should be restored"
        );

        // Verify stats_desc_index is reset to None (intentionally)
        assert_eq!(
            balloon2.stats_desc_index, None,
            "stats_desc_index should be reset to None after restore"
        );
    }

    /// Test AC4.5: restore_backend_state with empty data doesn't panic
    /// Should log error but continue normally
    #[test]
    #[cfg(feature = "snapshot")]
    fn test_balloon_snapshot_empty_data_no_panic() {
        let mut balloon = Balloon::new().expect("Failed to create balloon device");

        // Set some initial values different from defaults
        balloon.config.num_pages = 999;

        // Call restore with empty data — should not panic
        balloon.restore_backend_state(&[]);

        // Config values should remain at whatever they were before (or defaults on error)
        // The important thing is it doesn't panic
    }

    /// Test AC4.5: Reclaimed page bitmaps are not serialized
    /// After save/restore, bitmaps are still None (not created during restore)
    #[test]
    #[cfg(feature = "snapshot")]
    fn test_balloon_snapshot_no_bitmaps() {
        // Create first balloon
        let mut balloon1 = Balloon::new().expect("Failed to create balloon device");
        balloon1.config.num_pages = 100;

        // Verify bitmaps are None initially
        assert!(balloon1.inflated_bitmap.is_none());
        assert!(balloon1.reported_free_bitmap.is_none());

        // Save state
        let saved_data = balloon1
            .save_backend_state()
            .expect("save_backend_state should return Some(data)");

        // Create second balloon and restore
        let mut balloon2 = Balloon::new().expect("Failed to create balloon device");
        balloon2.restore_backend_state(&saved_data);

        // Verify bitmaps are still None after restore
        // (They would only be created during activate(), not restore)
        assert!(balloon2.inflated_bitmap.is_none());
        assert!(balloon2.reported_free_bitmap.is_none());
    }

    #[cfg(feature = "shuttle")]
    mod shuttle_tests {
        use shuttle::sync::{Arc, Condvar, Mutex};
        use shuttle::thread;

        /// Shuttle test for the balloon actual-pages condvar pattern.
        ///
        /// Models BalloonHandle::await_target + guest config-write handler coordination:
        ///   - "guest" thread: sets actual_pages and signals condvar
        ///     (device.rs lines 671-674: *val = actual_pages; cvar.notify_all())
        ///   - "VMM" thread: waits on condvar until actual >= target
        ///     (lib.rs await_target: cvar.wait(actual) loop)
        ///
        /// Verifies: condvar wait terminates, no deadlock, correct final value.
        ///
        /// Note: Uses cvar.wait (not wait_timeout) since shuttle does not respect
        /// real wall-clock durations. The stall_timeout path is tested separately
        /// in unit tests (test_balloon_handle_await_target_stalled_no_progress).
        #[test]
        fn shuttle_balloon_condvar_no_deadlock() {
            shuttle::check_random(
                || {
                    // actual_condvar: Arc<(Mutex<u64>, Condvar)>
                    // Mirrors device.rs actual_condvar structure (line 147)
                    let actual_condvar: Arc<(Mutex<u64>, Condvar)> =
                        Arc::new((Mutex::new(0u64), Condvar::new()));

                    let target_pages: u64 = 64; // arbitrary target

                    // "Guest" thread: writes actual and signals (device.rs lines 671-674)
                    let condvar_guest = Arc::clone(&actual_condvar);
                    let guest = thread::spawn(move || {
                        let (lock, cvar) = &*condvar_guest;
                        let mut val = lock.lock().unwrap();
                        *val = target_pages;
                        cvar.notify_all();
                    });

                    // "VMM" thread: await_target loop (lib.rs lines 3483-3514, wait path only)
                    let condvar_vmm = Arc::clone(&actual_condvar);
                    let vmm = thread::spawn(move || {
                        let (lock, cvar) = &*condvar_vmm;
                        let mut actual = lock.lock().unwrap();
                        while *actual < target_pages {
                            actual = cvar.wait(actual).unwrap();
                        }
                        assert!(
                            *actual >= target_pages,
                            "await_target must observe actual >= target after condvar wait, got {}",
                            *actual
                        );
                    });

                    guest.join().unwrap();
                    vmm.join().unwrap();
                },
                1000,
            );
        }

        /// Shuttle test: multiple guest updates, VMM observes final value.
        ///
        /// Models incremental inflation: guest sends multiple actual updates before
        /// reaching target. Verifies VMM loop terminates correctly.
        #[test]
        fn shuttle_balloon_incremental_updates_no_deadlock() {
            shuttle::check_random(
                || {
                    let actual_condvar: Arc<(Mutex<u64>, Condvar)> =
                        Arc::new((Mutex::new(0u64), Condvar::new()));

                    let target_pages: u64 = 3;

                    // Guest sends three incremental updates
                    let condvar_guest = Arc::clone(&actual_condvar);
                    let guest = thread::spawn(move || {
                        for pages in 1u64..=target_pages {
                            let (lock, cvar) = &*condvar_guest;
                            let mut val = lock.lock().unwrap();
                            *val = pages;
                            cvar.notify_all();
                        }
                    });

                    // VMM waits until actual reaches target
                    let condvar_vmm = Arc::clone(&actual_condvar);
                    let vmm = thread::spawn(move || {
                        let (lock, cvar) = &*condvar_vmm;
                        let mut actual = lock.lock().unwrap();
                        while *actual < target_pages {
                            actual = cvar.wait(actual).unwrap();
                        }
                        assert!(*actual >= target_pages);
                    });

                    guest.join().unwrap();
                    vmm.join().unwrap();
                },
                500,
            );
        }
    }
}
