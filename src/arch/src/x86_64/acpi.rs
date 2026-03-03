// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use acpi_tables::rsdp::Rsdp;
use acpi_tables::sdt::Sdt;
use acpi_tables::aml::*;
use acpi_tables::Aml;
use std::result;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use zerocopy::IntoBytes;

use crate::x86_64::layout;

/// Errors during ACPI table setup.
#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    /// Failure to write RSDP to memory.
    Rsdp,
    /// Failure to write XSDT to memory.
    Xsdt,
    /// Failure to write FADT to memory.
    Fadt,
    /// Failure to write DSDT to memory.
    Dsdt,
    /// Failure to write MADT to memory.
    Madt,
    /// ACPI tables exceed maximum size.
    Overflow,
}

pub type Result<T> = result::Result<T, Error>;

// OEM identifiers
const OEM_ID: [u8; 6] = *b"LIBKRN";
const OEM_TABLE_ID: [u8; 8] = *b"KRUNVMGN";
const OEM_REVISION: u32 = 1;

// FADT field offsets (ACPI 6.x spec)
const FADT_FIELD_FLAGS: usize = 112;
const FADT_FIELD_X_DSDT: usize = 140;
const FADT_FIELD_HYPERVISOR_ID: usize = 268;

// MADT constants
const MADT_FIELD_LOCAL_APIC_ADDR: usize = 36;
const MADT_FIELD_FLAGS: usize = 40;
const MADT_CPU_ENABLE_FLAG: u32 = 1;
const LOCAL_APIC_ADDR: u32 = 0xfee0_0000;
const IO_APIC_ADDR: u32 = 0xfec0_0000;
// MADT entry types
const ACPI_MADT_LOCAL_APIC: u8 = 0;
const ACPI_MADT_IO_APIC: u8 = 1;

