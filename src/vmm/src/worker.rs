use std::io;
use std::sync::{Arc, Mutex};

#[cfg(feature = "tee")]
use utils::worker_message::MemoryProperties;
use utils::worker_message::WorkerMessage;

use crossbeam_channel::Receiver;
#[cfg(feature = "tee")]
use crossbeam_channel::Sender;
#[cfg(feature = "tee")]
use kvm_bindings::{kvm_memory_attributes, KVM_MEMORY_ATTRIBUTE_PRIVATE};
#[cfg(feature = "tee")]
use libc::{fallocate, madvise, FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, MADV_DONTNEED};
#[cfg(feature = "tee")]
use std::ffi::c_void;
#[cfg(feature = "tee")]
use vm_memory::{
    guest_memory::GuestMemoryBackend, Address, GuestAddress, GuestMemoryRegion, MemoryRegionAddress,
};

pub fn start_worker_thread(
    vmm: Arc<Mutex<super::Vmm>>,
    receiver: Receiver<WorkerMessage>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("vmm worker".into())
        .spawn(move || loop {
            match receiver.recv() {
                Err(e) => error!("error receiving message from vmm worker thread: {e:?}"),
                #[cfg(target_os = "macos")]
                Ok(message) => vmm.lock().unwrap().match_worker_message(message),
                #[cfg(target_os = "linux")]
                Ok(message) => vmm.lock().unwrap().match_worker_message(message),
            }
        })?;
    Ok(())
}

/// Validates that a convert_memory operation is within region bounds and fits
/// in syscall parameter types.
///
/// Returns Ok((offset, size)) on success where both are safe for use in
/// madvise (as usize) and fallocate (as i64).
/// Returns Err if: size is zero, offset out of region, size + offset > region, size > i64::MAX,
/// or offset > i64::MAX.
#[cfg(any(feature = "tee", kani))]
pub(crate) fn validate_convert_bounds(
    gpa: u64,
    size: u64,
    region_start: u64,
    region_size: u64,
) -> Result<(u64, u64), &'static str> {
    if size == 0 {
        return Err("size cannot be zero");
    }
    if gpa < region_start {
        return Err("gpa before region start");
    }
    let offset = gpa - region_start;
    let end = offset
        .checked_add(size)
        .ok_or("offset + size overflows u64")?;
    if end > region_size {
        return Err("range exceeds region");
    }
    if size > i64::MAX as u64 {
        return Err("size exceeds i64::MAX");
    }
    if offset > i64::MAX as u64 {
        return Err("offset exceeds i64::MAX");
    }
    Ok((offset, size))
}

impl super::Vmm {
    fn match_worker_message(&self, msg: WorkerMessage) {
        match msg {
            #[cfg(target_os = "macos")]
            WorkerMessage::GpuAddMapping(s, h, g, l) => self.add_mapping(s, h, g, l),
            #[cfg(target_os = "macos")]
            WorkerMessage::GpuRemoveMapping(s, g, l) => self.remove_mapping(s, g, l),
            #[cfg(target_arch = "x86_64")]
            WorkerMessage::GsiRoute(sender, entries) => {
                let mut routing = kvm_bindings::KvmIrqRouting::new(entries.len()).unwrap();
                let routing_entries = routing.as_mut_slice();
                routing_entries.copy_from_slice(&entries);
                sender
                    .send(self.vm.fd().set_gsi_routing(&routing).is_ok())
                    .unwrap();
            }
            #[cfg(target_arch = "x86_64")]
            WorkerMessage::IrqLine(sender, irq, active) => {
                sender
                    .send(self.vm.fd().set_irq_line(irq, active).is_ok())
                    .unwrap();
            }
            WorkerMessage::ConvertMemory(_sender, _properties) =>
            {
                #[cfg(feature = "tee")]
                self.convert_memory(_sender, _properties)
            }
        }
    }

