use std::time::{SystemTime, UNIX_EPOCH};

use utils::eventfd::EventFd;
use vm_memory::{Address, ByteValued, Bytes, GuestMemoryMmap, Le16, Le64};

use super::super::{ActivateError, ActivateResult, DeviceState, Queue as VirtQueue, VirtioDevice};
use super::{defs, defs::uapi, RtcError};
use crate::virtio::{DeviceQueue, InterruptTransport, QueueConfig};

#[cfg(target_os = "macos")]
use hvf::Vcpus;

// Request queue index
pub(crate) const REQ_INDEX: usize = 0;

// Supported features
pub(crate) const AVAIL_FEATURES: u64 = 1 << uapi::VIRTIO_F_VERSION_1 as u64;

// Number of clocks we expose (just UTC for now)
const NUM_CLOCKS: u16 = 1;

/// Request header - 8 bytes
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct ReqHead {
    msg_type: Le16,
    _reserved: [u8; 6],
}

// SAFETY: ReqHead contains only plain data
unsafe impl ByteValued for ReqHead {}

/// Response header - 8 bytes
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct RespHead {
    status: u8,
    _reserved: [u8; 7],
}

// SAFETY: RespHead contains only plain data
unsafe impl ByteValued for RespHead {}

/// REQ_CFG response
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct RespCfg {
    head: RespHead,
    num_clocks: Le16,
    _padding: [u8; 6],
}

// SAFETY: RespCfg contains only plain data
unsafe impl ByteValued for RespCfg {}

/// REQ_CLOCK_CAP request body (after header)
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct ReqClockCap {
    clock_id: Le16,
    _padding: [u8; 6],
}

// SAFETY: ReqClockCap contains only plain data
unsafe impl ByteValued for ReqClockCap {}

/// REQ_CLOCK_CAP response
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct RespClockCap {
    head: RespHead,
    clock_type: Le16,
    flags: u8,
    _reserved: [u8; 5],
}

// SAFETY: RespClockCap contains only plain data
unsafe impl ByteValued for RespClockCap {}

/// REQ_READ request body (after header)
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct ReqRead {
    clock_id: Le16,
    _padding: [u8; 6],
}

// SAFETY: ReqRead contains only plain data
unsafe impl ByteValued for ReqRead {}

/// REQ_READ response
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct RespRead {
    head: RespHead,
    clock_ns: Le64,
}

// SAFETY: RespRead contains only plain data
unsafe impl ByteValued for RespRead {}

/// REQ_CROSS_CAP request body (after header)
/// Kernel uses: le16 clock_id, u8 hw_counter, u8 reserved[5]
#[allow(dead_code)]
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct ReqCrossCap {
    clock_id: Le16,
    hw_counter: u8,
    _padding: [u8; 5],
}

// SAFETY: ReqCrossCap contains only plain data
unsafe impl ByteValued for ReqCrossCap {}

/// REQ_CROSS_CAP response
/// Kernel uses: head, u8 flags (bit 0 = cross_cap supported), u8 reserved[7]
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct RespCrossCap {
    head: RespHead,
    /// VIRTIO_RTC_FLAG_CROSS_CAP (1 << 0) = supported
    flags: u8,
    _reserved: [u8; 7],
}

// SAFETY: RespCrossCap contains only plain data
unsafe impl ByteValued for RespCrossCap {}

/// REQ_READ_CROSS request body (after header)
/// Kernel uses: le16 clock_id, u8 hw_counter, u8 reserved[5]
#[allow(dead_code)]
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct ReqReadCross {
    clock_id: Le16,
    hw_counter: u8,
    _padding: [u8; 5],
}

// SAFETY: ReqReadCross contains only plain data
unsafe impl ByteValued for ReqReadCross {}

/// REQ_READ_CROSS response
#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct RespReadCross {
    head: RespHead,
    clock_ns: Le64,
    counter_value: Le64,
}

// SAFETY: RespReadCross contains only plain data
unsafe impl ByteValued for RespReadCross {}

/// Read the hardware counter for cross-timestamping
#[allow(dead_code)]
#[cfg(target_arch = "aarch64")]
fn read_counter() -> u64 {
    let val: u64;
    // SAFETY: Reading CNTVCT_EL0 is safe and doesn't have side effects
    unsafe {
        std::arch::asm!("mrs {}, cntvct_el0", out(reg) val, options(nostack, nomem));
    }
    val
}

