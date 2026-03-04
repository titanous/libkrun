// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Magic addresses externally used to lay out x86_64 VMs.

/// Initial stack for the boot CPU.
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;

/// Kernel command line start address.
pub const CMDLINE_START: u64 = 0x20000;
/// Kernel command line start address maximum size.
pub const CMDLINE_MAX_SIZE: usize = 0x10000;
/// Kernel command line static size on SEV.
pub const CMDLINE_SEV_SIZE: usize = 0x200;
/// Initrd start address on SEV.
pub const INITRD_SEV_START: u64 = 0xa00000;

/// Start of the high memory.
pub const HIMEM_START: u64 = 0x0010_0000; //1 MB.

// Typically, on x86 systems 16 IRQs are used (0-15).
/// First usable IRQ ID for virtio device interrupts on x86_64.
pub const IRQ_BASE: u32 = 5;
/// Last usable IRQ ID for virtio device interrupts on x86_64.
/// Stops at 14 to reserve IRQ 15 for the vmgenid interrupt.
pub const IRQ_MAX: u32 = 14;

/// Address for the TSS setup.
pub const KVM_TSS_ADDRESS: u64 = 0xfffb_d000;

/// The 'zero page', a.k.a linux kernel bootparams.
pub const ZERO_PAGE_START: u64 = 0x7000;

/// SNP: space for the initial LIDT
pub const SNP_LIDT_START: u64 = 0x0;
/// SNP: Secrets page.
pub const SNP_SECRETS_START: u64 = 0x5000;
/// SNP: CPUID page
pub const SNP_CPUID_START: u64 = 0x6000;
/// SNP: FW stack and initial page tables
pub const SNP_FWDATA_START: u64 = 0x8000;
pub const SNP_FWDATA_SIZE: usize = 0x7000;

// Where BIOS/VGA magic would live on a real PC.
pub const EBDA_START: u64 = 0x9fc00;

/// Where the PC register will point after a reset.
#[cfg(not(feature = "tdx"))]
pub const RESET_VECTOR: u64 = 0xfff0;
#[cfg(feature = "tdx")]
pub const RESET_VECTOR: u64 = 0xffff_fff0;
pub const RESET_VECTOR_SEV_AP: u64 = 0xfff3;

/// The address to load the firmware, if present.
pub const FIRMWARE_START: u64 = 0xffff_0000;

/// The size of the firmware.
pub const FIRMWARE_SIZE: u64 = 65536;

/// The start of the memory area reserved for MMIO devices.
pub const FIRST_ADDR_PAST_32BITS: u64 = 1 << 32;
pub const MEM_32BIT_GAP_SIZE: u64 = 768 << 20;
pub const MMIO_MEM_START: u64 = FIRST_ADDR_PAST_32BITS - MEM_32BIT_GAP_SIZE;

/// Guest physical address for the SETUP_VMGENID setup_data node (32 bytes).
/// Placed in the BIOS ROM scan region (0xE0000–0xFFFFF), which is excluded
/// from E820 RAM entries so the kernel never allocates over it.
pub const SETUP_DATA_ADDR: u64 = 0xE0000;

/// Guest physical address of the VMGENID GUID page (4 KB).
/// Placed in the ROM expansion region, outside E820 RAM entries.
/// Address space layout (no overlaps):
///   0x9FC00..~0x9FDFF  mptable (a few hundred bytes, scales with vCPU count)
///   0xC0000..0xC0FFF   VMGENID GUID page (4 KB)
///   0xE0000..0xE001F   SETUP_VMGENID setup_data node (32 bytes)
pub const VMGENID_GUID_PAGE: u64 = 0xC0000;
/// Offset within the GUID page where the 128-bit GUID is stored.
pub const VMGENID_GUID_OFFSET: u64 = 40;

/// IRQ number used to notify the guest vmgenid driver when the GUID changes.
/// Uses IRQ 15 (secondary ATA, unused in this VM) to avoid conflict with
/// virtio devices (IRQ_BASE..IRQ_MAX = 5..14).
pub const VMGENID_IRQ: u32 = 15;

#[cfg(kani)]
mod verification {
    use super::*;

