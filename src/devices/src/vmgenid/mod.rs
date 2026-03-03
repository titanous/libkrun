// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! VM Generation ID (VMGENID) Device
//!
//! A simple platform device that manages a 128-bit GUID and writes it to guest memory.
//! This is not a virtio device — it's a platform device that bypasses MmioTransport.

use rand::Rng;
use utils::eventfd::EventFd;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

/// Size of a GUID in bytes (128-bit).
const GUID_SIZE: usize = 16;

/// VM Generation ID device that manages the 128-bit GUID.
pub struct Vmgenid {
    /// Guest physical address of the 4KB GUID page.
    guid_page_addr: u64,
    /// Byte offset within the page where the GUID is stored.
    guid_offset: u64,
    /// Current GUID value.
    guid: [u8; GUID_SIZE],
    /// EventFd used to inject the GED interrupt via KVM irqfd.
    interrupt_evt: EventFd,
    /// IRQ number registered with KVM for the GED.
    irq: u32,
}

impl Vmgenid {
    /// Creates a new Vmgenid device with a randomly generated GUID.
    ///
    /// Generates a fresh 128-bit random GUID and immediately writes it to guest memory
    /// at the specified address and offset.
    ///
    /// # Arguments
    /// * `guid_page_addr` - Guest physical address of the 4KB GUID page
    /// * `guid_offset` - Byte offset within the page where the GUID is stored
    /// * `irq` - IRQ number for the GED device
    /// * `mem` - Guest memory instance for writing the GUID
    ///
    /// # Returns
    /// A new Vmgenid instance with the generated GUID already written to guest memory,
    /// or an error if the GUID write to guest memory fails.
    pub fn new(
        guid_page_addr: u64,
        guid_offset: u64,
        irq: u32,
        mem: &GuestMemoryMmap,
    ) -> Result<Self, vm_memory::GuestMemoryError> {
        let mut guid = [0u8; GUID_SIZE];
        let mut rng = rand::rng();
        rng.fill(&mut guid);

        let interrupt_evt = EventFd::new(0).expect("Failed to create EventFd for GED interrupt");

        let vmgenid = Vmgenid {
            guid_page_addr,
            guid_offset,
            guid,
            interrupt_evt,
            irq,
        };

        // Write the initial GUID to guest memory.
        vmgenid.write_guid_to_memory(mem)?;

        Ok(vmgenid)
    }

    /// Updates the GUID with a new randomly generated value and writes it to guest memory.
    ///
    /// # Arguments
    /// * `mem` - Guest memory instance for writing the new GUID
    ///
    /// # Returns
    /// A tuple of `(old_guid, new_guid)` showing the previous and new GUID values,
    /// or an error if the GUID write to guest memory fails.
    pub fn update_guid(
        &mut self,
        mem: &GuestMemoryMmap,
    ) -> Result<([u8; 16], [u8; 16]), vm_memory::GuestMemoryError> {
        let old_guid = self.guid;

        // Generate a new random GUID.
        let mut new_guid = [0u8; GUID_SIZE];
        let mut rng = rand::rng();
        rng.fill(&mut new_guid);

        self.guid = new_guid;

        // Write the new GUID to guest memory.
        self.write_guid_to_memory(mem)?;

        Ok((old_guid, new_guid))
    }

    /// Returns a reference to the current GUID value.
    pub fn guid(&self) -> &[u8; 16] {
        &self.guid
    }

    /// Returns the guest physical address where the GUID is stored.
    pub fn guest_addr(&self) -> u64 {
        self.guid_page_addr + self.guid_offset
    }

    /// Returns a reference to the interrupt EventFd.
    pub fn interrupt_evt(&self) -> &EventFd {
        &self.interrupt_evt
    }

    /// Returns the IRQ number registered with KVM for the GED.
    pub fn irq(&self) -> u32 {
        self.irq
    }