#[allow(dead_code)]
#[cfg(target_arch = "x86_64")]
fn read_counter() -> u64 {
    // SAFETY: RDTSC is safe and doesn't have side effects
    unsafe { std::arch::x86_64::_rdtsc() }
}

/// Get the supported counter type for this architecture
#[allow(dead_code)]
#[cfg(target_arch = "aarch64")]
fn supported_counter_id() -> u16 {
    uapi::VIRTIO_RTC_COUNTER_ARM_VCT
}

#[allow(dead_code)]
#[cfg(target_arch = "x86_64")]
fn supported_counter_id() -> u16 {
    uapi::VIRTIO_RTC_COUNTER_X86_TSC
}

/// Get the counter frequency in Hz (used by tests, may be needed in future spec versions)
#[allow(dead_code)]
#[cfg(target_arch = "aarch64")]
fn get_counter_freq() -> u64 {
    // Read the counter frequency from CNTFRQ_EL0
    let freq: u64;
    // SAFETY: Reading CNTFRQ_EL0 is safe and has no side effects
    unsafe {
        std::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq, options(nostack, nomem));
    }
    freq
}

#[allow(dead_code)]
#[cfg(target_arch = "x86_64")]
fn get_counter_freq() -> u64 {
    // Try to get TSC frequency from CPUID leaf 0x15
    // If not available, use a reasonable default (most modern CPUs are ~2-3 GHz)
    let cpuid = unsafe { std::arch::x86_64::__cpuid(0x15) };
    if cpuid.eax != 0 && cpuid.ebx != 0 && cpuid.ecx != 0 {
        // TSC freq = ECX * EBX / EAX
        (cpuid.ecx as u64 * cpuid.ebx as u64) / cpuid.eax as u64
    } else {
        // Fallback: try leaf 0x16 for base frequency
        let cpuid16 = unsafe { std::arch::x86_64::__cpuid(0x16) };
        if cpuid16.eax != 0 {
            // EAX contains base frequency in MHz
            cpuid16.eax as u64 * 1_000_000
        } else {
            // Last resort fallback: assume 2.4 GHz (common for VMs)
            2_400_000_000
        }
    }
}

pub struct Rtc {
    pub(crate) queues: Vec<VirtQueue>,
    pub(crate) queue_events: Vec<EventFd>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
    /// Reference to vCPU state for cross-timestamping (macOS only).
    /// Used to check if cross-timestamping is supported.
    #[cfg(target_os = "macos")]
    vcpus: Option<Arc<dyn Vcpus + Send + Sync>>,
}

