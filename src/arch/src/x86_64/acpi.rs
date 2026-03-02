// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use acpi_tables::rsdp::Rsdp;
use acpi_tables::xsdt::XSDT;
use acpi_tables::sdt::Sdt;
use acpi_tables::fadt::FADTBuilder;
use acpi_tables::fadt::Flags;
use acpi_tables::Aml;
use std::result;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use zerocopy::IntoBytes as _;

use crate::x86_64::layout;

/// Errors during ACPI table setup.
#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    /// Failure to write RSDP to memory.
    WriteRsdp,
    /// Failure to write XSDT to memory.
    WriteXsdt,
    /// Failure to write FADT to memory.
    WriteFadt,
    /// Failure to write DSDT to memory.
    WriteDsdt,
}

pub type Result<T> = result::Result<T, Error>;

// OEM identifiers
const OEM_ID: [u8; 6] = *b"LIBKRN";
const OEM_TABLE_ID: [u8; 8] = *b"KRUNVMGN";
const OEM_REVISION: u32 = 1;

/// Simple AmlSink implementation for serializing FADT to bytes.
struct AmlBytes(Vec<u8>);

impl acpi_tables::AmlSink for AmlBytes {
    fn byte(&mut self, byte: u8) {
        self.0.push(byte);
    }
}

/// Sets up minimal ACPI tables for the guest.
///
/// Generates RSDP → XSDT → FADT → DSDT in the EBDA/ROM scan region (0xE0000–0xFFFFF).
/// FADT is configured with HW_REDUCED_ACPI flag (bit 20) and revision 6.
pub fn setup_acpi_tables(guest_mem: &GuestMemoryMmap) -> Result<()> {
    // Create an empty DSDT (just the SDT header, no AML body yet).
    let dsdt = Sdt::new(*b"DSDT", 36, 2, OEM_ID, OEM_TABLE_ID, OEM_REVISION);

    // Compute sizes:
    // - DSDT: 36 bytes (SDT header only)
    // - FADT: variable but typically ~276 bytes (will compute after building)
    // - XSDT: 36 bytes header + 8 bytes per entry (1 entry = FADT)
    // - RSDP: 36 bytes (v2 structure)

    let dsdt_bytes = dsdt.as_slice();
    let dsdt_size = dsdt_bytes.len() as u64;

    // Build FADT with HW-reduced ACPI flag
    let fadt_builder = FADTBuilder::new(OEM_ID, OEM_TABLE_ID, OEM_REVISION)
        .flag(Flags::HwReducedAcpi)
        .flag(Flags::PwrButton)
        .flag(Flags::SlpButton);

    // Compute DSDT address (will be placed after FADT and XSDT)
    let rsdp_size: u64 = 36; // RSDP v2 fixed size
    let xsdt_size: u64 = 44; // SDT header (36) + 1 entry (8)
    let dsdt_addr = layout::ACPI_START + rsdp_size + xsdt_size;

    let fadt = fadt_builder.dsdt_64(dsdt_addr).finalize();

    // Serialize FADT to bytes
    let mut fadt_bytes_sink = AmlBytes(Vec::new());
    fadt.to_aml_bytes(&mut fadt_bytes_sink);
    let fadt_size = fadt_bytes_sink.0.len() as u64;

    // Compute XSDT and FADT addresses
    let fadt_addr = layout::ACPI_START + rsdp_size;
    let xsdt_addr = fadt_addr + fadt_size;

    // Verify tables fit within ACPI_MAX_SIZE
    let total_size = rsdp_size + fadt_size + xsdt_size + dsdt_size;
    if total_size > layout::ACPI_MAX_SIZE {
        return Err(Error::WriteFadt); // Generic error for overflow
    }

    // Build XSDT with FADT address
    let mut xsdt = XSDT::new(OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    xsdt.add_entry(fadt_addr);

    // Serialize XSDT to bytes
    let mut xsdt_bytes_sink = AmlBytes(Vec::new());
    xsdt.to_aml_bytes(&mut xsdt_bytes_sink);

    // Build RSDP pointing to XSDT
    let rsdp = Rsdp::new(OEM_ID, xsdt_addr);
    let rsdp_bytes = rsdp.as_bytes();

    // Write tables to guest memory in order: RSDP, FADT, XSDT, DSDT
    let rsdp_addr = GuestAddress(layout::ACPI_START);
    guest_mem
        .write_slice(rsdp_bytes, rsdp_addr)
        .map_err(|_| Error::WriteRsdp)?;

    let fadt_addr_guest = GuestAddress(fadt_addr);
    guest_mem
        .write_slice(&fadt_bytes_sink.0, fadt_addr_guest)
        .map_err(|_| Error::WriteFadt)?;

    let xsdt_addr_guest = GuestAddress(xsdt_addr);
    guest_mem
        .write_slice(&xsdt_bytes_sink.0, xsdt_addr_guest)
        .map_err(|_| Error::WriteXsdt)?;

    let dsdt_addr_guest = GuestAddress(dsdt_addr);
    guest_mem
        .write_slice(dsdt_bytes, dsdt_addr_guest)
        .map_err(|_| Error::WriteDsdt)?;

    Ok(())
}