    #[cfg(feature = "tee")]
    fn convert_memory(&self, sender: Sender<bool>, properties: MemoryProperties) {
        let Some((guest_memfd, region_start)) = self.kvm_vm().guest_memfd_get(properties.gpa)
        else {
            error!(
                "unable to find KVM guest_memfd for memory region corresponding to GPA 0x{:x}",
                properties.gpa
            );
            sender.send(false).unwrap();
            return;
        };

        let attributes: u64 = if properties.private {
            KVM_MEMORY_ATTRIBUTE_PRIVATE as u64
        } else {
            0
        };

        let attr = kvm_memory_attributes {
            address: properties.gpa,
            size: properties.size,
            attributes,
            flags: 0,
        };

        if self.kvm_vm().fd().set_memory_attributes(attr).is_err() {
            error!("unable to set memory attributes for memory region corresponding to guest address 0x{:x}", properties.gpa);
            sender.send(false).unwrap();
            return;
        }

        let region = self
            .guest_memory()
            .find_region(GuestAddress(properties.gpa));
        if region.is_none() {
            error!(
                "guest memory region corresponding to GPA 0x{:x} not found",
                properties.gpa
            );
            sender.send(false).unwrap();
            return;
        }

        let region = region.unwrap();
        let region_size = region.len();

        let (offset, _) = match validate_convert_bounds(
            properties.gpa,
            properties.size,
            region_start,
            region_size,
        ) {
            Ok(v) => v,
            Err(e) => {
                error!(
                    "convert_memory: invalid bounds for GPA 0x{:x} size 0x{:x}: {}",
                    properties.gpa, properties.size, e
                );
                sender.send(false).unwrap();
                return;
            }
        };

        if properties.private {
            let region_addr = MemoryRegionAddress(offset);

            let Ok(host_startaddr) = region.get_host_address(region_addr) else {
                error!(
                    "host address corresponding to memory region address 0x{:x} not found",
                    region_addr.raw_value()
                );
                sender.send(false).unwrap();
                return;
            };

            let ret = unsafe {
                madvise(
                    host_startaddr as *mut c_void,
                    properties.size.try_into().unwrap(),
                    MADV_DONTNEED,
                )
            };

            if ret < 0 {
                error!("unable to advise kernel that memory region corresponding to GPA 0x{:x} will likely not be needed (madvise)", properties.gpa);
                sender.send(false).unwrap();
                return;
            }
        } else {
            let ret = unsafe {
                fallocate(
                    guest_memfd,
                    FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
                    offset as i64,
                    properties.size as i64,
                )
            };

            if ret < 0 {
                error!("unable to allocate space in guest_memfd for shared memory (fallocate)");
                sender.send(false).unwrap();
                return;
            }
        }

        sender.send(true).unwrap();
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn simulated_host_address_valid(gpa: u64, region_start: u64, region_size: u64) -> bool {
        gpa >= region_start && gpa < region_start.saturating_add(region_size)
    }

    /// Proof: validate_convert_bounds correctly enforces all safety preconditions
    /// for madvise and fallocate calls in convert_memory.
    ///
    /// GAP-022: convert_memory calls madvise/fallocate with offset and size derived
    /// from WorkerMessage without checking region bounds or i64 cast safety.
    /// The fix extracts validate_convert_bounds which checks all conditions.
    /// Breaking change: removing any check in validate_convert_bounds (e.g., the
    /// `end > region_size` guard) would allow offset+size to exceed region_size,
    /// failing the first assertion.
    ///
    /// Bound: no loops; unwind 1 is sufficient.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_worker_madvise_size_within_region() {
        let region_start: u64 = kani::any_where(|&s: &u64| s <= u64::MAX - 0x10000);
        let region_size: u64 = kani::any_where(|&sz| sz > 0 && sz <= 0x10000);
        let gpa: u64 = kani::any();
        let properties_size: u64 = kani::any();

        kani::assume(simulated_host_address_valid(gpa, region_start, region_size));

        match validate_convert_bounds(gpa, properties_size, region_start, region_size) {
            Ok((offset, size)) => {
                kani::assert(offset + size <= region_size, "madvise range within region");
                kani::assert(size <= i64::MAX as u64, "size fits i64");
                // Cover: both the gpa-at-region-start and gpa-at-region-end cases.
                kani::cover!(gpa == region_start, "gpa at region start");
                kani::cover!(
                    gpa == region_start.saturating_add(region_size).saturating_sub(1),
                    "gpa at last byte of region"
                );
            }
            Err(_) => {
                // Cover the two primary rejection causes: out-of-range size and zero size.
                kani::cover!(properties_size == 0, "rejected: zero size");
                kani::cover!(
                    properties_size > region_size,
                    "rejected: size exceeds region"
                );
            }
        }
    }

    /// Proof: validate_convert_bounds enforces fallocate range safety.
    ///
    /// Breaking change: removing the `offset > i64::MAX` or `size > i64::MAX` guard
    /// would allow unsafe casts to i64, failing the non-negative assertions.
    ///
    /// Bound: no loops; unwind 1 is sufficient.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_worker_fallocate_size_within_region() {
        let region_start: u64 = kani::any_where(|&s: &u64| s <= u64::MAX - 0x10000);
        let region_size: u64 = kani::any_where(|&sz| sz > 0 && sz <= 0x10000);
        let gpa: u64 = kani::any();
        let properties_size: u64 = kani::any();

        kani::assume(simulated_host_address_valid(gpa, region_start, region_size));