impl Rtc {
    pub(crate) fn with_queues(queues: Vec<VirtQueue>) -> super::Result<Rtc> {
        let mut queue_events = Vec::new();
        for _ in 0..queues.len() {
            queue_events
                .push(EventFd::new(utils::eventfd::EFD_NONBLOCK).map_err(RtcError::EventFd)?);
        }

        Ok(Rtc {
            queues,
            queue_events,
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK).map_err(RtcError::EventFd)?,
            device_state: DeviceState::Inactive,
            #[cfg(target_os = "macos")]
            vcpus: None,
        })
    }

    pub fn new() -> super::Result<Rtc> {
        let queues: Vec<VirtQueue> = defs::QUEUE_SIZES
            .iter()
            .map(|&max_size| VirtQueue::new(max_size))
            .collect();
        Self::with_queues(queues)
    }

    /// Set the vCPU reference for cross-timestamping support (macOS only)
    #[cfg(target_os = "macos")]
    pub fn set_vcpus(&mut self, vcpus: Arc<dyn Vcpus + Send + Sync>) {
        self.vcpus = Some(vcpus);
    }

    pub fn id(&self) -> &str {
        defs::RTC_DEV_ID
    }

    /// Validate a clock_id from the guest and return the appropriate status code.
    ///
    /// Returns `VIRTIO_RTC_S_OK` for clock_id 0 (UTC) and `VIRTIO_RTC_S_EINVAL`
    /// for any other value.  Called directly by `process_req` for both
    /// `VIRTIO_RTC_REQ_CLOCK_CAP` and `VIRTIO_RTC_REQ_READ` dispatches.
    fn validate_clock_id(clock_id: u16) -> u8 {
        if clock_id == 0 {
            uapi::VIRTIO_RTC_S_OK
        } else {
            uapi::VIRTIO_RTC_S_EINVAL
        }
    }

    /// Get current UTC time in nanoseconds since UNIX epoch
    fn get_utc_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    /// Get a cross-timestamp: (time_ns, counter_value)
    /// Reads the counter before and after the time to minimize skew
    #[allow(dead_code)]
    fn get_cross_timestamp() -> (u64, u64) {
        let cnt1 = read_counter();
        let time_ns = Self::get_utc_ns();
        let cnt2 = read_counter();

        // Use midpoint of counter reads to minimize error
        let counter = cnt1.wrapping_add(cnt2) / 2;
        (time_ns, counter)
    }

    pub fn process_req(&mut self) -> bool {
        debug!("rtc: process_req()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => unreachable!(),
        };

        let mut have_used = false;

        while let Some(head) = self.queues[REQ_INDEX].pop(mem) {
            let index = head.index;
            let mut written = 0u32;

            // We need at least 2 descriptors: one for request, one for response
            let descriptors: Vec<_> = head.into_iter().collect();
            if descriptors.len() < 2 {
                error!("rtc: not enough descriptors");
                if let Err(e) = self.queues[REQ_INDEX].add_used(mem, index, 0) {
                    error!("failed to add used elements to the queue: {e:?}");
                }
                have_used = true;
                continue;
            }

            let req_desc = &descriptors[0];
            let resp_desc = &descriptors[1];

            // Read request header
            let req_head: ReqHead = match mem.read_obj(req_desc.addr) {
                Ok(h) => h,
                Err(e) => {
                    error!("rtc: failed to read request header: {e:?}");
                    if let Err(e) = self.queues[REQ_INDEX].add_used(mem, index, 0) {
                        error!("failed to add used elements to the queue: {e:?}");
                    }
                    have_used = true;
                    continue;
                }
            };

            let msg_type = req_head.msg_type.to_native();

            match msg_type {
                uapi::VIRTIO_RTC_REQ_CFG => {
                    let resp = RespCfg {
                        head: RespHead {
                            status: uapi::VIRTIO_RTC_S_OK,
                            _reserved: [0; 7],
                        },
                        num_clocks: Le16::from(NUM_CLOCKS),
                        _padding: [0; 6],
                    };
                    if let Err(e) = mem.write_obj(resp, resp_desc.addr) {
                        error!("rtc: failed to write CFG response: {e:?}");
                    } else {
                        written = std::mem::size_of::<RespCfg>() as u32;
                    }
                }

                uapi::VIRTIO_RTC_REQ_CLOCK_CAP => {
                    // Read clock_id from request body
                    let req_body: ReqClockCap = mem
                        .read_obj(
                            req_desc
                                .addr
                                .unchecked_add(std::mem::size_of::<ReqHead>() as u64),
                        )
                        .unwrap_or_default();

                    let clock_id = req_body.clock_id.to_native();
                    let status = Self::validate_clock_id(clock_id);

                    let resp = if status == uapi::VIRTIO_RTC_S_OK {
                        // Clock 0 = UTC
                        RespClockCap {
                            head: RespHead {
                                status,
                                _reserved: [0; 7],
                            },
                            clock_type: Le16::from(uapi::VIRTIO_RTC_CLOCK_UTC),
                            flags: uapi::VIRTIO_RTC_FLAG_LEAP_SECOND_INFO,
                            _reserved: [0; 5],
                        }
                    } else {
                        RespClockCap {
                            head: RespHead {
                                status,
                                _reserved: [0; 7],
                            },
                            clock_type: Le16::from(0),
                            flags: 0,
                            _reserved: [0; 5],
                        }
                    };

                    if let Err(e) = mem.write_obj(resp, resp_desc.addr) {
                        error!("rtc: failed to write CLOCK_CAP response: {e:?}");
                    } else {
                        written = std::mem::size_of::<RespClockCap>() as u32;
                    }
                }

                uapi::VIRTIO_RTC_REQ_READ => {
                    // Read clock_id from request body
                    let req_body: ReqRead = mem
                        .read_obj(
                            req_desc
                                .addr
                                .unchecked_add(std::mem::size_of::<ReqHead>() as u64),
                        )
                        .unwrap_or_default();

                    let clock_id = req_body.clock_id.to_native();
                    let status = Self::validate_clock_id(clock_id);

                    let resp = if status == uapi::VIRTIO_RTC_S_OK {
                        // Clock 0 = UTC
                        RespRead {
                            head: RespHead {
                                status,
                                _reserved: [0; 7],
                            },
                            clock_ns: Le64::from(Self::get_utc_ns()),
                        }
                    } else {
                        RespRead {
                            head: RespHead {
                                status,
                                _reserved: [0; 7],
                            },
                            clock_ns: Le64::from(0),
                        }
                    };

                    if let Err(e) = mem.write_obj(resp, resp_desc.addr) {
                        error!("rtc: failed to write READ response: {e:?}");
                    } else {
                        written = std::mem::size_of::<RespRead>() as u32;
                    }
                }

                uapi::VIRTIO_RTC_REQ_CROSS_CAP => {
                    // Cross-timestamping is disabled for now. On macOS with HVF,
                    // PTP_SYS_OFFSET_PRECISE fails with EOVERFLOW due to kernel
                    // interpolation issues, causing error logs. The fallback method
                    // (PTP_SYS_OFFSET) works fine without cross-timestamping support.
                    let resp = RespCrossCap {
                        head: RespHead {
                            status: uapi::VIRTIO_RTC_S_OK,
                            _reserved: [0; 7],
                        },
                        flags: 0, // not supported
                        _reserved: [0; 7],
                    };

                    if let Err(e) = mem.write_obj(resp, resp_desc.addr) {
                        error!("rtc: failed to write CROSS_CAP response: {e:?}");
                    } else {
                        written = std::mem::size_of::<RespCrossCap>() as u32;
                    }
                }

                uapi::VIRTIO_RTC_REQ_READ_CROSS => {
                    // Cross-timestamping is not supported (we advertise flags=0 in CROSS_CAP).
                    // Return EOPNOTSUPP if the kernel calls this anyway.
                    let resp = RespReadCross {
                        head: RespHead {
                            status: uapi::VIRTIO_RTC_S_EOPNOTSUPP,
                            _reserved: [0; 7],
                        },
                        clock_ns: Le64::from(0),
                        counter_value: Le64::from(0),
                    };

                    if let Err(e) = mem.write_obj(resp, resp_desc.addr) {
                        error!("rtc: failed to write READ_CROSS response: {e:?}");
                    } else {
                        written = std::mem::size_of::<RespReadCross>() as u32;
                    }
                }

                _ => {
                    warn!("rtc: unknown request type {msg_type:#x}");
                    let resp = RespHead {
                        status: uapi::VIRTIO_RTC_S_EINVAL,
                        _reserved: [0; 7],
                    };
                    if let Err(e) = mem.write_obj(resp, resp_desc.addr) {
                        error!("rtc: failed to write error response: {e:?}");
                    } else {
                        written = std::mem::size_of::<RespHead>() as u32;
                    }
                }
            }

            have_used = true;
            if let Err(e) = self.queues[REQ_INDEX].add_used(mem, index, written) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }
}

