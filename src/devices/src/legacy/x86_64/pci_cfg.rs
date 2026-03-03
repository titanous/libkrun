// Copyright 2026 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Minimal PCI Type 1 config space handler.
//!
//! Handles I/O ports 0xCF8 (Config Address) and 0xCFC (Config Data).
//! Emulates a single PCI host bridge at bus 0, device 0, function 0
//! so the kernel's PCI sanity check passes (it looks for class 0x0600).
//! All other bus/device/function slots return 0xFFFF vendor ID (no device).

use crate::bus::BusDevice;

/// PCI config address register (port 0xCF8) is at offset 0 from base.
const ADDR_OFFSET: u64 = 0;
/// PCI config data register (port 0xCFC) is at offset 4 from base.
const DATA_OFFSET: u64 = 4;

/// Minimal 64-byte PCI config header for a host bridge at 0:0.0.
///
/// Layout (PCI Local Bus Specification 3.0, Section 6.1):
///   0x00: Vendor ID  = 0x1B36 (Red Hat, Inc.)
///   0x02: Device ID  = 0x0008 (PCIe Host bridge)
///   0x08: Revision   = 0x00
///   0x09: Prog IF    = 0x00
///   0x0A: Subclass   = 0x00 (Host bridge)
///   0x0B: Class      = 0x06 (Bridge device)
///   0x0E: Header Type = 0x00
#[rustfmt::skip]
static HOST_BRIDGE_CONFIG: [u8; 64] = [
    0x36, 0x1B, // 0x00: Vendor ID (Red Hat 0x1B36)
    0x08, 0x00, // 0x02: Device ID (0x0008)
    0x00, 0x00, // 0x04: Command
    0x00, 0x00, // 0x06: Status
    0x00,       // 0x08: Revision ID
    0x00,       // 0x09: Prog IF
    0x00,       // 0x0A: Subclass (Host bridge)
    0x06,       // 0x0B: Class (Bridge)
    0x00,       // 0x0C: Cache Line Size
    0x00,       // 0x0D: Latency Timer
    0x00,       // 0x0E: Header Type (normal)
    0x00,       // 0x0F: BIST
    // 0x10..0x3F: BARs and remaining header fields (all zero)
    0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0,
];

#[derive(Default)]
pub struct PciConfigSpace {
    /// The last value written to the config address register (port 0xCF8).
    address: u32,
}

impl PciConfigSpace {
    pub fn new() -> Self {
        Self::default()
    }
}

impl BusDevice for PciConfigSpace {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        match offset {
            ADDR_OFFSET if data.len() == 4 => {
                // Echo back the address register — this is how the kernel
                // detects PCI Type 1 config mechanism is present.
                data.copy_from_slice(&self.address.to_le_bytes());
            }
            DATA_OFFSET..=7 if !data.is_empty() => {
                if self.address & 0x8000_0000 == 0 {
                    // Enable bit not set — return all ones.
                    data.fill(0xFF);
                    return;
                }

                let bus = (self.address >> 16) & 0xFF;
                let device = (self.address >> 11) & 0x1F;
                let function = (self.address >> 8) & 0x7;

                if bus == 0 && device == 0 && function == 0 {
                    // Host bridge: return config from static header.
                    let reg_base = (self.address & 0xFC) as usize;
                    let byte_off = (offset - DATA_OFFSET) as usize;
                    for (i, d) in data.iter_mut().enumerate() {
                        let idx = reg_base + byte_off + i;
                        *d = HOST_BRIDGE_CONFIG.get(idx).copied().unwrap_or(0);
                    }
                } else {
                    // No device at this address.
                    data.fill(0xFF);
                }
            }
            _ => {
                data.fill(0xFF);
            }
        }
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if offset == ADDR_OFFSET && data.len() == 4 {
            self.address = u32::from_le_bytes(data.try_into().unwrap_or([0; 4]));
        }
        // Writes to data register (0xCFC) are silently ignored — no real devices.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type1_probe() {
        let mut pci = PciConfigSpace::new();

        // Kernel writes 0x80000000 to address register
        pci.write(0, ADDR_OFFSET, &0x8000_0000u32.to_le_bytes());

        // Kernel reads it back — must match
        let mut buf = [0u8; 4];
        pci.read(0, ADDR_OFFSET, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 0x8000_0000);
    }

    #[test]
    fn host_bridge_vendor_id() {
        let mut pci = PciConfigSpace::new();

        // Address bus 0, dev 0, func 0, offset 0 (vendor/device ID)
        pci.write(0, ADDR_OFFSET, &0x8000_0000u32.to_le_bytes());

        let mut buf = [0u8; 4];
        pci.read(0, DATA_OFFSET, &mut buf);
        let vendor = u16::from_le_bytes([buf[0], buf[1]]);
        let device = u16::from_le_bytes([buf[2], buf[3]]);
        assert_eq!(vendor, 0x1B36, "Red Hat vendor ID");
        assert_eq!(device, 0x0008, "Host bridge device ID");
    }

    #[test]
    fn host_bridge_class_code() {
        let mut pci = PciConfigSpace::new();

        // Address bus 0, dev 0, func 0, offset 0x08 (revision + class)
        pci.write(0, ADDR_OFFSET, &0x8000_0008u32.to_le_bytes());

        let mut buf = [0u8; 4];
        pci.read(0, DATA_OFFSET, &mut buf);
        // buf[2] = subclass (0x00), buf[3] = class (0x06)
        assert_eq!(buf[2], 0x00, "Host bridge subclass");
        assert_eq!(buf[3], 0x06, "Bridge device class");
    }

    #[test]
    fn no_device_at_other_slots() {
        let mut pci = PciConfigSpace::new();

        // Address bus 0, dev 1, func 0
        pci.write(0, ADDR_OFFSET, &0x8000_0800u32.to_le_bytes());

        let mut buf = [0u8; 4];
        pci.read(0, DATA_OFFSET, &mut buf);
        assert_eq!(buf, [0xFF; 4], "No device at 0:1.0");
    }

    #[test]
    fn enable_bit_required() {
        let mut pci = PciConfigSpace::new();

        // Address without enable bit
        pci.write(0, ADDR_OFFSET, &0x0000_0000u32.to_le_bytes());

        let mut buf = [0u8; 4];
        pci.read(0, DATA_OFFSET, &mut buf);
        assert_eq!(buf, [0xFF; 4], "Should return 0xFF without enable bit");
    }
}
