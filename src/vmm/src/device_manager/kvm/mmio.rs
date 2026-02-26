// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::{fmt, io};

#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
use devices::fdt::DeviceInfoForFDT;
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
use devices::legacy::IrqChip;
use devices::{BusDevice, DeviceType};
use kernel::cmdline as kernel_cmdline;
use kvm_ioctls::{IoEventAddress, VmFd};
#[cfg(target_arch = "aarch64")]
use utils::eventfd::EventFd;

/// Errors for MMIO device manager.
#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum Error {
    /// Failed to create MmioTransport
    CreateMmioTransport(devices::virtio::CreateMmioTransportError),
    /// Failed to perform an operation on the bus.
    BusError(devices::BusError),
    /// Appending to kernel command line failed.
    Cmdline(kernel_cmdline::Error),
    /// Failure in creating or cloning an event fd.
    EventFd(io::Error),
    /// No more IRQs are available.
    IrqsExhausted,
    /// Registering an IO Event failed.
    RegisterIoEvent(kvm_ioctls::Error),
    /// Registering an IRQ FD failed.
    RegisterIrqFd(kvm_ioctls::Error),
    /// The device couldn't be found
    DeviceNotFound,
    /// Failed to update the mmio device.
    UpdateFailed,
    /// Snapshot operation failed.
    SnapshotState(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::CreateMmioTransport(ref e) => {
                write!(f, "failed to create mmio transport for the device {e}")
            }
            Error::BusError(ref e) => write!(f, "failed to perform bus operation: {e}"),
            Error::Cmdline(ref e) => {
                write!(f, "unable to add device to kernel command line: {e}")
            }
            Error::EventFd(ref e) => write!(f, "failed to create or clone event descriptor: {e}"),
            Error::IrqsExhausted => write!(f, "no more IRQs are available"),
            Error::RegisterIoEvent(ref e) => write!(f, "failed to register IO event: {e}"),
            Error::RegisterIrqFd(ref e) => write!(f, "failed to register irqfd: {e}"),
            Error::DeviceNotFound => write!(f, "the device couldn't be found"),
            Error::UpdateFailed => write!(f, "failed to update the mmio device"),
            Error::SnapshotState(ref e) => write!(f, "snapshot operation failed: {e}"),
        }
    }
}

impl From<devices::virtio::CreateMmioTransportError> for Error {
    fn from(e: devices::virtio::CreateMmioTransportError) -> Self {
        Self::CreateMmioTransport(e)
    }
}

type Result<T> = ::std::result::Result<T, Error>;

/// This represents the size of the mmio device specified to the kernel as a cmdline option
/// It has to be larger than 0x100 (the offset where the configuration space starts from
/// the beginning of the memory mapped device registers) + the size of the configuration space
/// Currently hardcoded to 4K.
const MMIO_LEN: u64 = 0x1000;

/// Manages the complexities of registering a MMIO device.
pub struct MMIODeviceManager {
    pub bus: devices::Bus,
    mmio_base: u64,
    irq: u32,
    last_irq: u32,
    id_to_dev_info: HashMap<(DeviceType, String), MMIODeviceInfo>,
}

impl MMIODeviceManager {
    /// Create a new DeviceManager handling mmio devices (virtio net, block).
    pub fn new(mmio_base: &mut u64, irq_interval: (u32, u32)) -> MMIODeviceManager {
        if cfg!(any(target_arch = "aarch64", target_arch = "riscv64")) {
            *mmio_base += MMIO_LEN;
        }
        MMIODeviceManager {
            mmio_base: *mmio_base,
            irq: irq_interval.0,
            last_irq: irq_interval.1,
            bus: devices::Bus::new(),
            id_to_dev_info: HashMap::new(),
        }
    }