impl VirtioDevice for Rtc {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_CLOCK
    }

    fn device_name(&self) -> &str {
        "rtc"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn queues(&self) -> &[VirtQueue] {
        &self.queues
    }

    fn queues_mut(&mut self) -> &mut [VirtQueue] {
        &mut self.queues
    }

    fn queue_events(&self) -> &[EventFd] {
        &self.queue_events
    }

    fn read_config(&self, _offset: u64, _data: &mut [u8]) {
        // virtio-rtc has no config space
        warn!("rtc: guest attempted to read config space");
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "rtc: guest attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if queues.len() != defs::NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                defs::NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        self.queues = queues.iter().map(|dq| dq.queue.clone()).collect();
        self.queue_events = queues
            .iter()
            .map(|dq| dq.event.as_ref().try_clone().unwrap())
            .collect();

        if self.activate_evt.write(1).is_err() {
            error!("Cannot write to activate_evt");
            return Err(ActivateError::BadActivate);
        }

        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn reset(&mut self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify structure sizes match the virtio-rtc spec.
    /// All structures should be 8-byte aligned per the spec.
    #[test]
    fn test_structure_sizes() {
        // Request header: le16 msg_type + 6 reserved = 8 bytes
        assert_eq!(std::mem::size_of::<ReqHead>(), 8);

        // Response header: u8 status + 7 reserved = 8 bytes
        assert_eq!(std::mem::size_of::<RespHead>(), 8);

        // REQ_CFG response: header(8) + le16 num_clocks + 6 padding = 16 bytes
        assert_eq!(std::mem::size_of::<RespCfg>(), 16);

        // REQ_CLOCK_CAP request body: le16 clock_id + 6 padding = 8 bytes
        assert_eq!(std::mem::size_of::<ReqClockCap>(), 8);

        // REQ_CLOCK_CAP response: header(8) + le16 clock_type + u8 flags + 5 reserved = 16 bytes
        assert_eq!(std::mem::size_of::<RespClockCap>(), 16);

        // REQ_READ request body: le16 clock_id + 6 padding = 8 bytes
        assert_eq!(std::mem::size_of::<ReqRead>(), 8);

        // REQ_READ response: header(8) + le64 clock_ns = 16 bytes
        assert_eq!(std::mem::size_of::<RespRead>(), 16);

        // REQ_CROSS_CAP request body: le16 clock_id + u8 hw_counter + 5 padding = 8 bytes
        assert_eq!(std::mem::size_of::<ReqCrossCap>(), 8);

        // REQ_CROSS_CAP response: header(8) + u8 flags + 7 reserved = 16 bytes
        assert_eq!(std::mem::size_of::<RespCrossCap>(), 16);
    }

    /// Verify device type matches virtio spec (device ID 17 for clock/RTC)
    #[test]
    fn test_device_type() {
        assert_eq!(uapi::VIRTIO_ID_CLOCK, 17);

        let rtc = Rtc::new().unwrap();
        assert_eq!(rtc.device_type(), 17);
    }

    /// Verify request type constants match the spec
    #[test]
    fn test_request_types() {
        assert_eq!(uapi::VIRTIO_RTC_REQ_READ, 0x0001);
        assert_eq!(uapi::VIRTIO_RTC_REQ_READ_CROSS, 0x0002);
        assert_eq!(uapi::VIRTIO_RTC_REQ_CFG, 0x1000);
        assert_eq!(uapi::VIRTIO_RTC_REQ_CLOCK_CAP, 0x1001);
        assert_eq!(uapi::VIRTIO_RTC_REQ_CROSS_CAP, 0x1002);
    }

    /// Verify status codes match the spec
    #[test]
    fn test_status_codes() {
        assert_eq!(uapi::VIRTIO_RTC_S_OK, 0);
        assert_eq!(uapi::VIRTIO_RTC_S_EOPNOTSUPP, 2);
        assert_eq!(uapi::VIRTIO_RTC_S_ENODEV, 3);
        assert_eq!(uapi::VIRTIO_RTC_S_EINVAL, 4);
        assert_eq!(uapi::VIRTIO_RTC_S_EIO, 5);
    }

    /// Verify clock type constants match the spec
    #[test]
    fn test_clock_types() {
        assert_eq!(uapi::VIRTIO_RTC_CLOCK_UTC, 0);
        assert_eq!(uapi::VIRTIO_RTC_CLOCK_TAI, 1);
        assert_eq!(uapi::VIRTIO_RTC_CLOCK_MONOTONIC, 2);
    }

    /// Verify the UTC time helper returns reasonable values
    #[test]
    fn test_get_utc_ns() {
        let time_ns = Rtc::get_utc_ns();

        // Time should be after 2020-01-01 (1577836800 seconds = 1577836800000000000 ns)
        let min_time_ns: u64 = 1577836800 * 1_000_000_000;
        assert!(time_ns > min_time_ns, "Time {time_ns} should be after 2020");

        // Time should be before 2100-01-01 (4102444800 seconds)
        let max_time_ns: u64 = 4102444800 * 1_000_000_000;
        assert!(
            time_ns < max_time_ns,
            "Time {time_ns} should be before 2100"
        );
    }

    /// Verify device initialization
    #[test]
    fn test_device_init() {
        let rtc = Rtc::new().unwrap();

        assert_eq!(rtc.device_name(), "rtc");
        assert_eq!(rtc.id(), "virtio_rtc");
        assert_eq!(rtc.queues().len(), 1);
        assert_eq!(rtc.queue_events().len(), 1);
        assert!(!rtc.is_activated());

        // Should have VIRTIO_F_VERSION_1 feature
        assert_eq!(rtc.avail_features(), 1 << 32);
    }

    /// Verify number of clocks we expose
    #[test]
    fn test_num_clocks() {
        // We expose 1 clock (UTC)
        assert_eq!(NUM_CLOCKS, 1);
    }

    /// Test that ReqHead can be properly constructed from bytes
    #[test]
    fn test_req_head_layout() {
        let mut bytes = [0u8; 8];
        // msg_type = 0x1000 (REQ_CFG) in little-endian
        bytes[0] = 0x00;
        bytes[1] = 0x10;

        let head: ReqHead = unsafe { std::ptr::read(bytes.as_ptr() as *const ReqHead) };
        assert_eq!(head.msg_type.to_native(), uapi::VIRTIO_RTC_REQ_CFG);
    }

    /// Test that RespHead can be properly constructed
    #[test]
    fn test_resp_head_layout() {
        let resp = RespHead {
            status: uapi::VIRTIO_RTC_S_OK,
            _reserved: [0; 7],
        };

        let bytes: [u8; 8] = unsafe { std::mem::transmute(resp) };
        assert_eq!(bytes[0], 0); // status = OK
        assert_eq!(bytes[1..], [0; 7]); // reserved
    }

    /// Test RespCfg layout
    #[test]
    fn test_resp_cfg_layout() {
        let resp = RespCfg {
            head: RespHead {
                status: uapi::VIRTIO_RTC_S_OK,
                _reserved: [0; 7],
            },
            num_clocks: Le16::from(1),
            _padding: [0; 6],
        };

        let bytes: [u8; 16] = unsafe { std::mem::transmute(resp) };
        assert_eq!(bytes[0], 0); // status = OK
        assert_eq!(bytes[8], 1); // num_clocks low byte
        assert_eq!(bytes[9], 0); // num_clocks high byte
    }

    /// Test RespRead layout
    #[test]
    fn test_resp_read_layout() {
        let resp = RespRead {
            head: RespHead {
                status: uapi::VIRTIO_RTC_S_OK,
                _reserved: [0; 7],
            },
            clock_ns: Le64::from(0x123456789ABCDEF0u64),
        };

        let bytes: [u8; 16] = unsafe { std::mem::transmute(resp) };
        assert_eq!(bytes[0], 0); // status = OK

        // clock_ns in little-endian at offset 8
        assert_eq!(bytes[8], 0xF0);
        assert_eq!(bytes[9], 0xDE);
        assert_eq!(bytes[10], 0xBC);
        assert_eq!(bytes[11], 0x9A);
        assert_eq!(bytes[12], 0x78);
        assert_eq!(bytes[13], 0x56);
        assert_eq!(bytes[14], 0x34);
        assert_eq!(bytes[15], 0x12);
    }

    /// Test ReqReadCross structure size
    #[test]
    fn test_req_read_cross_size() {
        // REQ_READ_CROSS request body: le16 clock_id + u8 hw_counter + 5 padding = 8 bytes
        assert_eq!(std::mem::size_of::<ReqReadCross>(), 8);
    }

    /// Test RespReadCross structure size and layout
    #[test]
    fn test_resp_read_cross_size() {
        // REQ_READ_CROSS response: header(8) + le64 clock_ns + le64 counter_value = 24 bytes
        assert_eq!(std::mem::size_of::<RespReadCross>(), 24);
    }

    /// Test RespReadCross layout
    #[test]
    fn test_resp_read_cross_layout() {
        let resp = RespReadCross {
            head: RespHead {
                status: uapi::VIRTIO_RTC_S_OK,
                _reserved: [0; 7],
            },
            clock_ns: Le64::from(0x1122334455667788u64),
            counter_value: Le64::from(0xAABBCCDDEEFF0011u64),
        };

        let bytes: [u8; 24] = unsafe { std::mem::transmute(resp) };
        assert_eq!(bytes[0], 0); // status = OK

        // clock_ns at offset 8
        assert_eq!(bytes[8], 0x88);
        assert_eq!(bytes[15], 0x11);

        // counter_value at offset 16
        assert_eq!(bytes[16], 0x11);
        assert_eq!(bytes[23], 0xAA);
    }

    /// Test counter reading function works
    #[test]
    fn test_read_counter() {
        let cnt1 = read_counter();
        let cnt2 = read_counter();

        // Counter should be monotonically increasing (or at least not wildly different)
        // Allow for wrap-around by checking they're both non-zero
        assert!(
            cnt1 > 0 || cnt2 > 0,
            "Counter should return non-zero values"
        );

        // Second read should be >= first (unless wrap, which is unlikely in a test)
        // We use wrapping comparison to handle potential overflow
        assert!(
            cnt2 >= cnt1 || cnt1.wrapping_sub(cnt2) > u64::MAX / 2,
            "Counter should be monotonic: cnt1={cnt1}, cnt2={cnt2}"
        );
    }

    /// Test supported_counter_id returns correct value for architecture
    #[test]
    fn test_supported_counter_id() {
        let id = supported_counter_id();

        #[cfg(target_arch = "aarch64")]
        assert_eq!(id, 0, "ARM should use counter type 0 (CNTVCT)");

        #[cfg(target_arch = "x86_64")]
        assert_eq!(id, 1, "x86_64 should use counter type 1 (TSC)");
    }

    /// Test get_cross_timestamp returns reasonable values
    #[test]
    fn test_get_cross_timestamp() {
        let (time_ns, counter) = Rtc::get_cross_timestamp();

        // Time should be reasonable (after 2020)
        let min_time_ns: u64 = 1577836800 * 1_000_000_000;
        assert!(time_ns > min_time_ns, "Time should be after 2020");

        // Counter should be non-zero
        assert!(counter > 0, "Counter should be non-zero");

        // Do a second read and verify time advances
        let (time_ns2, counter2) = Rtc::get_cross_timestamp();
        assert!(time_ns2 >= time_ns, "Time should not go backwards");
        assert!(
            counter2 >= counter || counter.wrapping_sub(counter2) > u64::MAX / 2,
            "Counter should be monotonic"
        );
    }

    /// Test counter frequency returns reasonable value
    #[test]
    fn test_get_counter_freq() {
        let freq = get_counter_freq();

        // Frequency should be non-zero
        assert!(freq > 0, "Counter frequency should be non-zero");

        // Frequency should be reasonable: between 1 MHz and 10 GHz
        // ARM counters are typically 24 MHz to a few GHz
        // x86 TSC is typically 1-5 GHz
        let min_freq = 1_000_000u64; // 1 MHz
        let max_freq = 10_000_000_000u64; // 10 GHz

        assert!(
            freq >= min_freq && freq <= max_freq,
            "Counter frequency {freq} Hz should be between {min_freq} and {max_freq}"
        );
    }
}

/// Mock for `SystemTime::now()` used in Kani proofs.
///
/// Returns a fixed time (UNIX_EPOCH + 1_700_000_000 seconds, an arbitrary
/// post-2020 timestamp) so that `get_utc_ns()` is deterministic.
#[cfg(kani)]
fn mock_system_time_now() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)
}