    /// Verify that key address regions in the x86_64 guest physical memory layout
    /// do not overlap each other.
    ///
    /// All checks are pure constant assertions; no symbolic inputs are required.
    /// The proof acts as a compile-time-checked specification: any future edit
    /// that accidentally creates an overlap will be caught by `just kani`.
    ///
    /// Regions verified (start inclusive, end exclusive unless noted):
    ///   VMGENID_GUID_PAGE .. +0x1000   (4 KB GUID page, 0xC0000..0xC1000)
    ///   SETUP_DATA_ADDR   .. +0x20     (32-byte setup_data node, 0xE0000..0xE0020)
    ///   EBDA_START        .. +0x400    (mptable lives here, ~0x9FC00..0xA0000)
    ///   MMIO_MEM_START    .. 4 GiB     (32-bit MMIO gap, 0xD000_0000..0x1_0000_0000)
    ///   VMGENID_IRQ                    (must not lie in IRQ_BASE..=IRQ_MAX)
    #[kani::proof]
    fn proof_layout_regions_no_overlap() {
        // ---- region extents ------------------------------------------------
        const VMGENID_GUID_PAGE_END: u64 = VMGENID_GUID_PAGE + 0x1000;
        const SETUP_DATA_END: u64 = SETUP_DATA_ADDR + 0x20;
        // mptable lives in the EBDA; reserve a conservative 1 KB for it
        const MPTABLE_START: u64 = EBDA_START;
        const MPTABLE_END: u64 = EBDA_START + 0x400;
        const HIMEM_END: u64 = FIRST_ADDR_PAST_32BITS;

        // ---- VMGENID GUID page vs SETUP_DATA_ADDR --------------------------
        // They must not overlap: one must end before the other starts.
        kani::assert(
            VMGENID_GUID_PAGE_END <= SETUP_DATA_ADDR || SETUP_DATA_END <= VMGENID_GUID_PAGE,
            "VMGENID_GUID_PAGE and SETUP_DATA_ADDR regions must not overlap",
        );

        // ---- mptable (EBDA) vs VMGENID GUID page ---------------------------
        kani::assert(
            MPTABLE_END <= VMGENID_GUID_PAGE || VMGENID_GUID_PAGE_END <= MPTABLE_START,
            "mptable (EBDA) and VMGENID_GUID_PAGE regions must not overlap",
        );

        // ---- mptable (EBDA) vs SETUP_DATA_ADDR -----------------------------
        kani::assert(
            MPTABLE_END <= SETUP_DATA_ADDR || SETUP_DATA_END <= MPTABLE_START,
            "mptable (EBDA) and SETUP_DATA_ADDR regions must not overlap",
        );

        // ---- VMGENID GUID page is below high memory (not in MMIO gap) ------
        // MMIO_MEM_START = 0xD000_0000; GUID page at 0xC0000 is well below it.
        kani::assert(
            VMGENID_GUID_PAGE_END <= MMIO_MEM_START,
            "VMGENID_GUID_PAGE must not overlap the 32-bit MMIO gap",
        );

        // ---- SETUP_DATA_ADDR is below high memory ---------------------------
        kani::assert(
            SETUP_DATA_END <= MMIO_MEM_START,
            "SETUP_DATA_ADDR must not overlap the 32-bit MMIO gap",
        );

        // ---- HIMEM_START is below MMIO gap ---------------------------------
        kani::assert(
            HIMEM_START < MMIO_MEM_START,
            "HIMEM_START must be below MMIO_MEM_START",
        );

        // ---- MMIO gap fits below 4 GiB -------------------------------------
        kani::assert(
            MMIO_MEM_START < HIMEM_END,
            "MMIO_MEM_START must be less than 4 GiB (FIRST_ADDR_PAST_32BITS)",
        );

        // ---- IRQ range sanity ----------------------------------------------
        kani::assert(
            IRQ_BASE < IRQ_MAX,
            "IRQ_BASE must be strictly less than IRQ_MAX",
        );

        // ---- VMGENID_IRQ is outside the virtio IRQ range -------------------
        // VMGENID_IRQ must not compete with virtio device IRQs.
        kani::assert(
            VMGENID_IRQ < IRQ_BASE || VMGENID_IRQ > IRQ_MAX,
            "VMGENID_IRQ must not fall within IRQ_BASE..=IRQ_MAX (virtio device range)",
        );

        // ---- CMDLINE fits below HIMEM_START --------------------------------
        kani::assert(
            CMDLINE_START + CMDLINE_MAX_SIZE as u64 <= HIMEM_START,
            "kernel cmdline region must not overlap high memory",
        );

        // ---- ZERO_PAGE_START is below CMDLINE_START ------------------------
        kani::assert(
            ZERO_PAGE_START < CMDLINE_START,
            "zero page must be below the cmdline start address",
        );

        kani::cover!(true, "layout non-overlap proof path reachable");
    }
}
