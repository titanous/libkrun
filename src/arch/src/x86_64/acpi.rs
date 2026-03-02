// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use acpi_tables::rsdp::Rsdp;
use acpi_tables::xsdt::XSDT;
use acpi_tables::sdt::Sdt;
use acpi_tables::fadt::FADTBuilder;
use acpi_tables::fadt::Flags;
use acpi_tables::{Aml, AmlSink};
use std::result;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use zerocopy::IntoBytes as _;

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
    /// ACPI tables exceed maximum size.
    Overflow,
}

pub type Result<T> = result::Result<T, Error>;

// OEM identifiers
const OEM_ID: [u8; 6] = *b"LIBKRN";
const OEM_TABLE_ID: [u8; 8] = *b"KRUNVMGN";
const OEM_REVISION: u32 = 1;

/// Simple wrapper for Vec<u8> to implement AmlSink for FADT serialization.
struct AmlBuffer(Vec<u8>);

impl AmlSink for AmlBuffer {
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

    // Compute sizes sequentially (all sizes must be known before assigning addresses)
    let rsdp_size: u64 = 36; // RSDP v2 fixed size

    // Build FADT to compute its size
    let fadt_builder = FADTBuilder::new(OEM_ID, OEM_TABLE_ID, OEM_REVISION)
        .flag(Flags::HwReducedAcpi)
        .flag(Flags::PwrButton)
        .flag(Flags::SlpButton);

    // Placeholder DSDT address (will update below once real address is computed)
    let fadt = fadt_builder.dsdt_64(0).finalize();
    let mut fadt_bytes_buffer = AmlBuffer(Vec::new());
    fadt.to_aml_bytes(&mut fadt_bytes_buffer);
    let fadt_size = fadt_bytes_buffer.0.len() as u64;

    // XSDT size is fixed: 36 byte header + 8 bytes per entry (1 FADT entry)
    let xsdt_size: u64 = 44;

    // DSDT size (SDT header only)
    let dsdt_bytes = dsdt.as_slice();
    let dsdt_size = dsdt_bytes.len() as u64;

    // Assign sequential addresses (RSDP, FADT, XSDT, DSDT)
    let rsdp_addr = layout::ACPI_START;
    let fadt_addr = rsdp_addr + rsdp_size;
    let xsdt_addr = fadt_addr + fadt_size;
    let dsdt_addr = xsdt_addr + xsdt_size;

    // Verify all tables fit within ACPI_MAX_SIZE
    let total_size = rsdp_size + fadt_size + xsdt_size + dsdt_size;
    if total_size > layout::ACPI_MAX_SIZE {
        return Err(Error::Overflow);
    }

    // Rebuild FADT with correct DSDT address
    let fadt = fadt_builder.dsdt_64(dsdt_addr).finalize();
    let mut fadt_bytes_sink = AmlBuffer(Vec::new());
    fadt.to_aml_bytes(&mut fadt_bytes_sink);

    // Build XSDT with FADT address
    let mut xsdt = XSDT::new(OEM_ID, OEM_TABLE_ID, OEM_REVISION);
    xsdt.add_entry(fadt_addr);

    // Serialize XSDT to bytes
    let mut xsdt_bytes_sink = AmlBuffer(Vec::new());
    xsdt.to_aml_bytes(&mut xsdt_bytes_sink);

    // Build RSDP pointing to XSDT
    let rsdp = Rsdp::new(OEM_ID, xsdt_addr);
    let rsdp_bytes = rsdp.as_bytes();

    // Write tables to guest memory in sequential order
    let rsdp_addr_guest = GuestAddress(rsdp_addr);
    guest_mem
        .write_slice(rsdp_bytes, rsdp_addr_guest)
        .map_err(|_| Error::Rsdp)?;

    let fadt_addr_guest = GuestAddress(fadt_addr);
    guest_mem
        .write_slice(&fadt_bytes_sink.0, fadt_addr_guest)
        .map_err(|_| Error::Fadt)?;

    let xsdt_addr_guest = GuestAddress(xsdt_addr);
    guest_mem
        .write_slice(&xsdt_bytes_sink.0, xsdt_addr_guest)
        .map_err(|_| Error::Xsdt)?;

    let dsdt_addr_guest = GuestAddress(dsdt_addr);
    guest_mem
        .write_slice(dsdt_bytes, dsdt_addr_guest)
        .map_err(|_| Error::Dsdt)?;

    Ok(())
}