    /// Register a MMIO IOAPIC device.
    #[cfg(target_arch = "x86_64")]
    pub fn register_mmio_ioapic(
        &mut self,
        intc: Option<Arc<Mutex<devices::legacy::IrqChipDevice>>>,
    ) -> Result<()> {
        if let Some(intc) = intc {
            let (addr, size) = {
                let intc = intc.lock().unwrap();
                (intc.get_mmio_addr(), intc.get_mmio_size())
            };
            self.bus.insert(intc, addr, size).map_err(Error::BusError)?;
        }

        Ok(())
    }

    /// Returns the count of virtio devices currently registered.
    pub fn virtio_device_count(&self) -> usize {
        self.id_to_dev_info
            .keys()
            .filter(|(dt, _)| matches!(dt, DeviceType::Virtio(_)))
            .count()
    }

    /// Register an already created MMIO device to be used via MMIO transport.
    pub fn register_mmio_device(
        &mut self,
        vm: &VmFd,
        mut mmio_device: devices::virtio::MmioTransport,
        type_id: u32,
        device_id: String,
    ) -> Result<(u64, u32)> {
        if self.irq > self.last_irq {
            return Err(Error::IrqsExhausted);
        }

        for (i, queue_evt) in mmio_device.queue_evts().iter().enumerate() {
            let io_addr = IoEventAddress::Mmio(
                self.mmio_base + u64::from(devices::virtio::NOTIFY_REG_OFFSET),
            );

            vm.register_ioevent(queue_evt, &io_addr, i as u32)
                .map_err(Error::RegisterIoEvent)?;
        }

        vm.register_irqfd(mmio_device.interrupt_evt(), self.irq)
            .map_err(Error::RegisterIrqFd)?;

        mmio_device.set_irq_line(self.irq);

        self.bus
            .insert(Arc::new(Mutex::new(mmio_device)), self.mmio_base, MMIO_LEN)
            .map_err(Error::BusError)?;
        let ret = (self.mmio_base, self.irq);
        self.id_to_dev_info.insert(
            (DeviceType::Virtio(type_id), device_id),
            MMIODeviceInfo {
                addr: self.mmio_base,
                _len: MMIO_LEN,
                _irq: self.irq,
            },
        );
        self.mmio_base += MMIO_LEN;
        self.irq += 1;

        Ok(ret)
    }

