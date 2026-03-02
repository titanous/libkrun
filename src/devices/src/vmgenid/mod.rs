// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! VM Generation ID (VMGENID) Device
//!
//! A simple platform device that manages a 128-bit GUID and writes it to guest memory.
//! This is not a virtio device — it's a platform device that bypasses MmioTransport.

use rand::Rng;
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
    /// * `mem` - Guest memory instance for writing the GUID
    ///
    /// # Returns
    /// A new Vmgenid instance with the generated GUID already written to guest memory.
    pub fn new(guid_page_addr: u64, guid_offset: u64, mem: &GuestMemoryMmap) -> Self {
        let mut guid = [0u8; GUID_SIZE];
        let mut rng = rand::rng();
        rng.fill(&mut guid);

        let vmgenid = Vmgenid {
            guid_page_addr,
            guid_offset,
            guid,
        };

        // Write the initial GUID to guest memory.
        vmgenid.write_guid_to_memory(mem);

        vmgenid
    }

    /// Updates the GUID with a new randomly generated value and writes it to guest memory.
    ///
    /// # Arguments
    /// * `mem` - Guest memory instance for writing the new GUID
    ///
    /// # Returns
    /// A tuple of `(old_guid, new_guid)` showing the previous and new GUID values.
    pub fn update_guid(&mut self, mem: &GuestMemoryMmap) -> ([u8; 16], [u8; 16]) {
        let old_guid = self.guid;

        // Generate a new random GUID.
        let mut new_guid = [0u8; GUID_SIZE];
        let mut rng = rand::rng();
        rng.fill(&mut new_guid);

        self.guid = new_guid;

        // Write the new GUID to guest memory.
        self.write_guid_to_memory(mem);

        (old_guid, new_guid)
    }

    /// Returns a reference to the current GUID value.
    pub fn guid(&self) -> &[u8; 16] {
        &self.guid
    }

    /// Returns the guest physical address where the GUID is stored.
    pub fn guest_addr(&self) -> u64 {
        self.guid_page_addr + self.guid_offset
    }

    /// Writes the current GUID to guest memory at the configured address and offset.
    fn write_guid_to_memory(&self, mem: &GuestMemoryMmap) {
        let addr = GuestAddress(self.guest_addr());
        let _ = mem.write_slice(&self.guid, addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestAddress;

    #[test]
    fn test_new_generates_non_zero_guid() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let vmgenid = Vmgenid::new(0x1000, 40, &mem);

        // GUID should not be all zeros.
        assert_ne!(vmgenid.guid(), &[0u8; 16]);
    }

    #[test]
    fn test_new_writes_guid_to_guest_memory() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let vmgenid = Vmgenid::new(0x1000, 40, &mem);

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
        let mut vmgenid = Vmgenid::new(0x2000, 50, &mem);

        let initial_guid = *vmgenid.guid();

        let (old_guid, new_guid) = vmgenid.update_guid(&mem);

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
        let mut vmgenid = Vmgenid::new(0x3000, 60, &mem);

        let (_old_guid, new_guid) = vmgenid.update_guid(&mem);

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
        let mut vmgenid = Vmgenid::new(0x4000, 70, &mem);

        let (_old1, guid1) = vmgenid.update_guid(&mem);
        let (_old2, guid2) = vmgenid.update_guid(&mem);
        let (_old3, guid3) = vmgenid.update_guid(&mem);

        // All should be different (with very high probability).
        assert_ne!(guid1, guid2);
        assert_ne!(guid2, guid3);
        assert_ne!(guid1, guid3);
    }

    #[test]
    fn test_guest_addr_calculation() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let vmgenid = Vmgenid::new(0x5000, 80, &mem);

        // guest_addr should return page address + offset.
        assert_eq!(vmgenid.guest_addr(), 0x5000 + 80);
    }
}