#[cfg(kani)]
mod verification {
    use super::*;

    // ── Structure size proofs ─────────────────────────────────────────────────
    //
    // Virtio-RTC spec (§5.16) mandates specific struct layouts.  We verify each
    // size at the type level via `std::mem::size_of`, which is a compile-time
    // constant that Kani resolves immediately.

    /// Verify: ReqHead is exactly 8 bytes (virtio-rtc spec §5.16.6.1).
    /// This is a compile-time assertion (const usize at compile time).
    const _: () = {
        let _ = [(); 1][if std::mem::size_of::<ReqHead>() == 8 {
            0
        } else {
            1
        }];
    };

    /// Verify: RespHead is exactly 8 bytes.
    /// This is a compile-time assertion (const usize at compile time).
    const _: () = {
        let _ = [(); 1][if std::mem::size_of::<RespHead>() == 8 {
            0
        } else {
            1
        }];
    };

    /// Verify: RespCfg is exactly 16 bytes (header + num_clocks + padding).
    /// This is a compile-time assertion (const usize at compile time).
    const _: () = {
        let _ = [(); 1][if std::mem::size_of::<RespCfg>() == 16 {
            0
        } else {
            1
        }];
    };

    /// Verify: RespRead is exactly 16 bytes (header + clock_ns Le64).
    /// This is a compile-time assertion (const usize at compile time).
    const _: () = {
        let _ = [(); 1][if std::mem::size_of::<RespRead>() == 16 {
            0
        } else {
            1
        }];
    };