    /// Signals the GED interrupt by writing to the EventFd.
    pub fn signal_interrupt(&self) -> std::io::Result<()> {
        self.interrupt_evt.write(1)
    }

    /// Writes the current GUID to guest memory at the configured address and offset.
    fn write_guid_to_memory(
        &self,
        mem: &GuestMemoryMmap,
    ) -> Result<(), vm_memory::GuestMemoryError> {
        let addr = GuestAddress(self.guest_addr());
        mem.write_slice(&self.guid, addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestAddress;

    #[test]
    fn test_new_generates_non_zero_guid() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let vmgenid = Vmgenid::new(0x1000, 40, 16, &mem).unwrap();

        // GUID should not be all zeros.
        assert_ne!(vmgenid.guid(), &[0u8; 16]);
    }

    #[test]
    fn test_new_writes_guid_to_guest_memory() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let vmgenid = Vmgenid::new(0x1000, 40, 16, &mem).unwrap();

        // Read back the GUID from guest memory.
        let addr = GuestAddress(0x1000 + 40);
        let mut buffer = [0u8; 16];
        mem.read_slice(&mut buffer, addr).unwrap();

        // Should match the internal GUID.
        assert_eq!(buffer, *vmgenid.guid());
    }

    #[test]
    fn test_update_guid_produces_different_guid() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut vmgenid = Vmgenid::new(0x2000, 50, 16, &mem).unwrap();

        let initial_guid = *vmgenid.guid();

        let (old_guid, new_guid) = vmgenid.update_guid(&mem).unwrap();

        // Old GUID should match initial.
        assert_eq!(old_guid, initial_guid);

        // New GUID should be different (with very high probability).
        assert_ne!(old_guid, new_guid);

        // Current GUID should match the new one.
        assert_eq!(*vmgenid.guid(), new_guid);
    }

    #[test]
    fn test_update_guid_writes_to_guest_memory() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut vmgenid = Vmgenid::new(0x3000, 60, 16, &mem).unwrap();

        let (_old_guid, new_guid) = vmgenid.update_guid(&mem).unwrap();

        // Read back from guest memory.
        let addr = GuestAddress(0x3000 + 60);
        let mut buffer = [0u8; 16];
        mem.read_slice(&mut buffer, addr).unwrap();

        // Should match the new GUID.
        assert_eq!(buffer, new_guid);
    }

    #[test]
    fn test_multiple_updates_produce_different_guids() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut vmgenid = Vmgenid::new(0x4000, 70, 16, &mem).unwrap();

        let (_old1, guid1) = vmgenid.update_guid(&mem).unwrap();
        let (_old2, guid2) = vmgenid.update_guid(&mem).unwrap();
        let (_old3, guid3) = vmgenid.update_guid(&mem).unwrap();

        // All should be different (with very high probability).
        assert_ne!(guid1, guid2);
        assert_ne!(guid2, guid3);
        assert_ne!(guid1, guid3);
    }

    #[test]
    fn test_guest_addr_calculation() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let vmgenid = Vmgenid::new(0x5000, 80, 16, &mem).unwrap();

        // guest_addr should return page address + offset.
        assert_eq!(vmgenid.guest_addr(), 0x5000 + 80);
    }

    #[test]
    fn test_new_handles_invalid_guest_address() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        // Try to create with an address that's out of bounds.
        let result = Vmgenid::new(0x200000, 0, 16, &mem);
        assert!(result.is_err());
    }

    #[test]
    fn test_update_guid_handles_invalid_guest_address() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut vmgenid = Vmgenid::new(0x50000, 0, 16, &mem).unwrap();

        // Manually set an invalid address by mutating the internal state.
        // Since we can't do that with the current API, we just verify the signature accepts errors.
        // This test validates that update_guid returns a Result type that can propagate errors.
        vmgenid.guid_page_addr = 0x200000; // Out of bounds
        let result = vmgenid.update_guid(&mem);
        assert!(result.is_err());
    }
}