    /// Append a registered MMIO device to the kernel cmdline.
    #[cfg(target_arch = "x86_64")]
    pub fn add_device_to_cmdline(
        &mut self,
        cmdline: &mut kernel_cmdline::Cmdline,
        mmio_base: u64,
        irq: u32,
    ) -> Result<()> {
        // as per doc, [virtio_mmio.]device=<size>@<baseaddr>:<irq> needs to be appended
        // to kernel commandline for virtio mmio devices to get recognized
        // the size parameter has to be transformed to KiB, so dividing hexadecimal value in
        // bytes to 1024; further, the '{}' formatting rust construct will automatically
        // transform it to decimal
        cmdline
            .insert(
                "virtio_mmio.device",
                &format!("{}K@0x{:08x}:{}", MMIO_LEN / 1024, mmio_base, irq),
            )
            .map_err(Error::Cmdline)
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    /// Register an early console at some MMIO address.
    pub fn register_mmio_serial(
        &mut self,
        vm: &VmFd,
        cmdline: &mut kernel_cmdline::Cmdline,
        intc: IrqChip,
        serial: Arc<Mutex<devices::legacy::Serial>>,
    ) -> Result<()> {
        if self.irq > self.last_irq {
            return Err(Error::IrqsExhausted);
        }

        vm.register_irqfd(serial.lock().unwrap().interrupt_evt(), self.irq)
            .map_err(Error::RegisterIrqFd)?;

        serial.lock().unwrap().set_intc(intc);

        self.bus
            .insert(serial, self.mmio_base, MMIO_LEN)
            .map_err(Error::BusError)?;

        cmdline
            .insert(
                "earlycon",
                #[cfg(target_arch = "aarch64")]
                &format!("pl011,mmio32,0x{:08x}", self.mmio_base),
                #[cfg(target_arch = "riscv64")]
                &format!("uart,mmio,0x{:08x}", self.mmio_base),
            )
            .map_err(Error::Cmdline)?;

        let ret = self.mmio_base;
        self.id_to_dev_info.insert(
            (DeviceType::Serial, DeviceType::Serial.to_string()),
            MMIODeviceInfo {
                addr: ret,
                _len: MMIO_LEN,
                _irq: self.irq,
            },
        );

        self.mmio_base += MMIO_LEN;
        self.irq += 1;

        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    /// Register a MMIO RTC device.
    pub fn register_mmio_rtc(&mut self, vm: &VmFd) -> Result<()> {
        if self.irq > self.last_irq {
            return Err(Error::IrqsExhausted);
        }

        // Attaching the RTC device.
        let rtc_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK).map_err(Error::EventFd)?;
        let device = devices::legacy::RTC::new(rtc_evt.try_clone().map_err(Error::EventFd)?);
        vm.register_irqfd(&rtc_evt, self.irq)
            .map_err(Error::RegisterIrqFd)?;

        self.bus
            .insert(Arc::new(Mutex::new(device)), self.mmio_base, MMIO_LEN)
            .map_err(Error::BusError)?;

        let ret = self.mmio_base;
        self.id_to_dev_info.insert(
            (DeviceType::RTC, "rtc".to_string()),
            MMIODeviceInfo {
                addr: ret,
                _len: MMIO_LEN,
                _irq: self.irq,
            },
        );

        self.mmio_base += MMIO_LEN;
        self.irq += 1;

        Ok(())
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    /// Gets the information of the devices registered up to some point in time.
    pub fn get_device_info(&self) -> &HashMap<(DeviceType, String), MMIODeviceInfo> {
        &self.id_to_dev_info
    }

    /// Gets the the specified device.
    pub fn get_device(
        &self,
        device_type: DeviceType,
        device_id: &str,
    ) -> Option<&Mutex<dyn BusDevice>> {
        if let Some(dev_info) = self
            .id_to_dev_info
            .get(&(device_type, device_id.to_string()))
        {
            if let Some((_, device)) = self.bus.get_device(dev_info.addr) {
                return Some(device);
            }
        }
        None
    }

    /// Save all snapshottable device states.
    #[cfg(feature = "snapshot")]
    pub fn save_all_device_states(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut states = Vec::new();

        for ((device_type, device_id), dev_info) in &self.id_to_dev_info {
            let Some((_, device)) = self.bus.get_device(dev_info.addr) else {
                return Err(Error::SnapshotState(format!(
                    "Device {device_type}:{device_id} missing from bus"
                )));
            };

            let device = match device.lock() {
                Ok(device) => device,
                Err(e) => {
                    return Err(Error::SnapshotState(format!(
                        "Failed to lock device {device_type}:{device_id}: {e}"
                    )));
                }
            };
            if let Some(snapshottable) = device.as_snapshottable() {
                let state = match snapshottable.save_state() {
                    Ok(state) => state,
                    Err(e) => {
                        return Err(Error::SnapshotState(format!(
                            "Failed to save state for {device_type}:{device_id}: {e}"
                        )));
                    }
                };
                let id = format!("{device_type}:{device_id}");
                states.push((id, state));
            } else {
                debug!(
                    "Skipping non-snapshottable device during snapshot save: {device_type}:{device_id}"
                );
            }
        }
        Ok(states)
    }

    /// Quiesce all device workers before snapshot/restore.
    #[cfg(feature = "snapshot")]
    pub fn quiesce_all_device_workers(&self, timeout: std::time::Duration) -> Result<()> {
        for ((device_type, device_id), dev_info) in &self.id_to_dev_info {
            let Some((_, device)) = self.bus.get_device(dev_info.addr) else {
                continue;
            };
            let device = match device.lock() {
                Ok(device) => device,
                Err(e) => {
                    return Err(Error::SnapshotState(format!(
                        "Failed to lock device {device_type}:{device_id} for quiesce: {e}"
                    )));
                }
            };
            if let Err(e) = device.quiesce_workers(timeout) {
                return Err(Error::SnapshotState(format!(
                    "Failed to quiesce workers for {device_type}:{device_id}: {e}"
                )));
            }
        }
        Ok(())
    }

    /// Resume all device workers after snapshot/restore.
    #[cfg(feature = "snapshot")]
    pub fn resume_all_device_workers(&self) {
        for dev_info in self.id_to_dev_info.values() {
            if let Some((_, device)) = self.bus.get_device(dev_info.addr) {
                if let Ok(device) = device.lock() {
                    device.resume_workers();
                }
            }
        }
    }

    /// Complete device restore (activate devices, spawn worker threads).
    #[cfg(feature = "snapshot")]
    pub fn complete_all_device_restores(&self) -> Result<()> {
        for ((device_type, device_id), dev_info) in &self.id_to_dev_info {
            let Some((_, device)) = self.bus.get_device(dev_info.addr) else {
                continue;
            };
            let mut device = match device.lock() {
                Ok(device) => device,
                Err(e) => {
                    return Err(Error::SnapshotState(format!(
                        "Failed to lock device {device_type}:{device_id} for complete_restore: {e}"
                    )));
                }
            };
            if let Err(e) = device.complete_restore() {
                return Err(Error::SnapshotState(format!(
                    "Failed to complete restore for {device_type}:{device_id}: {e}"
                )));
            }
        }
        Ok(())
    }

    /// Restore all snapshottable device states.
    #[cfg(feature = "snapshot")]
    pub fn restore_all_device_states(&self, states: &[(String, Vec<u8>)]) -> Result<()> {
        for (id, data) in states {
            let mut found = false;
            for ((device_type, device_id), dev_info) in &self.id_to_dev_info {
                let expected_id = format!("{device_type}:{device_id}");
                if &expected_id == id {
                    let Some((_, device)) = self.bus.get_device(dev_info.addr) else {
                        return Err(Error::SnapshotState(format!(
                            "Device {id} missing from bus during restore"
                        )));
                    };
                    let mut device = match device.lock() {
                        Ok(device) => device,
                        Err(e) => {
                            return Err(Error::SnapshotState(format!(
                                "Failed to lock device {id} during restore: {e}"
                            )));
                        }
                    };
                    let Some(snapshottable) = device.as_snapshottable_mut() else {
                        return Err(Error::SnapshotState(format!(
                            "Device {id} does not support snapshot restore"
                        )));
                    };
                    if let Err(e) = snapshottable.restore_state(data) {
                        return Err(Error::SnapshotState(format!(
                            "Failed to restore state for device {id}: {e}"
                        )));
                    }

                    found = true;
                    break;
                }
            }
            if !found {
                // Unknown device state — skip silently (forward compat for device manager layering).
                // This allows PortIO states to coexist in the shared device_states vec.
                debug!("Skipping unknown MMIO device state: {id}");
            }
        }
        Ok(())
    }

    /// Collect used ring page ranges for all registered virtio devices.
    ///
    /// Iterates through all MMIO devices, downcasts to MmioTransport, and collects
    /// the used ring page ranges from each active device. Returns a vector of
    /// (page_addr, page_size) tuples.
    #[cfg(feature = "snapshot")]
    pub fn get_virtio_used_ring_ranges(&self) -> Vec<(u64, u64)> {
        let mut ranges = Vec::new();

        for ((_device_type, _device_id), dev_info) in &self.id_to_dev_info {
            let Some((_, device)) = self.bus.get_device(dev_info.addr) else {
                continue;
            };
            let Ok(device) = device.lock() else {
                continue;
            };

            // Downcast BusDevice to MmioTransport
            let Some(transport) = device
                .as_any()
                .downcast_ref::<devices::virtio::MmioTransport>()
            else {
                continue;
            };

            ranges.extend(transport.get_used_ring_ranges());
        }

        ranges
    }
}

/// Private structure for storing information about the MMIO device registered at some address on the bus.
#[derive(Clone, Debug)]
pub struct MMIODeviceInfo {
    addr: u64,
    _irq: u32,
    _len: u64,
}

#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
impl DeviceInfoForFDT for MMIODeviceInfo {
    fn addr(&self) -> u64 {
        self.addr
    }
    fn irq(&self) -> u32 {
        self._irq
    }
    fn length(&self) -> u64 {
        self._len
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::super::builder;
    use super::*;
    use arch;
    use devices::legacy::DummyIrqChip;
    #[cfg(target_arch = "aarch64")]
    use devices::legacy::KvmGicV3;
    #[cfg(target_arch = "x86_64")]
    use devices::legacy::KvmIoapic;
    use devices::virtio::{
        ActivateResult, DeviceQueue, InterruptTransport, QueueConfig, VirtioDevice,
    };
    use std::sync::Arc;
    use utils::errno;
    use vm_memory::{GuestAddress, GuestMemoryMmap};

    const QUEUE_CONFIG: &[QueueConfig] = &[QueueConfig::new(64)];

    impl MMIODeviceManager {
        fn register_virtio_device(
            &mut self,
            vm: &VmFd,
            guest_mem: GuestMemoryMmap,
            device: Arc<Mutex<dyn devices::virtio::VirtioDevice>>,
            _cmdline: &mut kernel_cmdline::Cmdline,
            type_id: u32,
            device_id: &str,
        ) -> Result<u64> {
            let mmio_device =
                devices::virtio::MmioTransport::new(guest_mem, DummyIrqChip::new().into(), device)
                    .unwrap();
            let (mmio_base, _irq) =
                self.register_mmio_device(vm, mmio_device, type_id, device_id.to_string())?;
            #[cfg(target_arch = "x86_64")]
            self.add_device_to_cmdline(_cmdline, mmio_base, _irq)?;
            Ok(mmio_base)
        }
    }

    #[allow(dead_code)]
    struct DummyDevice {
        dummy: u32,
    }

    impl DummyDevice {
        pub fn new() -> Self {
            DummyDevice { dummy: 0 }
        }
    }

    impl devices::virtio::VirtioDevice for DummyDevice {
        fn avail_features(&self) -> u64 {
            0
        }

        fn acked_features(&self) -> u64 {
            0
        }

        fn set_acked_features(&mut self, _: u64) {}

        fn device_type(&self) -> u32 {
            0
        }

        fn device_name(&self) -> &str {
            "dummy"
        }

        fn queue_config(&self) -> &[QueueConfig] {
            QUEUE_CONFIG
        }

        fn read_config(&self, offset: u64, data: &mut [u8]) {
            let _ = offset;
            let _ = data;
        }

        fn write_config(&mut self, offset: u64, data: &[u8]) {
            let _ = offset;
            let _ = data;
        }

        fn activate(
            &mut self,
            _mem: GuestMemoryMmap,
            _intc: InterruptTransport,
            _queues: Vec<DeviceQueue>,
        ) -> ActivateResult {
            Ok(())
        }

        fn is_activated(&self) -> bool {
            false
        }
    }

    #[test]
    fn test_register_virtio_device() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x1000);
        let guest_mem =
            GuestMemoryMmap::from_ranges(&[(start_addr1, 0x1000), (start_addr2, 0x1000)]).unwrap();
        let vm = builder::setup_vm(&guest_mem, false).unwrap();
        let mut device_manager =
            MMIODeviceManager::new(&mut 0xd000_0000, (arch::IRQ_BASE, arch::IRQ_MAX));
        #[cfg(target_arch = "x86_64")]
        let _kvmioapic = KvmIoapic::new(vm.fd()).unwrap();
        #[cfg(target_arch = "aarch64")]
        let _gic = KvmGicV3::new(vm.fd(), 1).unwrap();

        let mut cmdline = kernel_cmdline::Cmdline::new(4096);
        let dummy = Arc::new(Mutex::new(DummyDevice::new()));

        assert!(device_manager
            .register_virtio_device(vm.fd(), guest_mem, dummy, &mut cmdline, 0, "dummy")
            .is_ok());
    }

    #[test]
    fn test_register_too_many_devices() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x1000);
        let guest_mem =
            GuestMemoryMmap::from_ranges(&[(start_addr1, 0x1000), (start_addr2, 0x1000)]).unwrap();
        let vm = builder::setup_vm(&guest_mem, false).unwrap();
        let mut device_manager =
            MMIODeviceManager::new(&mut 0xd000_0000, (arch::IRQ_BASE, arch::IRQ_MAX));
        #[cfg(target_arch = "x86_64")]
        let _kvmioapic = KvmIoapic::new(vm.fd()).unwrap();
        #[cfg(target_arch = "aarch64")]
        let _gic = KvmGicV3::new(vm.fd(), 1).unwrap();