/// Sets up minimal ACPI tables for the guest.
///
/// Generates RSDP → XSDT → FADT → DSDT in the ACPI scan region.
/// FADT is configured with HW_REDUCED_ACPI flag (bit 20) and revision 6.
/// DSDT contains device definitions for VMGENID and GED.
///
/// Table layout follows the cloud-hypervisor approach: all tables built
/// using raw Sdt with manual field writes at ACPI-spec offsets.
pub fn setup_acpi_tables(guest_mem: &GuestMemoryMmap, guid_addr: u64, ged_irq: u32, num_cpus: u8) -> Result<u64> {
    // Build AML for VMGENID and GED devices
    let guid_addr_qword: u64 = guid_addr;
    let ged_irq_dword: u32 = ged_irq;

    // Build VGEN device children
    let hid_name = Name::new(Path::new("_HID"), &"LNRO0003");
    let cid_name = Name::new(Path::new("_CID"), &"VMGENCTR");
    let ddn_name = Name::new(Path::new("_DDN"), &"VM Generation ID");
    let sta_return = Return::new(&0xfu8);
    let sta_method = Method::new(Path::new("_STA"), 0, false, vec![&sta_return]);
    let addr_package = Package::new(vec![&guid_addr_qword, &0u64]);
    let addr_return = Return::new(&addr_package);
    let addr_method = Method::new(Path::new("ADDR"), 0, false, vec![&addr_return]);

    // VMGENID device under \_SB
    let vgen = Device::new(
        Path::new("\\_SB_.VGEN"),
        vec![
            &hid_name,
            &cid_name,
            &ddn_name,
            &sta_method,
            &addr_method,
        ],
    );

    // Build GED device children
    let ged_hid_name = Name::new(Path::new("_HID"), &"ACPI0013");
    let interrupt = Interrupt::new(true, true, false, true, ged_irq_dword);
    let ged_crs_resource = ResourceTemplate::new(vec![&interrupt]);
    let ged_crs_name = Name::new(Path::new("_CRS"), &ged_crs_resource);
    let equal_check = Equal::new(&Arg(0), &ged_irq_dword);
    let vgen_path = Path::new("\\_SB_.VGEN");
    let notify_call = Notify::new(&vgen_path, &0x80u8);
    let evt_if = If::new(&equal_check, vec![&notify_call]);
    let evt_method = Method::new(Path::new("_EVT"), 1, true, vec![&evt_if]);

    // GED device under \_SB
    let ged = Device::new(
        Path::new("\\_SB_.GED_"),
        vec![
            &ged_hid_name,
            &ged_crs_name,
            &evt_method,
        ],
    );

    // Wrap devices in \_SB scope
    let sb_scope = Scope::new(Path::new("\\_SB_"), vec![&vgen, &ged]);

    // Serialize AML to bytes
    let mut dsdt_aml = Vec::<u8>::new();
    sb_scope.to_aml_bytes(&mut dsdt_aml);

    let mut dsdt = Sdt::new(*b"DSDT", 36, 2, OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    dsdt.append_slice(&dsdt_aml);

    // FADT (FACP): revision 6, 276 bytes — built as raw Sdt with manual field writes
    // (matches cloud-hypervisor approach for known-good compatibility)
    let mut fadt = Sdt::new(*b"FACP", 276, 6, OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    // HW_REDUCED_ACPI (bit 20) only — no legacy PM hardware
    let fadt_flags: u32 = 1 << 20;
    fadt.write(FADT_FIELD_FLAGS, fadt_flags);
    // Hypervisor vendor identity
    fadt.write_bytes(FADT_FIELD_HYPERVISOR_ID, b"LIBKRUN\0");

    // MADT (APIC): describes Local APIC + I/O APIC
    // Header is 44 bytes (36 SDT header + 4 Local APIC addr + 4 flags)
    let mut madt = Sdt::new(*b"APIC", 44, 5, OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    madt.write(MADT_FIELD_LOCAL_APIC_ADDR, LOCAL_APIC_ADDR);
    // Flags: bit 0 = PCAT_COMPAT (dual 8259 present)
    madt.write(MADT_FIELD_FLAGS, 1u32);

    // Local APIC entries (8 bytes each): type(1) + len(1) + proc_id(1) + apic_id(1) + flags(4)
    for cpu_id in 0..num_cpus {
        let mut lapic_entry = [0u8; 8];
        lapic_entry[0] = ACPI_MADT_LOCAL_APIC;
        lapic_entry[1] = 8; // length
        lapic_entry[2] = cpu_id; // ACPI processor ID
        lapic_entry[3] = cpu_id; // APIC ID
        lapic_entry[4..8].copy_from_slice(&MADT_CPU_ENABLE_FLAG.to_le_bytes());
        madt.append_slice(&lapic_entry);
    }

    // I/O APIC entry (12 bytes): type(1) + len(1) + id(1) + reserved(1) + addr(4) + gsi_base(4)
    let mut ioapic_entry = [0u8; 12];
    ioapic_entry[0] = ACPI_MADT_IO_APIC;
    ioapic_entry[1] = 12; // length
    ioapic_entry[2] = 0; // I/O APIC ID
    ioapic_entry[3] = 0; // reserved
    ioapic_entry[4..8].copy_from_slice(&IO_APIC_ADDR.to_le_bytes());
    ioapic_entry[8..12].copy_from_slice(&0u32.to_le_bytes()); // GSI base
    madt.append_slice(&ioapic_entry);
    madt.update_checksum();

    // Compute addresses: RSDP at ACPI_START, then DSDT, FADT, MADT, XSDT sequentially
    let rsdp_addr = layout::ACPI_START;
    let dsdt_addr = rsdp_addr + Rsdp::len() as u64;

    // Write X_DSDT address into FADT (must be done before checksum)
    fadt.write(FADT_FIELD_X_DSDT, dsdt_addr);
    fadt.update_checksum();

    let fadt_addr = dsdt_addr + dsdt.len() as u64;
    let madt_addr = fadt_addr + fadt.len() as u64;

    // XSDT: references both FADT and MADT
    let mut xsdt = Sdt::new(*b"XSDT", 36, 1, OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    xsdt.append(fadt_addr);
    xsdt.append(madt_addr);
    xsdt.update_checksum();

    let xsdt_addr = madt_addr + madt.len() as u64;

    // Build RSDP pointing to XSDT
    let rsdp = Rsdp::new(OEM_ID, xsdt_addr);

    // Verify all tables fit within ACPI_MAX_SIZE
    let total_size = Rsdp::len() as u64 + dsdt.len() as u64 + fadt.len() as u64
        + madt.len() as u64 + xsdt.len() as u64;
    if total_size > layout::ACPI_MAX_SIZE {
        return Err(Error::Overflow);
    }

    // Write tables to guest memory: RSDP, DSDT, FADT, MADT, XSDT
    guest_mem
        .write_slice(rsdp.as_bytes(), GuestAddress(rsdp_addr))
        .map_err(|_| Error::Rsdp)?;

    guest_mem
        .write_slice(dsdt.as_slice(), GuestAddress(dsdt_addr))
        .map_err(|_| Error::Dsdt)?;

    guest_mem
        .write_slice(fadt.as_slice(), GuestAddress(fadt_addr))
        .map_err(|_| Error::Fadt)?;

    guest_mem
        .write_slice(madt.as_slice(), GuestAddress(madt_addr))
        .map_err(|_| Error::Madt)?;

    guest_mem
        .write_slice(xsdt.as_slice(), GuestAddress(xsdt_addr))
        .map_err(|_| Error::Xsdt)?;

    Ok(rsdp_addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn checksum(bytes: &[u8]) -> u8 {
        bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b))
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn read_u64(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    /// Build ACPI tables via setup_acpi_tables() and validate all table structures.
    #[test]
    fn validate_acpi_tables() {
        let guid_addr: u64 = 0x100000;
        let ged_irq: u32 = layout::GED_IRQ;
        let num_cpus: u8 = 2;

        // Create guest memory covering the ACPI region.
        let acpi_end = layout::ACPI_START + layout::ACPI_MAX_SIZE;
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(layout::ACPI_START), acpi_end as usize)])
            .expect("create guest memory");

        let rsdp_addr = setup_acpi_tables(&mem, guid_addr, ged_irq, num_cpus)
            .expect("setup_acpi_tables");
        assert_eq!(rsdp_addr, layout::ACPI_START);

        // Read RSDP from guest memory.
        let rsdp_len = Rsdp::len();
        let mut rsdp_bytes = vec![0u8; rsdp_len];
        mem.read_slice(&mut rsdp_bytes, GuestAddress(rsdp_addr)).unwrap();

        assert_eq!(&rsdp_bytes[0..8], b"RSD PTR ", "RSDP signature");
        assert_eq!(rsdp_bytes[15], 2, "RSDP revision = 2");
        assert_eq!(checksum(&rsdp_bytes[0..20]), 0, "RSDP legacy checksum");
        assert_eq!(checksum(&rsdp_bytes), 0, "RSDP extended checksum");

        let xsdt_addr = read_u64(&rsdp_bytes, 24);

        // Read XSDT header to get length, then read full table.
        let mut xsdt_hdr = [0u8; 8];
        mem.read_slice(&mut xsdt_hdr, GuestAddress(xsdt_addr)).unwrap();
        assert_eq!(&xsdt_hdr[0..4], b"XSDT", "XSDT signature");
        let xsdt_len = read_u32(&xsdt_hdr, 4) as usize;
        // XSDT = 36-byte header + two 8-byte entries (FADT + MADT)
        assert_eq!(xsdt_len, 36 + 2 * 8, "XSDT has 2 entries");

        let mut xsdt_bytes = vec![0u8; xsdt_len];
        mem.read_slice(&mut xsdt_bytes, GuestAddress(xsdt_addr)).unwrap();
        assert_eq!(checksum(&xsdt_bytes), 0, "XSDT checksum");

        let fadt_addr = read_u64(&xsdt_bytes, 36);
        let madt_addr = read_u64(&xsdt_bytes, 44);

        // Validate FADT.
        let mut fadt_hdr = [0u8; 8];
        mem.read_slice(&mut fadt_hdr, GuestAddress(fadt_addr)).unwrap();
        assert_eq!(&fadt_hdr[0..4], b"FACP", "FADT signature");
        let fadt_len = read_u32(&fadt_hdr, 4) as usize;
        assert_eq!(fadt_len, 276, "FADT length");

        let mut fadt_bytes = vec![0u8; fadt_len];
        mem.read_slice(&mut fadt_bytes, GuestAddress(fadt_addr)).unwrap();
        assert_eq!(fadt_bytes[8], 6, "FADT revision 6");
        assert_eq!(checksum(&fadt_bytes), 0, "FADT checksum");
        assert_ne!(read_u32(&fadt_bytes, FADT_FIELD_FLAGS) & (1 << 20), 0, "HW_REDUCED_ACPI set");

        let dsdt_addr = read_u64(&fadt_bytes, FADT_FIELD_X_DSDT);

        // Validate DSDT.
        let mut dsdt_hdr = [0u8; 8];
        mem.read_slice(&mut dsdt_hdr, GuestAddress(dsdt_addr)).unwrap();
        assert_eq!(&dsdt_hdr[0..4], b"DSDT", "DSDT signature");
        let dsdt_len = read_u32(&dsdt_hdr, 4) as usize;

        let mut dsdt_bytes = vec![0u8; dsdt_len];
        mem.read_slice(&mut dsdt_bytes, GuestAddress(dsdt_addr)).unwrap();
        assert_eq!(checksum(&dsdt_bytes), 0, "DSDT checksum");

        // Validate MADT.
        let mut madt_hdr = [0u8; 8];
        mem.read_slice(&mut madt_hdr, GuestAddress(madt_addr)).unwrap();
        assert_eq!(&madt_hdr[0..4], b"APIC", "MADT signature");
        let madt_len = read_u32(&madt_hdr, 4) as usize;
        // MADT = 44-byte header + num_cpus * 8 (LAPIC entries) + 12 (I/O APIC entry)
        let expected_madt_len = 44 + (num_cpus as usize) * 8 + 12;
        assert_eq!(madt_len, expected_madt_len, "MADT length");

        let mut madt_bytes = vec![0u8; madt_len];
        mem.read_slice(&mut madt_bytes, GuestAddress(madt_addr)).unwrap();
        assert_eq!(madt_bytes[8], 5, "MADT revision 5");
        assert_eq!(checksum(&madt_bytes), 0, "MADT checksum");
        assert_eq!(read_u32(&madt_bytes, MADT_FIELD_LOCAL_APIC_ADDR), LOCAL_APIC_ADDR, "LAPIC addr");
        assert_eq!(read_u32(&madt_bytes, MADT_FIELD_FLAGS), 1, "PCAT_COMPAT flag");

        // Validate LAPIC entries (starting at offset 44).
        for i in 0..num_cpus {
            let off = 44 + (i as usize) * 8;
            assert_eq!(madt_bytes[off], ACPI_MADT_LOCAL_APIC, "LAPIC entry type for CPU {i}");
            assert_eq!(madt_bytes[off + 1], 8, "LAPIC entry length");
            assert_eq!(madt_bytes[off + 2], i, "ACPI processor ID {i}");
            assert_eq!(madt_bytes[off + 3], i, "APIC ID {i}");
            assert_eq!(
                read_u32(&madt_bytes, off + 4), MADT_CPU_ENABLE_FLAG,
                "LAPIC enabled flag for CPU {i}"
            );
        }

        // Validate I/O APIC entry (after LAPIC entries).
        let ioapic_off = 44 + (num_cpus as usize) * 8;
        assert_eq!(madt_bytes[ioapic_off], ACPI_MADT_IO_APIC, "I/O APIC entry type");
        assert_eq!(madt_bytes[ioapic_off + 1], 12, "I/O APIC entry length");
        assert_eq!(read_u32(&madt_bytes, ioapic_off + 4), IO_APIC_ADDR, "I/O APIC addr");
    }
}