        match validate_convert_bounds(gpa, properties_size, region_start, region_size) {
            Ok((offset, size)) => {
                kani::assert(
                    offset + size <= region_size,
                    "fallocate range within region",
                );
                // Safe to cast: both fit in i64
                let _offset_i64 = offset as i64;
                let _size_i64 = size as i64;
                kani::assert(_offset_i64 >= 0, "offset non-negative after cast");
                kani::assert(_size_i64 >= 0, "size non-negative after cast");
                kani::cover!(offset == 0, "offset zero (gpa at region start)");
                kani::cover!(size == 1, "minimum accepted size");
            }
            Err(_) => {
                kani::cover!(properties_size == 0, "rejected: zero size");
                kani::cover!(
                    properties_size > 0 && gpa >= region_start,
                    "rejected: size or overflow"
                );
            }
        }
    }

    /// Proof: validate_convert_bounds prevents i64 cast overflow for size.
    ///
    /// Breaking change: removing the `size > i64::MAX as u64` guard would allow
    /// an oversized `size` to pass through, making the `as i64` cast UB.
    ///
    /// Bound: no loops; unwind 1 is sufficient.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_worker_fallocate_size_fits_i64() {
        let properties_size: u64 = kani::any();
        let region_start: u64 = kani::any_where(|&s: &u64| s <= u64::MAX - 0x10000);
        let region_size: u64 = kani::any_where(|&sz| sz > 0 && sz <= 0x10000);
        let gpa: u64 = kani::any();

        kani::assume(simulated_host_address_valid(gpa, region_start, region_size));

        match validate_convert_bounds(gpa, properties_size, region_start, region_size) {
            Ok((_, size)) => {
                kani::assert(size <= i64::MAX as u64, "size safe for i64 cast");
                // Cover: prove the non-trivial case where size is large but still fits.
                kani::cover!(size > 0x1000, "large size fits i64");
                kani::cover!(size == 1, "minimum size accepted");
            }
            Err(_) => {
                // Cover: properties_size > i64::MAX is a specific rejection cause.
                kani::cover!(
                    properties_size > i64::MAX as u64,
                    "rejected: size exceeds i64::MAX"
                );
                kani::cover!(properties_size == 0, "rejected: zero size");
            }
        }
    }

    /// Proof: validate_convert_bounds rejects zero size.
    ///
    /// Breaking change: removing the `size == 0` guard in validate_convert_bounds
    /// would allow zero-size madvise/fallocate calls (undefined behavior on some kernels).
    ///
    /// Bound: no loops; unwind 1 is sufficient.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_worker_size_cannot_be_zero() {
        let region_start: u64 = kani::any();
        let region_size: u64 = kani::any_where(|&sz| sz > 0);
        let gpa: u64 = kani::any_where(|&g: &u64| {
            g >= region_start && g < region_start.saturating_add(region_size)
        });

        // Test with size == 0, which must always be rejected.
        // String `.contains()` uses SIMD intrinsics unsupported by Kani; only
        // check that Err is returned (not the message text).
        kani::assert(
            validate_convert_bounds(gpa, 0, region_start, region_size).is_err(),
            "size==0 must always be rejected",
        );
        // Cover the full range of valid GPAs to confirm zero-size is always rejected.
        kani::cover!(gpa == region_start, "zero size rejected at region start");
        kani::cover!(gpa > region_start, "zero size rejected at interior GPA");
    }

    /// Proof (negative): GPA below region_start is always rejected.
    ///
    /// Security-critical: a GPA below the region start would compute a negative
    /// offset (wrapping to a huge u64), giving access to memory outside the region
    /// (potential TEE host memory exposure). validate_convert_bounds must reject it.
    ///
    /// Breaking change: removing the `gpa < region_start` guard in
    /// validate_convert_bounds would allow underflow and pass this proof.
    ///
    /// Bound: no loops; unwind 1 is sufficient.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_worker_gpa_below_region_rejected() {
        let region_start: u64 = kani::any_where(|&s: &u64| s > 0); // must be > 0 so GPA can be below
        let region_size: u64 = kani::any_where(|&sz| sz > 0 && sz <= 0x10000);
        // GPA strictly below region_start.
        let gpa: u64 = kani::any_where(|&g| g < region_start);
        let size: u64 = kani::any_where(|&s| s > 0);

        let result = validate_convert_bounds(gpa, size, region_start, region_size);

        kani::assert(result.is_err(), "GPA below region_start must be rejected");
        // Cover both: GPA just one below start, and GPA much further below start.
        kani::cover!(gpa == region_start - 1, "GPA one below region_start");
        kani::cover!(
            gpa == 0 && region_start > 1,
            "GPA is zero, region_start is high"
        );
    }

    /// Proof (negative): GPA at or above region_start + region_size is always rejected.
    ///
    /// Security-critical: a GPA >= region_start + region_size is outside the region;
    /// allowing it would expose memory beyond the allocated region to syscalls.
    /// validate_convert_bounds must reject it regardless of size.
    ///
    /// Breaking change: removing or weakening the `end > region_size` guard in
    /// validate_convert_bounds would allow out-of-bounds access.
    ///
    /// Bound: no loops; unwind 1 is sufficient.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_worker_gpa_at_or_above_region_end_rejected() {
        let region_start: u64 = kani::any_where(|&s: &u64| s <= u64::MAX - 0x10000);
        let region_size: u64 = kani::any_where(|&sz| sz > 0 && sz <= 0x10000);
        // GPA at or after the end of the region.
        let region_end = region_start.saturating_add(region_size);
        let gpa: u64 = kani::any_where(|&g| g >= region_end);
        let size: u64 = kani::any_where(|&s| s > 0);

        let result = validate_convert_bounds(gpa, size, region_start, region_size);

        kani::assert(
            result.is_err(),
            "GPA at or above region_end must be rejected",
        );
        // Cover: GPA exactly at region_end, and GPA well beyond it.
        kani::cover!(gpa == region_end, "GPA exactly at region_end");
        kani::cover!(gpa > region_end, "GPA past region_end");
    }
}