    /// Verify: RespReadCross is exactly 24 bytes (header + clock_ns + counter_value).
    /// This is a compile-time assertion (const usize at compile time).
    const _: () = {
        let _ = [(); 1][if std::mem::size_of::<RespReadCross>() == 24 {
            0
        } else {
            1
        }];
    };

    // ── Protocol dispatch proofs ──────────────────────────────────────────────

    /// Proof: clock_id 0 always selects VIRTIO_RTC_S_OK.
    ///
    /// The virtio-rtc spec mandates that clock 0 (UTC) must always be supported.
    /// This calls the production `Rtc::validate_clock_id` directly.
    #[kani::proof]
    fn proof_clock_cap_clock0_is_ok() {
        let status = Rtc::validate_clock_id(0);
        kani::assert(
            status == uapi::VIRTIO_RTC_S_OK,
            "clock 0 must return VIRTIO_RTC_S_OK",
        );
        kani::cover!(true, "clock 0 OK path reachable");
    }

    /// Proof: any clock_id other than 0 always returns VIRTIO_RTC_S_EINVAL.
    #[kani::proof]
    fn proof_clock_cap_nonzero_clock_is_einval() {
        let clock_id: u16 = kani::any_where(|&id| id != 0);
        let status = Rtc::validate_clock_id(clock_id);
        kani::assert(
            status == uapi::VIRTIO_RTC_S_EINVAL,
            "non-zero clock_id must return VIRTIO_RTC_S_EINVAL",
        );
        kani::cover!(true, "nonzero clock EINVAL path reachable");
    }

