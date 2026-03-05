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
                kani::cover!(true, "valid convert accepted");
            }
            Err(_) => {
                kani::cover!(true, "invalid convert rejected");
            }
        }
    }

    /// Proof: validate_convert_bounds enforces fallocate range safety.
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
                kani::cover!(true, "valid fallocate range");
            }
            Err(_) => {
                kani::cover!(true, "invalid rejected");
            }
        }
    }

    /// Proof: validate_convert_bounds prevents i64 cast overflow for size.
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
                kani::cover!(true, "size fits i64");
            }
            Err(_) => {
                kani::cover!(true, "oversized or out-of-bounds rejected");
            }
        }
    }

    /// Proof: validate_convert_bounds rejects zero size.
    #[kani::proof]
    #[kani::solver(cadical)]
    fn proof_worker_size_cannot_be_zero() {
        let region_start: u64 = kani::any();
        let region_size: u64 = kani::any_where(|&sz| sz > 0);
        let gpa: u64 = kani::any_where(|&g: &u64| {
            g >= region_start && g < region_start.saturating_add(region_size)
        });

        // Test with size == 0, which must always be rejected
        // String `.contains()` uses SIMD intrinsics unsupported by Kani; only
        // check that Err is returned (not the message text).
        kani::assert(
            validate_convert_bounds(gpa, 0, region_start, region_size).is_err(),
            "size==0 must always be rejected",
        );
        kani::cover!(true, "zero size correctly rejected");
    }
}
