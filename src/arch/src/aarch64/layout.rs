// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//      ==== Address map in use in ARM development systems today ====
//
//              - 32-bit -              - 36-bit -          - 40-bit -
//1024GB    +                   +                      +-------------------+     <- 40-bit
//          |                                           | DRAM              |
//          ~                   ~                       ~                   ~
//          |                                           |                   |
//          |                                           |                   |
//          |                                           |                   |
//          |                                           |                   |
//544GB     +                   +                       +-------------------+
//          |                                           | Hole or DRAM      |
//          |                                           |                   |
//512GB     +                   +                       +-------------------+
//          |                                           |       Mapped      |
//          |                                           |       I/O         |
//          ~                   ~                       ~                   ~
//          |                                           |                   |
//256GB     +                   +                       +-------------------+
//          |                                           |       Reserved    |
//          ~                   ~                       ~                   ~
//          |                                           |                   |
//64GB      +                   +-----------------------+-------------------+   <- 36-bit
//          |                   |                   DRAM                    |
//          ~                   ~                   ~                       ~
//          |                   |                                           |
//          |                   |                                           |
//34GB      +                   +-----------------------+-------------------+
//          |                   |                  Hole or DRAM             |
//32GB      +                   +-----------------------+-------------------+
//          |                   |                   Mapped I/O              |
//          ~                   ~                       ~                   ~
//          |                   |                                           |
//16GB      +                   +-----------------------+-------------------+
//          |                   |                   Reserved                |
//          ~                   ~                       ~                   ~
//4GB       +-------------------+-----------------------+-------------------+   <- 32-bit
//          |           2GB of DRAM                                         |
//          |                                                               |
//2GB       +-------------------+-----------------------+-------------------+
//          |                           Mapped I/O                          |
//1GB       +-------------------+-----------------------+-------------------+
//          |                          ROM & RAM & I/O                      |
//0GB       +-------------------+-----------------------+-------------------+   0
//              - 32-bit -              - 36-bit -              - 40-bit -
//
// Taken from (http://infocenter.arm.com/help/topic/com.arm.doc.den0001c/DEN0001C_principles_of_arm_memory_maps.pdf).

/// Start of RAM on 64 bit ARM when loading an EFI firmware.
pub const DRAM_MEM_START_EFI: u64 = 0x4000_0000; // 1 GB.
/// Start of RAM on 64 bit ARM when loading a kernel.
pub const DRAM_MEM_START_KERNEL: u64 = 0x8000_0000; // 2 GB.
/// The maximum addressable RAM address.
pub const DRAM_MEM_END: u64 = 0x00FF_8000_0000; // 1024 - 2 = 1022 GB.
/// The maximum RAM size.
pub const DRAM_MEM_MAX_SIZE: u64 = DRAM_MEM_END - DRAM_MEM_START_KERNEL;

/// Kernel command line maximum size.
/// As per `arch/arm64/include/uapi/asm/setup.h`.
pub const CMDLINE_MAX_SIZE: usize = 2048;

/// Maximum size of the device tree blob as specified in https://www.kernel.org/doc/Documentation/arm64/booting.txt.
pub const FDT_MAX_SIZE: usize = 0x20_0000;

// As per virt/kvm/arm/vgic/vgic-kvm-device.c we need
// the number of interrupts our GIC will support to be:
// * bigger than 32
// * less than 1023 and
// * a multiple of 32.
// We are setting up our interrupt controller to support a maximum of 128 interrupts.
/// First usable interrupt on aarch64.
pub const IRQ_BASE: u32 = 32;

/// Last usable interrupt on aarch64.
pub const IRQ_MAX: u32 = 159;

/// Guest physical address of the VMGENID GUID page (4 KB).
/// Placed well below the GIC redistributor region (which grows downward
/// from 0x09FF_0000) to avoid conflicts at any vCPU count.
/// GICv3 redists reach 0x09FF_0000 - (0x20000 * vcpu_count); at 256 vCPUs
/// they'd reach 0x07FF_0000. Address 0x0800_0000 is safe for up to ~255 vCPUs.
/// This address is below DRAM start (0x8000_0000 for kernel boot, 0x4000_0000
/// for EFI boot), so it is NOT in guest RAM and not registered with UFFD.
pub const VMGENID_GUID_PAGE: u64 = 0x0800_0000;
/// Offset within the GUID page where the 128-bit GUID is stored.
pub const VMGENID_GUID_OFFSET: u64 = 40;
/// Fixed GIC SPI number for the VMGENID interrupt.
/// Allocated above the dynamic virtio SPI range (IRQ_BASE..IRQ_MAX = 32..159)
/// to avoid conflicts with virtio device allocations.
pub const VMGENID_SPI: u32 = 160;

/// Total number of interrupts to configure on the KVM GIC.
/// Must be a multiple of 32 (KVM requirement). Covers the dynamic virtio
/// range (SPIs 0-127, INTID 32-159) plus platform-reserved SPIs like
/// VMGENID_SPI (INTID 160). Value 192 supports INTIDs 0-191.
pub const GIC_NR_IRQS: u32 = 192;

/// Timer interrupts
pub const GTIMER_SEC: u32 = 13;
pub const GTIMER_HYP: u32 = 14;
pub const GTIMER_VIRT: u32 = 11;
pub const GTIMER_PHYS: u32 = 12;

pub const VTIMER_IRQ: u32 = GTIMER_VIRT + 16;

/// Below this address will reside the GIC, above this address will reside the MMIO devices.
pub const MAPPED_IO_START: u64 = 0x0a00_0000;

/// The address to put the SMBIOS contents, if present.
pub const SMBIOS_START: u64 = 0x4000_F000;

/// Where the PC register will point after a reset.
pub const RESET_VECTOR: u64 = 0x0;

/// The address to load the firmware, if present.
pub const FIRMWARE_START: u64 = 0;