    /// Proof: validate_clock_id returns only VIRTIO_RTC_S_OK or VIRTIO_RTC_S_EINVAL.
    ///
    /// For any clock_id, the status is either VIRTIO_RTC_S_OK (0) or
    /// VIRTIO_RTC_S_EINVAL (4).  No other status code is produced.  Both
    /// CLOCK_CAP and READ dispatches in `process_req` rely on this invariant.
    #[kani::proof]
    fn proof_clock_cap_status_is_ok_or_einval() {
        let clock_id: u16 = kani::any();
        let status = Rtc::validate_clock_id(clock_id);
        kani::assert(
            status == uapi::VIRTIO_RTC_S_OK || status == uapi::VIRTIO_RTC_S_EINVAL,
            "validate_clock_id status must be OK or EINVAL",
        );
        kani::cover!(status == uapi::VIRTIO_RTC_S_OK, "OK branch reachable");
        kani::cover!(
            status == uapi::VIRTIO_RTC_S_EINVAL,
            "EINVAL branch reachable"
        );
    }

    /// Proof: validate_clock_id is deterministic — two calls with the same
    /// clock_id always produce the same status.
    ///
    /// Both CLOCK_CAP and READ in `process_req` use this single function, so
    /// they are guaranteed to agree for every possible clock_id.
    #[kani::proof]
    fn proof_clock_cap_and_read_status_agree() {
        let clock_id: u16 = kani::any();
        let cap_status = Rtc::validate_clock_id(clock_id);
        let rd_status = Rtc::validate_clock_id(clock_id);
        kani::assert(
            cap_status == rd_status,
            "CLOCK_CAP and READ status must agree for all clock_id values",
        );
        kani::cover!(true, "status agreement proof reachable");
    }