        let mut cmdline = kernel_cmdline::Cmdline::new(4096);

        for _i in arch::IRQ_BASE..=arch::IRQ_MAX {
            device_manager
                .register_virtio_device(
                    vm.fd(),
                    guest_mem.clone(),
                    Arc::new(Mutex::new(DummyDevice::new())),
                    &mut cmdline,
                    0,
                    "dummy1",
                )
                .unwrap();
        }
        assert_eq!(
            format!(
                "{}",
                device_manager
                    .register_virtio_device(
                        vm.fd(),
                        guest_mem,
                        Arc::new(Mutex::new(DummyDevice::new())),
                        &mut cmdline,
                        0,
                        "dummy2"
                    )
                    .unwrap_err()
            ),
            "no more IRQs are available".to_string()
        );
    }

    #[test]
    fn test_dummy_device() {
        let dummy = DummyDevice::new();
        assert_eq!(dummy.device_type(), 0);
        assert_eq!(dummy.queue_config().len(), QUEUE_CONFIG.len());
    }

    #[test]
    fn test_error_messages() {
        let device_manager =
            MMIODeviceManager::new(&mut 0xd000_0000, (arch::IRQ_BASE, arch::IRQ_MAX));
        let mut cmdline = kernel_cmdline::Cmdline::new(4096);
        let e = Error::Cmdline(
            cmdline
                .insert(
                    "virtio_mmio=device",
                    &format!(
                        "{}K@0x{:08x}:{}",
                        MMIO_LEN / 1024,
                        device_manager.mmio_base,
                        device_manager.irq
                    ),
                )
                .unwrap_err(),
        );
        assert_eq!(
            format!("{e}"),
            format!(
                "unable to add device to kernel command line: {}",
                kernel_cmdline::Error::HasEquals
            ),
        );
        assert_eq!(
            format!("{}", Error::UpdateFailed),
            "failed to update the mmio device"
        );
        assert_eq!(
            format!("{}", Error::BusError(devices::BusError::Overlap)),
            format!(
                "failed to perform bus operation: {}",
                devices::BusError::Overlap
            )
        );
        assert_eq!(
            format!("{}", Error::IrqsExhausted),
            "no more IRQs are available"
        );
        assert_eq!(
            format!("{}", Error::RegisterIoEvent(errno::Error::new(0))),
            format!("failed to register IO event: {}", errno::Error::new(0))
        );
        assert_eq!(
            format!("{}", Error::RegisterIrqFd(errno::Error::new(0))),
            format!("failed to register irqfd: {}", errno::Error::new(0))
        );
    }

    #[test]
    fn test_device_info() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x1000);
        let guest_mem =
            GuestMemoryMmap::from_ranges(&[(start_addr1, 0x1000), (start_addr2, 0x1000)]).unwrap();
        let vm = builder::setup_vm(&guest_mem, false).unwrap();
        let mut device_manager =
            MMIODeviceManager::new(&mut 0xd000_0000, (arch::IRQ_BASE, arch::IRQ_MAX));
        let mut cmdline = kernel_cmdline::Cmdline::new(4096);
        let dummy = Arc::new(Mutex::new(DummyDevice::new()));

        let type_id = 0;
        let id = String::from("foo");
        if let Ok(addr) = device_manager.register_virtio_device(
            vm.fd(),
            guest_mem,
            dummy,
            &mut cmdline,
            type_id,
            &id,
        ) {
            assert!(device_manager
                .get_device(DeviceType::Virtio(type_id), &id)
                .is_some());
            assert_eq!(
                addr,
                device_manager.id_to_dev_info[&(DeviceType::Virtio(type_id), id.clone())].addr
            );
            assert_eq!(
                arch::IRQ_BASE,
                device_manager.id_to_dev_info[&(DeviceType::Virtio(type_id), id.clone())]._irq
            );
        }
        let id = "bar";
        assert!(device_manager
            .get_device(DeviceType::Virtio(type_id), id)
            .is_none());
    }

    #[cfg(feature = "snapshot")]
    mod snapshot_tests {
        use super::*;
        use devices::virtio::Queue;

        /// Mock device with configurable queues for testing dirty ring ranges.
        struct MockDeviceWithQueues {
            queues: Vec<Queue>,
        }

        impl MockDeviceWithQueues {
            fn new(queues: Vec<Queue>) -> Self {
                MockDeviceWithQueues { queues }
            }
        }

        impl devices::virtio::VirtioDevice for MockDeviceWithQueues {
            fn avail_features(&self) -> u64 {
                0
            }

            fn acked_features(&self) -> u64 {
                0
            }

            fn set_acked_features(&mut self, _: u64) {}

            fn device_type(&self) -> u32 {
                0
            }

            fn device_name(&self) -> &str {
                "mock"
            }

            fn queue_config(&self) -> &[QueueConfig] {
                QUEUE_CONFIG
            }

            fn read_config(&self, _offset: u64, _data: &mut [u8]) {}

            fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

            fn activate(
                &mut self,
                _mem: GuestMemoryMmap,
                _intc: InterruptTransport,
                _queues: Vec<DeviceQueue>,
            ) -> ActivateResult {
                Ok(())
            }

            fn is_activated(&self) -> bool {
                false
            }

            fn queues(&self) -> &[Queue] {
                &self.queues
            }
        }

        #[test]
        fn test_get_used_ring_ranges_ac21_active_queues() {
            let start_addr = GuestAddress(0x0);
            let guest_mem = GuestMemoryMmap::from_ranges(&[(start_addr, 0x10000)]).unwrap();

            // Create a queue with used_ring at a known address
            // used_ring at 0x5000 (page-aligned)
            let mut queue = Queue::new(64);
            queue.ready = true;
            queue.used_ring = GuestAddress(0x5000);
            queue.size = 64;

            let mock_device = MockDeviceWithQueues::new(vec![queue]);
            let device: Arc<Mutex<dyn devices::virtio::VirtioDevice>> =
                Arc::new(Mutex::new(mock_device));

            let mmio_transport =
                devices::virtio::MmioTransport::new(guest_mem, DummyIrqChip::new().into(), device)
                    .unwrap();

            // AC2.1: Active queue should produce correct page-aligned ranges
            let ranges = mmio_transport.get_used_ring_ranges();

            // used_ring at 0x5000, size = 6 + 8*64 = 518 bytes
            // 0x5000 to 0x5206 spans one page (0x5000)
            assert!(
                !ranges.is_empty(),
                "Should have at least one range for active queue"
            );
            assert_eq!(ranges[0].0, 0x5000, "Range start should be page-aligned");
            assert_eq!(ranges[0].1, 4096, "Range size should be page size");
        }

        #[test]
        fn test_get_used_ring_ranges_multiple_pages() {
            let start_addr = GuestAddress(0x0);
            let guest_mem = GuestMemoryMmap::from_ranges(&[(start_addr, 0x10000)]).unwrap();

            // Create a queue with used_ring at 0x4f00 with size 64
            // This spans two pages: 0x4000-0x4fff and 0x5000-0x5fff
            let mut queue = Queue::new(64);
            queue.ready = true;
            queue.used_ring = GuestAddress(0x4f00);
            queue.size = 64;

            let mock_device = MockDeviceWithQueues::new(vec![queue]);
            let device: Arc<Mutex<dyn devices::virtio::VirtioDevice>> =
                Arc::new(Mutex::new(mock_device));

            let mmio_transport =
                devices::virtio::MmioTransport::new(guest_mem, DummyIrqChip::new().into(), device)
                    .unwrap();

            let ranges = mmio_transport.get_used_ring_ranges();

            // Should have two ranges: one for 0x4000 and one for 0x5000
            assert_eq!(ranges.len(), 2, "Should have two page ranges");
            assert_eq!(ranges[0].0, 0x4000, "First page start should be 0x4000");
            assert_eq!(ranges[0].1, 4096);
            assert_eq!(ranges[1].0, 0x5000, "Second page start should be 0x5000");
            assert_eq!(ranges[1].1, 4096);
        }

        #[test]
        fn test_get_used_ring_ranges_ac23_inactive_queues() {
            let start_addr = GuestAddress(0x0);
            let guest_mem = GuestMemoryMmap::from_ranges(&[(start_addr, 0x10000)]).unwrap();

            // Create an inactive queue (ready=false)
            let mut queue_inactive = Queue::new(64);
            queue_inactive.ready = false;
            queue_inactive.used_ring = GuestAddress(0x5000);
            queue_inactive.size = 64;

            // Create an uninitialized queue (used_ring=0)
            let queue_uninitialized = Queue::new(64);

            let mock_device = MockDeviceWithQueues::new(vec![queue_inactive, queue_uninitialized]);
            let device: Arc<Mutex<dyn devices::virtio::VirtioDevice>> =
                Arc::new(Mutex::new(mock_device));

            let mmio_transport =
                devices::virtio::MmioTransport::new(guest_mem, DummyIrqChip::new().into(), device)
                    .unwrap();

            // AC2.3: Inactive queues should not appear in ranges, no crash
            let ranges = mmio_transport.get_used_ring_ranges();
            assert!(
                ranges.is_empty(),
                "Inactive queues should not produce ranges"
            );
        }

        #[test]
        fn test_get_used_ring_ranges_mixed_active_inactive() {
            let start_addr = GuestAddress(0x0);
            let guest_mem = GuestMemoryMmap::from_ranges(&[(start_addr, 0x10000)]).unwrap();

            // Create active queue
            let mut queue_active = Queue::new(64);
            queue_active.ready = true;
            queue_active.used_ring = GuestAddress(0x3000);
            queue_active.size = 64;

            // Create inactive queue
            let mut queue_inactive = Queue::new(64);
            queue_inactive.ready = false;
            queue_inactive.used_ring = GuestAddress(0x5000);
            queue_inactive.size = 64;

            let mock_device = MockDeviceWithQueues::new(vec![queue_active, queue_inactive]);
            let device: Arc<Mutex<dyn devices::virtio::VirtioDevice>> =
                Arc::new(Mutex::new(mock_device));

            let mmio_transport =
                devices::virtio::MmioTransport::new(guest_mem, DummyIrqChip::new().into(), device)
                    .unwrap();

            let ranges = mmio_transport.get_used_ring_ranges();

            // Should only have range for active queue at 0x3000
            assert_eq!(
                ranges.len(),
                1,
                "Should only have one range for active queue"
            );
            assert_eq!(ranges[0].0, 0x3000, "Range should be for active queue");
            assert_eq!(ranges[0].1, 4096);
        }
    }
}
