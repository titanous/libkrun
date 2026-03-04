// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Write a SETUP_VMGENID setup_data node into guest memory.
//!
//! The 32-byte struct at SETUP_DATA_ADDR follows the Linux x86 boot protocol
//! setup_data linked-list format (arch/x86/include/uapi/asm/setup_data.h):
//!
//!   offset  0: next     (u64) = 0  — end of chain
//!   offset  8: type     (u32) = SETUP_VMGENID (10)
//!   offset 12: len      (u32) = 16 — sizeof(vmgenid_setup_data)
//!   offset 16: guid_pa  (u64) — guest physical address of the 16-byte GUID
//!   offset 24: irq      (u32) — PIC IRQ number for guest notification
//!   offset 28: pad      (u32) = 0

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::x86_64::layout::{SETUP_DATA_ADDR, VMGENID_GUID_OFFSET, VMGENID_GUID_PAGE, VMGENID_IRQ};

/// SETUP_VMGENID type constant (Linux 6.12+).
const SETUP_VMGENID: u32 = 10;

/// vmgenid_setup_data payload size: guid_pa(8) + irq(4) + pad(4).
const VMGENID_SETUP_DATA_LEN: u32 = 16;

#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    /// Failed to write setup_data to guest memory.
    WriteSetupData,
}

/// Write the SETUP_VMGENID node and return the physical address for
/// `boot_params.hdr.setup_data`.
pub fn write_vmgenid_setup_data(guest_mem: &GuestMemoryMmap) -> Result<u64, Error> {
    let guid_pa = VMGENID_GUID_PAGE + VMGENID_GUID_OFFSET;

    // next: 0 (end of chain)
    guest_mem
        .write_slice(&0u64.to_le_bytes(), GuestAddress(SETUP_DATA_ADDR))
        .map_err(|_| Error::WriteSetupData)?;
    // type: SETUP_VMGENID
    guest_mem
        .write_slice(
            &SETUP_VMGENID.to_le_bytes(),
            GuestAddress(SETUP_DATA_ADDR + 8),
        )
        .map_err(|_| Error::WriteSetupData)?;
    // len: payload size
    guest_mem
        .write_slice(
            &VMGENID_SETUP_DATA_LEN.to_le_bytes(),
            GuestAddress(SETUP_DATA_ADDR + 12),
        )
        .map_err(|_| Error::WriteSetupData)?;
    // guid_pa
    guest_mem
        .write_slice(&guid_pa.to_le_bytes(), GuestAddress(SETUP_DATA_ADDR + 16))
        .map_err(|_| Error::WriteSetupData)?;
    // irq
    guest_mem
        .write_slice(
            &VMGENID_IRQ.to_le_bytes(),
            GuestAddress(SETUP_DATA_ADDR + 24),
        )
        .map_err(|_| Error::WriteSetupData)?;
    // pad
    guest_mem
        .write_slice(&0u32.to_le_bytes(), GuestAddress(SETUP_DATA_ADDR + 28))
        .map_err(|_| Error::WriteSetupData)?;

    Ok(SETUP_DATA_ADDR)
}

#[cfg(test)]
mod tests {
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    use super::*;
    use crate::x86_64::layout::{
        SETUP_DATA_ADDR, VMGENID_GUID_OFFSET, VMGENID_GUID_PAGE, VMGENID_IRQ,
    };

    fn read_u32(mem: &GuestMemoryMmap, offset: u64) -> u32 {
        let mut buf = [0u8; 4];
        mem.read_slice(&mut buf, GuestAddress(SETUP_DATA_ADDR + offset))
            .unwrap();
        u32::from_le_bytes(buf)
    }

    fn read_u64(mem: &GuestMemoryMmap, offset: u64) -> u64 {
        let mut buf = [0u8; 8];
        mem.read_slice(&mut buf, GuestAddress(SETUP_DATA_ADDR + offset))
            .unwrap();
        u64::from_le_bytes(buf)
    }

    #[test]
    fn test_write_vmgenid_setup_data_layout() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();

        let addr = write_vmgenid_setup_data(&mem).unwrap();
        assert_eq!(addr, SETUP_DATA_ADDR);

        // offset 0: next = 0 (end of chain)
        assert_eq!(read_u64(&mem, 0), 0);
        // offset 8: type = SETUP_VMGENID (10)
        assert_eq!(read_u32(&mem, 8), 10);
        // offset 12: len = 16
        assert_eq!(read_u32(&mem, 12), 16);
        // offset 16: guid_pa = VMGENID_GUID_PAGE + VMGENID_GUID_OFFSET
        assert_eq!(read_u64(&mem, 16), VMGENID_GUID_PAGE + VMGENID_GUID_OFFSET);
        // offset 24: irq = VMGENID_IRQ
        assert_eq!(read_u32(&mem, 24), VMGENID_IRQ);
        // offset 28: pad = 0
        assert_eq!(read_u32(&mem, 28), 0);
    }
}