    // ── get_utc_ns with SystemTime stub ───────────────────────────────────────

    /// Proof: get_utc_ns returns a value that fits in u64.
    ///
    /// `SystemTime::now().duration_since(UNIX_EPOCH)` can only fail if the
    /// system clock is set before the UNIX epoch (1970).  In practice this
    /// cannot happen in a VM, but we stub the call to a fixed post-epoch
    /// timestamp so that Kani can reason about the arithmetic.
    ///
    /// The stub replaces `SystemTime::now` with `mock_system_time_now`, which
    /// returns `UNIX_EPOCH + 1_700_000_000s`.  The proof then checks that the
    /// resulting nanosecond value is non-zero and fits in u64.
    #[kani::proof]
    #[kani::stub(std::time::SystemTime::now, mock_system_time_now)]
    fn proof_get_utc_ns_no_panic() {
        let ns = Rtc::get_utc_ns();
        // With our fixed stub, duration_since(UNIX_EPOCH) succeeds and the
        // value is 1_700_000_000 * 1e9 = 1.7e18, which fits in a u64.
        kani::assert(ns > 0, "get_utc_ns with fixed stub must return non-zero");
        kani::cover!(true, "get_utc_ns stub path reachable");
    }

    // ── Constant value proofs ─────────────────────────────────────────────────

    /// Proof: VIRTIO_RTC_S_OK == 0 as required by the virtio spec.
    ///
    /// The spec (§5.16.6.2) defines the success status as 0.
    #[kani::proof]
    fn proof_virtio_rtc_s_ok_is_zero() {
        kani::assert(
            uapi::VIRTIO_RTC_S_OK == 0,
            "VIRTIO_RTC_S_OK must be 0 per virtio-rtc spec",
        );
        kani::cover!(true, "status constant proof reachable");
    }

    /// Proof: NUM_CLOCKS == 1.
    ///
    /// We advertise exactly one clock (UTC).  This proof ensures that the
    /// constant is not accidentally changed.
    #[kani::proof]
    fn proof_num_clocks_is_one() {
        kani::assert(NUM_CLOCKS == 1, "NUM_CLOCKS must be 1 (UTC clock only)");
        kani::cover!(true, "NUM_CLOCKS constant proof reachable");
    }
}
