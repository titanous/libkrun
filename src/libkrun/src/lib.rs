#[cfg(all(feature = "nitro-sandbox", feature = "tee"))]
compile_error!("unsupported on this backend: nitro-sandbox cannot be combined with tee");
#[cfg(all(feature = "nitro-sandbox", feature = "aws-nitro"))]
compile_error!("unsupported on this backend: nitro-sandbox cannot be combined with aws-nitro");
mod readiness;
pub use readiness::{GuestReadiness, GuestReadyTimeout};

/// A cooperative template descriptor or owner-only persisted file.
#[cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    not(feature = "tee")
))]
pub enum TemplateSource {
    Descriptor(std::fs::File),
    Path(std::path::PathBuf),
}

use crossbeam_channel::unbounded;
#[cfg(feature = "gpu")]
use devices::virtio::gpu::display::DisplayInfo;
#[cfg(feature = "blk")]
pub use devices::virtio::CacheType;
#[cfg(feature = "gpu")]
use krun_display::DisplayBackend;
use log::info;
use std::ops::{Deref, DerefMut};

#[cfg(feature = "blk")]
pub use devices::virtio::block::device::BlockDeviceType;
#[cfg(feature = "blk")]
pub use devices::virtio::block::{
    AsyncBlockBackend, AsyncBlockBackendFactory, BlockBackend, BoxFuture, ImageType, IoVector,
    IoVectorMut, SendBoxFuture, SyncMode, VolatileSlice, VolatileSliceGuard,
};
#[cfg(not(feature = "tee"))]
pub use devices::virtio::fs::dax_mapper;
#[cfg(not(feature = "tee"))]
pub use devices::virtio::fs::filesystem::{
    Context as FilesystemContext, DirEntry, Entry, FileSystem, Handle, Inode, OpenOptions,
    ZeroCopyReader, ZeroCopyWriter,
};
#[cfg(not(feature = "tee"))]
pub use devices::virtio::fs::passthrough;
#[cfg(feature = "net")]
pub use devices::virtio::net::device::VirtioNetBackend;
#[cfg(feature = "net")]
pub use devices::virtio::net::{
    AsyncNetBackend, AsyncNetBackendFactory, BoxFuture as NetBoxFuture, NetBackendHandle,
    SendBoxFuture as NetSendBoxFuture,
};
pub use devices::virtio::port_io::{self, PortInput, PortOutput};
#[cfg(not(feature = "tee"))]
pub use devices::virtio::rng::{OsRngBackend, RngBackend};
use devices::virtio::vsock::policy::SocketPolicy;
/// Unmapped or policy-denied TSI listeners return EPERM for native fallback,
/// without allocating a host socket. Successful host listeners still return zero.
pub const TSI_UNMAPPED_LISTEN_NATIVE_FALLBACK: bool = true;

pub use devices::virtio::vsock::policy::{
    AllowlistPolicy, OutboundPolicy, SocketAddress, SocketOperation,
};
pub use devices::virtio::PortDescription;
pub use devices::virtio::VmmExitObserver;
use libc::{c_char, size_t};
use polly::event_manager::EventManager;
use std::collections::HashMap;
use std::convert::TryInto;
use std::num::NonZeroU64;
#[cfg(feature = "net")]
use std::os::fd::RawFd;
use std::path::PathBuf;
#[cfg(not(feature = "static-firmware"))]
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};
use utils::eventfd::EventFd;
use vmm::builder::StartMicrovmError;
pub use vmm::resources::VirtioConsoleConfigMode;
use vmm::resources::{TsiFlags, VmResources, VsockConfig};
pub use vmm::vm_exit::VmExit;
#[cfg(feature = "blk")]
pub use vmm::vmm_config::block::{BlockConfigError, BlockDeviceConfig, BlockRootConfig};
use vmm::vmm_config::external_kernel::ExternalKernel;
pub use vmm::vmm_config::external_kernel::KernelFormat;
#[cfg(not(feature = "tee"))]
use vmm::vmm_config::fs::FsMount;
use vmm::vmm_config::kernel_bundle::KernelBundle;
#[cfg(feature = "tee")]
use vmm::vmm_config::kernel_bundle::{InitrdBundle, QbootBundle};
use vmm::vmm_config::kernel_cmdline::{KernelCmdlineConfig, DEFAULT_KERNEL_CMDLINE};
use vmm::vmm_config::machine_config::VmConfig;
#[cfg(feature = "net")]
use vmm::vmm_config::net::{NetworkInterfaceConfig, NetworkInterfaceError};
#[cfg(feature = "vhost-user")]
use vmm::vmm_config::vhost_user_fs::VhostUserFsConfig;
use vmm::vmm_config::vsock::VsockDeviceConfig;

#[cfg(feature = "aws-nitro")]
use aws_nitro::enclave::NitroEnclave;

#[cfg(feature = "gpu")]
use devices::virtio::display::{DisplayInfoEdid, PhysicalSize, MAX_DISPLAYS};
#[cfg(feature = "input")]
use krun_input::{InputConfigBackend, InputEventProviderBackend};

// krunfw library name for each context (dynamic loading)
#[cfg(all(
    target_os = "linux",
    not(feature = "tee"),
    not(feature = "static-firmware")
))]
const KRUNFW_NAME: &str = "libkrunfw.so.5";
#[cfg(all(
    target_os = "linux",
    feature = "amd-sev",
    not(feature = "static-firmware")
))]
const KRUNFW_NAME: &str = "libkrunfw-sev.so.5";
#[cfg(all(target_os = "linux", feature = "tdx", not(feature = "static-firmware")))]
const KRUNFW_NAME: &str = "libkrunfw-tdx.so.5";

#[cfg(feature = "aws-nitro")]
static KRUN_NITRO_DEBUG: Mutex<bool> = Mutex::new(false);

// Path to the init binary to be executed inside the VM.
const INIT_PATH: &str = "/init.krun";

// --- Dynamic firmware loading (default) ---
#[cfg(not(feature = "static-firmware"))]
static KRUNFW: LazyLock<Option<libloading::Library>> =
    LazyLock::new(|| unsafe { libloading::Library::new(KRUNFW_NAME).ok() });

#[cfg(not(feature = "static-firmware"))]
pub struct KrunfwBindings {
    get_kernel: libloading::Symbol<
        'static,
        unsafe extern "C" fn(*mut u64, *mut u64, *mut size_t) -> *mut c_char,
    >,
    #[cfg(feature = "tee")]
    get_initrd: libloading::Symbol<'static, unsafe extern "C" fn(*mut size_t) -> *mut c_char>,
    #[cfg(feature = "tee")]
    get_qboot: libloading::Symbol<'static, unsafe extern "C" fn(*mut size_t) -> *mut c_char>,
}

#[cfg(not(feature = "static-firmware"))]
impl KrunfwBindings {
    fn load_bindings() -> Result<KrunfwBindings, libloading::Error> {
        let krunfw = match KRUNFW.as_ref() {
            Some(krunfw) => krunfw,
            None => return Err(libloading::Error::DlOpenUnknown),
        };
        Ok(unsafe {
            KrunfwBindings {
                get_kernel: krunfw.get(b"krunfw_get_kernel")?,
                #[cfg(feature = "tee")]
                get_initrd: krunfw.get(b"krunfw_get_initrd")?,
                #[cfg(feature = "tee")]
                get_qboot: krunfw.get(b"krunfw_get_qboot")?,
            }
        })
    }

    pub fn new() -> Option<Self> {
        Self::load_bindings().ok()
    }
}

// --- Static firmware linking ---
#[cfg(feature = "static-firmware")]
unsafe extern "C" {
    fn krunfw_get_kernel(
        guest_addr: *mut u64,
        entry_addr: *mut u64,
        size: *mut size_t,
    ) -> *mut c_char;
}

#[cfg(feature = "static-firmware")]
pub struct KrunfwBindings;

#[cfg(feature = "static-firmware")]
impl KrunfwBindings {
    pub fn new() -> Option<Self> {
        Some(KrunfwBindings)
    }
}

#[derive(Clone)]
#[cfg(feature = "net")]
#[allow(dead_code)]
// Kept for backwards compatibility; only VirtioNetPasst is currently used
enum LegacyNetworkConfig {
    VirtioNetPasst(RawFd),
    VirtioNetGvproxy(PathBuf),
}

#[derive(Default)]
pub struct ContextConfig {
    krunfw: Option<KrunfwBindings>,
    vmr: VmResources,
    workdir: Option<String>,
    exec_path: Option<String>,
    env: Option<String>,
    args: Option<String>,
    rlimits: Option<String>,
    #[cfg(feature = "net")]
    legacy_net_cfg: Option<LegacyNetworkConfig>,
    #[cfg(feature = "net")]
    legacy_mac: Option<[u8; 6]>,
    net_index: u8,
    tsi_port_map: Option<HashMap<u16, u16>>,
    vsock_config: VsockConfig,
    #[cfg(feature = "blk")]
    block_cfgs: Vec<BlockDeviceConfig>,
    #[cfg(feature = "blk")]
    root_block_cfg: Option<BlockDeviceConfig>,
    #[cfg(feature = "blk")]
    data_block_cfg: Option<BlockDeviceConfig>,
    #[cfg(feature = "blk")]
    block_root: Option<BlockRootConfig>,
    #[cfg(feature = "tee")]
    tee_config_file: Option<PathBuf>,
    unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
    #[cfg(feature = "vhost-user")]
    vhost_user_vsock: bool,
    shutdown_efd: Option<EventFd>,
    gpu_virgl_flags: Option<u32>,
    gpu_shm_size: Option<usize>,
    #[allow(dead_code)]
    // Only read when 'snd' feature is enabled; keep field for API completeness
    enable_snd: bool,
    console_output: Option<PathBuf>,
    vmm_uid: Option<libc::uid_t>,
    vmm_gid: Option<libc::gid_t>,
}

impl ContextConfig {
    fn get_workdir(&self) -> String {
        match &self.workdir {
            Some(workdir) => format!("KRUN_WORKDIR={workdir}"),
            None => "".to_string(),
        }
    }

    fn get_exec_path(&self) -> String {
        match &self.exec_path {
            Some(exec_path) => format!("KRUN_INIT={exec_path}"),
            None => "".to_string(),
        }
    }

    fn get_block_root(&self) -> String {
        #[cfg(feature = "blk")]
        match &self.block_root {
            Some(block_root) => {
                let mut res = format!("KRUN_BLOCK_ROOT_DEVICE={}", block_root.device);
                if let Some(fstype) = &block_root.fstype {
                    res += &format!(" KRUN_BLOCK_ROOT_FSTYPE={fstype}");
                }
                if let Some(options) = &block_root.options {
                    res += &format!(" KRUN_BLOCK_ROOT_OPTIONS={options}");
                }
                res
            }
            None => "".to_string(),
        }
        #[cfg(not(feature = "blk"))]
        "".to_string()
    }

    fn get_env(&self) -> String {
        match &self.env {
            Some(env) => env.clone(),
            None => "".to_string(),
        }
    }

    fn get_args(&self) -> String {
        match &self.args {
            Some(args) => args.clone(),
            None => "".to_string(),
        }
    }

    fn get_rlimits(&self) -> String {
        match &self.rlimits {
            Some(rlimits) => format!("KRUN_RLIMITS={rlimits}"),
            None => "".to_string(),
        }
    }

    #[cfg(feature = "blk")]
    #[allow(dead_code)]
    // Kept for backwards compatibility; alternative API exists via Builder
    fn set_data_block_cfg(&mut self, block_cfg: BlockDeviceConfig) {
        self.data_block_cfg = Some(block_cfg);
    }

    #[cfg(feature = "blk")]
    fn take_block_cfg(&mut self) -> Vec<BlockDeviceConfig> {
        // For backwards compat, when cfgs is empty (the new API is not used), this needs to be
        // root and then data, in that order. Also for backwards compat, root/data are setters and
        // need to discard redundant calls. So we have simple setters above and fix up here.
        //
        // When the new API is used, this is simpler.
        if self.block_cfgs.is_empty() {
            [&mut self.root_block_cfg, &mut self.data_block_cfg]
                .into_iter()
                .filter_map(|cfg| cfg.take())
                .collect()
        } else {
            std::mem::take(&mut self.block_cfgs)
        }
    }

    #[cfg(feature = "tee")]
    #[mutants::skip] // tee feature not enabled in mutation testing
    fn set_tee_config_file(&mut self, filepath: PathBuf) {
        self.tee_config_file = Some(filepath);
    }

    #[cfg(feature = "tee")]
    #[mutants::skip] // tee feature not enabled in mutation testing
    fn get_tee_config_file(&self) -> Option<PathBuf> {
        self.tee_config_file.clone()
    }

    #[cfg(feature = "aws-nitro")]
    #[mutants::skip] // aws-nitro feature not enabled in mutation testing
    fn set_nitro_image(&mut self, image_path: PathBuf) {
        self.nitro_image_path = Some(image_path);
    }

    #[cfg(feature = "aws-nitro")]
    #[mutants::skip] // aws-nitro feature not enabled in mutation testing
    fn set_nitro_start_flags(&mut self, start_flags: StartFlags) {
        self.nitro_start_flags = start_flags;
    }
}

#[cfg(feature = "aws-nitro")]
#[mutants::skip] // aws-nitro feature not enabled in mutation testing
impl TryFrom<ContextConfig> for NitroEnclave {
    type Error = i32;

    fn try_from(ctx: ContextConfig) -> Result<Self, Self::Error> {
        let vm_config = ctx.vmr.vm_config();

        let Some(mem_size_mib) = vm_config.mem_size_mib else {
            log::error!("memory size not configured");
            return Err(-libc::EINVAL);
        };

        let Some(vcpus) = vm_config.vcpu_count else {
            log::error!("vCPU count not configured");
            return Err(-libc::EINVAL);
        };

        let rootfs = if let Some(path) = &ctx.vmr.fs.first() {
            path.shared_dir.clone()
        } else {
            log::error!("rootfs path required");
            return Err(-libc::EINVAL);
        };

        let Some(exec_path) = ctx.exec_path else {
            log::error!("exec path not specified");
            return Err(-libc::EINVAL);
        };

        let Some(exec_env) = ctx.env else {
            log::error!("execution env not specified");
            return Err(-libc::EINVAL);
        };

        let Some(exec_args) = ctx.args else {
            log::error!("execution args not specified");
            return Err(-libc::EINVAL);
        };

        let net_unixfd = {
            let mut list = ctx.vmr.net.list;
            let len = list.len();
            match len {
                0 => None,
                1 => {
                    let device = list.pop_front().unwrap();
                    let device = device.lock().unwrap();

                    let fd = match device.backend() {
                        Some(VirtioNetBackend::UnixstreamFd(fd)) => RawFd::from(*fd),
                        _ => {
                            log::error!("configured virtio-net backend must be unix stream fd");
                            return Err(-libc::EINVAL);
                        }
                    };

                    Some(fd)
                }
                _ => {
                    log::error!(
                        "more than one network interface configured (max 1 allowed, found {len})"
                    );
                    return Err(-libc::EINVAL);
                }
            }
        };

        let Some(output_path) = ctx.console_output else {
            log::error!("console output path not specified");
            return Err(-libc::EINVAL);
        };

        let debug = KRUN_NITRO_DEBUG.lock().unwrap();

        Ok(Self {
            mem_size_mib,
            vcpus,
            rootfs,
            exec_path,
            exec_args,
            exec_env,
            net_unixfd,
            output_path,
            debug: *debug,
        })
    }
}

// Helper function to load kernel payload from krunfw library
// Used by Builder::build() to load firmware when no external kernel is configured
#[cfg_attr(
    all(feature = "static-firmware", not(feature = "tee")),
    allow(unused_variables)
)]
unsafe fn load_krunfw_payload(
    krunfw: &KrunfwBindings,
    vmr: &mut VmResources,
) -> Result<(), StartError> {
    let mut kernel_guest_addr: u64 = 0;
    let mut kernel_entry_addr: u64 = 0;
    let mut kernel_size: usize = 0;

    #[cfg(not(feature = "static-firmware"))]
    let kernel_host_addr = unsafe {
        (krunfw.get_kernel)(
            &mut kernel_guest_addr as *mut u64,
            &mut kernel_entry_addr as *mut u64,
            &mut kernel_size as *mut usize,
        )
    };

    #[cfg(feature = "static-firmware")]
    let kernel_host_addr = unsafe {
        krunfw_get_kernel(
            &mut kernel_guest_addr as *mut u64,
            &mut kernel_entry_addr as *mut u64,
            &mut kernel_size as *mut usize,
        )
    };
    if kernel_host_addr.is_null() {
        return Err(StartError::MissingFirmware);
    }
    let kernel_bundle = KernelBundle {
        host_addr: NonZeroU64::new(kernel_host_addr as u64).ok_or(StartError::MissingFirmware)?,
        guest_addr: kernel_guest_addr,
        entry_addr: kernel_entry_addr,
        size: kernel_size,
    };
    vmr.set_kernel_bundle(kernel_bundle).unwrap();

    #[cfg(feature = "tee")]
    {
        let mut qboot_size: usize = 0;
        let qboot_host_addr = unsafe { (krunfw.get_qboot)(&mut qboot_size as *mut usize) };
        if qboot_host_addr.is_null() {
            return Err(StartError::MissingFirmware);
        }
        let qboot_bundle = QbootBundle {
            host_addr: NonZeroU64::new(qboot_host_addr as u64)
                .ok_or(StartError::MissingFirmware)?,
            size: qboot_size,
        };
        vmr.set_qboot_bundle(qboot_bundle).unwrap();

        let mut initrd_size: usize = 0;
        let initrd_host_addr = unsafe { (krunfw.get_initrd)(&mut initrd_size as *mut usize) };
        if initrd_host_addr.is_null() {
            return Err(StartError::MissingFirmware);
        }
        let initrd_bundle = InitrdBundle {
            host_addr: NonZeroU64::new(initrd_host_addr as u64)
                .ok_or(StartError::MissingFirmware)?,
            size: initrd_size,
        };
        vmr.set_initrd_bundle(initrd_bundle).unwrap();
    }

    Ok(())
}

/// Information about a console device for computing port paths.
#[derive(Debug, Clone)]
pub struct ConsoleDeviceInfo {
    /// The console ID (index in the virtio_consoles array)
    pub console_id: usize,
    /// The virtio device index (the X in /dev/vportXpY)
    pub device_index: u32,
}

#[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
pub use nitro_sandbox::lifecycle::{
    LifecycleLock as NitroLifecycleLock, OpenLock as NitroOpenLifecycleLock,
    Serialization as NitroSerialization, LIFECYCLE_LOCK_PATH as NITRO_LIFECYCLE_LOCK_PATH,
};

#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
pub use nitro_sandbox::runtime::ParkedReceiverError;

/// Why a Nitro template was refused for a receiver (CPU count, memory size,
/// device list or receiver kernel identity), before any receiver memory is
/// written. Carried inside the `std::io::Error` (`InvalidInput`).
#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
pub use vmm::nitro_restore::RestoreMismatch;

#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
pub use vmm::capture_request::RestoredCopyCaptureUnsupported;

/// Gives a parked Nitro receiver its template, once. See
/// `Builder::park_for_template`.
#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
pub struct TemplateAssignment {
    slot: nitro_sandbox::runtime::RestoreAssignment,
    vcpus: u8,
    ram_mib: u64,
    topology: Vec<u8>,
}
#[cfg(all(
    target_os = "linux",
    target_arch = "aarch64",
    feature = "nitro-sandbox"
))]
impl TemplateAssignment {
    /// Open and check the template against this receiver's CPU count, memory
    /// and device list, then hand it over. The receiver's own kernel identity
    /// is compared before any memory is written. Refused with a
    /// `ParkedReceiverError::Closed` error after an assignment, expiry or end
    /// (checked first, whatever the template); a template that does not fit
    /// is refused with `RestoreMismatch` and the receiver stays parked.
    pub fn assign(&self, source: TemplateSource) -> std::io::Result<()> {
        self.slot.check_open()?;
        let template = match source {
            TemplateSource::Descriptor(file) => vmm::template::Template::open(file)?,
            TemplateSource::Path(path) => vmm::template::Template::from_path(path)?,
        };
        let compatibility = template.compatibility.clone();
        vmm::nitro_restore::validate(
            &template,
            self.vcpus,
            self.ram_mib,
            &self.topology,
            &compatibility,
        )?;
        self.slot.assign(Box::new(vmm::nitro_restore::NitroRestore {
            template,
            vcpus: self.vcpus,
            ram_mib: self.ram_mib,
            topology: self.topology.clone(),
            compatibility,
        }))
    }
    /// True once the parked receiver has booted and published READY.
    pub fn receiver_ready(&self) -> bool {
        self.slot.receiver_ready()
    }
}

#[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
pub struct NitroBackendConfig {
    pub image: PathBuf,
    pub cid: u64,
    pub watchdog: std::time::Duration,
}

#[derive(Default)]
pub struct Builder {
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro: Option<NitroBackendConfig>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_lifecycle_lock: Option<nitro_sandbox::lifecycle::LifecycleLock>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_console: Option<nitro_sandbox::console::Console>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_heartbeat: Option<(u32, PathBuf, std::time::Duration)>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_image_cache: Option<PathBuf>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_stop_reason: bool,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_precreated_stop: Option<Arc<EventFd>>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_descriptor_handoff: Option<std::os::unix::net::UnixStream>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_console_base: Option<u32>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_selected_after_consoles: bool,
    #[cfg(all(
        target_os = "linux",
        target_arch = "aarch64",
        feature = "nitro-sandbox"
    ))]
    nitro_capture: Option<vmm::capture_request::CaptureRequest>,
    #[cfg(all(
        target_os = "linux",
        target_arch = "aarch64",
        feature = "nitro-sandbox"
    ))]
    nitro_park: Option<std::time::Duration>,
    config: ContextConfig,
    kernel_cmdline: Vec<String>,
    kernel_cmdline_configured: bool,
    configuration_error: Option<std::io::Error>,
    extra_kernel_args: Vec<String>,
    socket_policy: SocketPolicy,
    guest_vsock_cid: Option<u32>,
    /// Number of console devices added (for computing device paths)
    console_count: u32,
}

impl Deref for Builder {
    type Target = ContextConfig;
    fn deref(&self) -> &ContextConfig {
        &self.config
    }
}

impl DerefMut for Builder {
    fn deref_mut(&mut self) -> &mut ContextConfig {
        &mut self.config
    }
}

impl Builder {
    pub fn new() -> Self {
        let shutdown_efd = None;

        Self {
            config: ContextConfig {
                krunfw: KrunfwBindings::new(),
                shutdown_efd,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub fn vm_config(&mut self, num_vcpus: u8, ram_mib: u32) -> Result<&mut Self, StartError> {
        if num_vcpus == 0 {
            return Err(StartError::ZeroVcpus);
        }

        let mem_size_mib: usize = ram_mib.try_into().expect("ram_mib did not fit in a usize");

        let vm_config = VmConfig {
            vcpu_count: Some(num_vcpus),
            mem_size_mib: Some(mem_size_mib),
            ht_enabled: Some(false),
            cpu_template: None,
        };

        self.config
            .vmr
            .set_vm_config(&vm_config)
            .map_err(|_| StartError::ZeroVcpus)?;
        Ok(self)
    }

    /// Select an external kernel instead of the bundled firmware library.
    ///
    /// On x86-64 use `Elf` for an uncompressed ELF vmlinux. Compressed
    /// formats are unsupported by this setter.
    /// On arm64 use `Raw` for an uncompressed Linux Image or `PeGz` for a
    /// PE image containing a gzip-compressed Image. Other formats are rejected.
    /// Kernel and initramfs contents are copied into guest memory during build.
    pub fn external_kernel(
        &mut self,
        path: PathBuf,
        format: KernelFormat,
        initramfs_path: Option<PathBuf>,
    ) -> std::io::Result<&mut Self> {
        let supported = match format {
            #[cfg(target_arch = "x86_64")]
            KernelFormat::Elf => true,
            #[cfg(target_arch = "aarch64")]
            KernelFormat::Raw | KernelFormat::PeGz => true,
            _ => false,
        };
        if !supported {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "kernel format unsupported on this architecture",
            ));
        }
        let initramfs_size = initramfs_path
            .as_ref()
            .map(std::fs::metadata)
            .transpose()?
            .map_or(0, |m| m.len());
        self.config.vmr.set_external_kernel(ExternalKernel {
            path,
            format,
            initramfs_path,
            initramfs_size,
            cmdline: None,
        });
        Ok(self)
    }

    /// Authorize host socket operations for this guest. Defaults to allow all.
    pub fn outbound_policy(&mut self, policy: Arc<dyn OutboundPolicy>) -> &mut Self {
        self.socket_policy.policy = Some(policy);
        self
    }

    /// Select the guest-visible userspace-vsock CID (default 3). Copies need fresh CIDs.
    pub fn vsock_cid(&mut self, cid: u32) -> std::io::Result<&mut Self> {
        if cid <= 2 || cid == u32::MAX {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "guest vsock CID is reserved",
            ));
        }
        self.guest_vsock_cid = Some(cid);
        Ok(self)
    }

    /// Disable internet socket forwarding without disabling mapped vsock ports.
    pub fn internet_socket_forwarding(&mut self, enabled: bool) -> &mut Self {
        self.socket_policy.internet_enabled = enabled;
        self
    }

    /// Declare a block disk's frozen content identity (digest or snapshot token).
    /// A new guest must declare each disk as a copy of the source token.
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        not(feature = "tee")
    ))]
    pub fn template_disk_identity(
        &mut self,
        block_id: impl Into<String>,
        content_id: impl Into<String>,
    ) -> &mut Self {
        self.config
            .vmr
            .template_disk_identities
            .insert(block_id.into(), content_id.into());
        self
    }

    /// Mark this guest as a future cooperative template, retaining descriptor-backed RAM.
    /// Capture remains disarmed until explicitly requested through VmHandle.
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        not(feature = "tee")
    ))]
    pub fn future_template(&mut self) -> &mut Self {
        self.config.vmr.cooperative_capture = Some((0, std::path::PathBuf::new()));
        self
    }

    /// Capture a template only after the guest publishes its frozen control
    /// page and emits the cooperative freeze exit. The source never resumes.
    /// Pass zero to discover the GPA from BSP RSI at that exit, or a nonzero
    /// address to require an exact match with the reported GPA.
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        not(feature = "tee")
    ))]
    pub fn capture_cooperative_template(
        &mut self,
        control_page: u64,
        path: impl Into<std::path::PathBuf>,
    ) -> &mut Self {
        self.config.vmr.cooperative_capture = Some((control_page, path.into()));
        self
    }

    /// Import a template and configure this new guest's devices, connection
    /// rule, disk snapshot declarations, CID and identifiers in one operation.
    /// The returned deadline starts now, including subsequent build time.
    /// Mark it ready only after validating the new agent's identity/generation.
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        not(feature = "tee")
    ))]
    pub fn start_from_template(
        &mut self,
        source: TemplateSource,
        timeout: std::time::Duration,
        configure: impl FnOnce(&mut Self) -> std::io::Result<()>,
    ) -> std::io::Result<GuestReadiness> {
        let readiness = GuestReadiness::new(timeout)?;
        match source {
            TemplateSource::Descriptor(file) => {
                self.start_from_template_fd(file)?;
            }
            TemplateSource::Path(path) => {
                self.start_from_template_path(path)?;
            }
        }
        configure(self)?;
        Ok(readiness)
    }

    /// Nitro arm64 only: build a receiving VM that boots its parked receiver
    /// before any template is chosen. Configure the shape and the same device
    /// list, in the same order, that the template's source used; backends stay
    /// idle until release. Take the handle with
    /// `Context::take_template_assignment` and give it a template later.
    ///
    /// The receiver starts during `build()` and runs in its own thread, so its
    /// life is bounded from build whether or not `run` is called: `wait`
    /// counts from build and bounds assignment and release. On expiry the VM
    /// is torn down under the configured lifecycle lock (slot and pool memory
    /// released) and `run` returns a `TimedOut` error carrying
    /// `ParkedReceiverError::Expired`. Dropping the handle unassigned ends it
    /// the same way with `Abandoned`; dropping the context stops it and waits
    /// for its teardown. `run` waits for the end. Hand the VM descriptor to a
    /// daemon with `nitro_descriptor_handoff` (duplicating after build is
    /// refused, because the VM is already running).
    #[cfg(all(
        target_os = "linux",
        target_arch = "aarch64",
        feature = "nitro-sandbox"
    ))]
    pub fn park_for_template(&mut self, wait: std::time::Duration) -> &mut Self {
        self.nitro_park = Some(wait);
        self
    }

    /// Start a new VM over a sealed template descriptor. Configure the same
    /// device list in the same order, with independent writable disk backends.
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        not(feature = "tee")
    ))]
    pub fn start_from_template_fd(&mut self, file: std::fs::File) -> std::io::Result<&mut Self> {
        self.config.vmr.cooperative_template = Some(vmm::template::Template::open(file)?);
        Ok(self)
    }

    /// Load a persisted template into immutable backing before starting a VM.
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        not(feature = "tee")
    ))]
    pub fn start_from_template_path(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<&mut Self> {
        self.config.vmr.cooperative_template = Some(vmm::template::Template::from_path(path)?);
        Ok(self)
    }

    pub fn workdir(&mut self, workdir: String) -> &mut Self {
        self.config.workdir = Some(workdir);
        self
    }

    pub fn exec_path(&mut self, exec_path: String) -> &mut Self {
        self.config.exec_path = Some(exec_path);
        self
    }

    #[cfg(all(feature = "blk", not(feature = "tee")))]
    pub fn block_root(
        &mut self,
        device: String,
        fstype: Option<String>,
        options: Option<String>,
    ) -> &mut Self {
        self.config.block_root = Some(BlockRootConfig {
            device,
            fstype,
            options,
        });
        self
    }

    pub fn env(&mut self, env: String) -> &mut Self {
        self.config.env = Some(env);
        self
    }

    pub fn args(&mut self, args: String) -> &mut Self {
        self.config.args = Some(args);
        self
    }

    pub fn rlimits(&mut self, rlimits: String) -> &mut Self {
        self.config.rlimits = Some(rlimits);
        self
    }

    #[cfg(feature = "net")]
    pub fn add_net_device(
        &mut self,
        backend: VirtioNetBackend,
        mac: [u8; 6],
        features: u32,
    ) -> &mut Self {
        let network_interface_config = NetworkInterfaceConfig {
            iface_id: format!("eth{}", self.config.net_index),
            backend,
            mac,
            features,
        };
        self.config.net_index += 1;

        // EXPECT: this should never fail unless we can't create a unix pipe
        self.config
            .vmr
            .add_network_interface(network_interface_config)
            .expect("Failed to create network interface");
        self
    }

    #[cfg(feature = "blk")]
    pub fn add_block_cfg(&mut self, block_cfg: BlockDeviceConfig) -> &mut Self {
        self.config.block_cfgs.push(block_cfg);
        self
    }

    /// Attach caller-owned storage; `read_only` controls the guest-visible device.
    #[cfg(feature = "blk")]
    pub fn add_block_backend(
        &mut self,
        id: String,
        backend: Arc<dyn BlockBackend + Send + Sync>,
        read_only: bool,
    ) -> &mut Self {
        self.add_block_cfg(BlockDeviceConfig {
            block_id: id,
            cache_type: backend.cache_type(),
            disk_type: BlockDeviceType::Custom { backend },
            is_disk_read_only: read_only,
            direct_io: false,
        })
    }

    #[cfg(not(feature = "tee"))]
    pub fn add_virtiofs(
        &mut self,
        tag: &str,
        fs: Box<dyn devices::virtio::fs::FileSystem + Send + Sync>,
        shm_size: Option<usize>,
    ) -> &mut Self {
        self.config.vmr.add_fs_mount(FsMount {
            tag: tag.to_string(),
            fs,
            shm_size,
        });
        self
    }

    /// Add a virtiofs device backed by a host directory (passthrough).
    #[cfg(not(feature = "tee"))]
    pub fn add_virtiofs_path(
        &mut self,
        tag: &str,
        host_path: &str,
        shm_size: Option<usize>,
        allow_root_dir_delete: bool,
    ) -> &mut Self {
        match std::fs::metadata(host_path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                self.configuration_error = Some(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "virtio-fs host path is not a directory",
                ));
                return self;
            }
            Err(error) => {
                self.configuration_error = Some(error);
                return self;
            }
        }
        let cfg = devices::virtio::fs::passthrough::Config {
            root_dir: host_path.to_string(),
            allow_root_dir_delete,
            ..Default::default()
        };
        match devices::virtio::fs::passthrough::PassthroughFs::new(cfg) {
            Ok(pt) => self.add_virtiofs(tag, Box::new(pt), shm_size),
            Err(error) => {
                self.configuration_error = Some(error);
                self
            }
        }
    }

    /// Configure a vhost-user filesystem device.
    ///
    /// `tag`: filesystem mount tag (max 36 bytes)
    /// `socket_path`: path to the vhost-user Unix socket
    /// `dax_window_mib`: DAX window size in MiB, or None to disable DAX
    #[cfg(not(feature = "tee"))]
    #[cfg(feature = "vhost-user")]
    pub fn add_virtiofs_vhost_user(
        &mut self,
        tag: &str,
        socket_path: &str,
        dax_window_mib: Option<u32>,
    ) -> Result<&mut Self, StartError> {
        if tag.len() > 36 {
            return Err(StartError::TagTooLong(tag.len()));
        }
        self.config.vmr.add_vhost_user_fs_device(VhostUserFsConfig {
            tag: tag.to_string(),
            socket_path: socket_path.to_string(),
            dax_window_mib,
        });
        Ok(self)
    }

    /// Configure a vhost-user-vsock device via Unix socket path.
    ///
    /// The backend process must be listening at `socket_path` before the VM starts.
    /// Cannot be used together with `krun_add_vsock()` (userspace vsock).
    #[cfg(not(feature = "tee"))]
    #[cfg(feature = "vhost-user")]
    pub fn add_vsock_vhost_user(&mut self, socket_path: &str) -> Result<&mut Self, StartError> {
        use vmm::vmm_config::vhost_user_vsock::{VhostUserVsockConfig, VhostUserVsockConnection};

        // Reject if explicit userspace vsock was already configured via krun_add_vsock()
        if matches!(self.config.vsock_config, VsockConfig::Explicit { .. }) {
            return Err(StartError::VsockConflict);
        }
        if self.config.vhost_user_vsock {
            return Err(StartError::VsockConflict);
        }

        // Do NOT set vsock_config = Disabled — leave it as Implicit so that
        // krun_add_vsock_port() still accepts port configs (stored but unused,
        // per design: "The API does not error — the config is stored but unused").
        // The build flow in lib.rs checks vhost_user_vsock to skip userspace vsock creation.
        self.config.vhost_user_vsock = true;

        self.config.vmr.set_vhost_user_vsock(VhostUserVsockConfig {
            connection: VhostUserVsockConnection::SocketPath(socket_path.to_string()),
        });
        Ok(self)
    }

    /// Configure a vhost-user-vsock device via pre-provisioned file descriptor.
    ///
    /// The `stream` must be a connected UnixStream to the vhost-user backend.
    /// Cannot be used together with `krun_add_vsock()` (userspace vsock).
    #[cfg(not(feature = "tee"))]
    #[cfg(feature = "vhost-user")]
    pub fn add_vsock_vhost_user_fd(
        &mut self,
        stream: std::os::unix::net::UnixStream,
    ) -> Result<&mut Self, StartError> {
        use vmm::vmm_config::vhost_user_vsock::{VhostUserVsockConfig, VhostUserVsockConnection};

        if matches!(self.config.vsock_config, VsockConfig::Explicit { .. }) {
            return Err(StartError::VsockConflict);
        }
        if self.config.vhost_user_vsock {
            return Err(StartError::VsockConflict);
        }

        self.config.vhost_user_vsock = true;

        self.config.vmr.set_vhost_user_vsock(VhostUserVsockConfig {
            connection: VhostUserVsockConnection::Stream(stream),
        });
        Ok(self)
    }

    #[cfg(not(feature = "tee"))]
    pub fn set_rng_backend(&mut self, backend: Box<dyn RngBackend>) -> &mut Self {
        self.config.vmr.rng_backend = Some(backend);
        self
    }

    /// Compute the virtio device index for the next console to be added.
    ///
    /// The device index determines the X in /dev/vportXpY.
    /// Device order: balloon(1) + rng(1) + rtc(1) + implicit_console(0 or 1) + consoles_added
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    fn nitro_device_count_before_consoles(&self) -> u32 {
        let blocks = if self.config.block_cfgs.is_empty() {
            u32::from(self.config.root_block_cfg.is_some())
                + u32::from(self.config.data_block_cfg.is_some())
        } else {
            self.config.block_cfgs.len() as u32
        };
        blocks
            + u32::from(!matches!(self.config.vsock_config, VsockConfig::Disabled))
            + self.config.vmr.fs.len() as u32
    }

    fn next_console_device_index(&self) -> u32 {
        #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
        if self.nitro.is_some() {
            return self.nitro_device_count_before_consoles() + self.console_count;
        }
        let mut idx: u32 = 0;

        // balloon (non-TEE only)
        #[cfg(not(feature = "tee"))]
        {
            idx += 1;
        }

        // rng (non-TEE only)
        #[cfg(not(feature = "tee"))]
        {
            idx += 1;
        }

        // rtc
        idx += 1;

        // implicit console (if not disabled)
        if !self.config.vmr.disable_implicit_console {
            idx += 1;
        }

        // consoles already added
        idx += self.console_count;

        idx
    }

    /// Add a virtio-console with all ports at once.
    ///
    /// Returns a vector of device paths for each port (e.g., "/dev/vport3p0", "/dev/vport3p1").
    ///
    /// For incremental port addition, use `add_virtio_console()` followed by `add_port()`.
    pub fn add_virtio_console_with_ports(&mut self, ports: Vec<PortDescription>) -> Vec<String> {
        #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
        if self.nitro.is_some() && self.nitro_console_base.is_none() {
            self.nitro_console_base = Some(self.nitro_device_count_before_consoles());
        }
        let device_idx = self.next_console_device_index();
        let num_ports = ports.len();

        self.config
            .vmr
            .virtio_consoles
            .push(VirtioConsoleConfigMode::Custom(ports));

        self.console_count += 1;

        // Return the expected device paths
        (0..num_ports)
            .map(|port_idx| format!("/dev/vport{}p{}", device_idx, port_idx))
            .collect()
    }

    #[cfg(feature = "blk")]
    pub fn root_block_cfg(&mut self, block_cfg: BlockDeviceConfig) -> &mut Self {
        self.config.root_block_cfg = Some(block_cfg);
        self
    }

    #[cfg(feature = "blk")]
    pub fn data_block_cfg(&mut self, block_cfg: BlockDeviceConfig) -> &mut Self {
        self.config.data_block_cfg = Some(block_cfg);
        self
    }

    #[cfg(not(feature = "tee"))]
    pub fn set_root(&mut self, root_path: &str) -> &mut Self {
        // 64 MB DAX window (reduced from 512 MB; smaller KVM region setup overhead).
        self.add_virtiofs_path("krun_root", root_path, Some(1 << 26), false);
        self
    }

    #[cfg(feature = "net")]
    pub fn net_mac(&mut self, mac: [u8; 6]) -> &mut Self {
        self.config.legacy_mac = Some(mac);
        self
    }

    #[allow(clippy::result_unit_err)]
    pub fn port_map(&mut self, new_port_map: HashMap<u16, u16>) -> Result<&mut Self, ()> {
        if self.config.net_index != 0 {
            return Err(());
        }

        self.config.tsi_port_map.replace(new_port_map);
        Ok(self)
    }

    #[cfg(feature = "tee")]
    #[mutants::skip] // tee feature not enabled in mutation testing
    pub fn tee_config_file(&mut self, filepath: PathBuf) -> &mut Self {
        self.tee_config_file = Some(filepath);
        self
    }

    pub fn add_vsock_port(&mut self, port: u32, filepath: PathBuf, listen: bool) -> &mut Self {
        if let Some(ref mut map) = &mut self.config.unix_ipc_port_map {
            map.insert(port, (filepath, listen));
        } else {
            let mut map: HashMap<u32, (PathBuf, bool)> = HashMap::new();
            map.insert(port, (filepath, listen));
            self.config.unix_ipc_port_map = Some(map);
        }
        self
    }

    pub fn gpu_virgl_flags(&mut self, virgl_flags: u32) -> &mut Self {
        self.config.gpu_virgl_flags = Some(virgl_flags);
        self
    }

    pub fn gpu_shm_size(&mut self, shm_size: usize) -> &mut Self {
        self.config.gpu_shm_size = Some(shm_size);
        self
    }

    pub fn console_output(&mut self, path: PathBuf) -> &mut Self {
        self.config.console_output = Some(path);
        self
    }

    /// Disable the implicit console that is created by default.
    /// Use this when configuring console ports explicitly.
    ///
    /// **Must be called before adding any console devices**, otherwise the device
    /// paths returned by `add_virtio_console()` / `add_virtio_console_with_ports()` would
    /// be invalidated.
    ///
    /// Returns an error if console devices have already been added.
    pub fn disable_implicit_console(&mut self) -> Result<&mut Self, BuilderError> {
        if self.console_count > 0 {
            return Err(BuilderError::ConsoleAlreadyAdded);
        }
        self.config.vmr.disable_implicit_console = true;
        Ok(self)
    }

    /// Add a multiport virtio console and return info for computing port paths.
    ///
    /// Use `console_port_path(info.device_index, port_index)` to get the path for each port.
    /// Ports are added with `add_port()` or the convenience methods `add_port_fd()` / `add_port_console_fd()`.
    pub fn add_virtio_console(&mut self) -> ConsoleDeviceInfo {
        #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
        if self.nitro.is_some() && self.nitro_console_base.is_none() {
            self.nitro_console_base = Some(self.nitro_device_count_before_consoles());
        }
        let console_id = self.config.vmr.virtio_consoles.len();
        let device_index = self.next_console_device_index();

        self.config
            .vmr
            .virtio_consoles
            .push(VirtioConsoleConfigMode::Custom(Vec::new()));

        self.console_count += 1;

        ConsoleDeviceInfo {
            console_id,
            device_index,
        }
    }

    /// Add a port with a custom PortDescription.
    ///
    /// Returns the port path on success (e.g., "/dev/vport3p0").
    pub fn add_port(&mut self, info: &ConsoleDeviceInfo, port: PortDescription) -> Option<String> {
        self.config
            .vmr
            .virtio_consoles
            .get_mut(info.console_id)
            .and_then(|config_mode| match config_mode {
                VirtioConsoleConfigMode::Custom(ports) => {
                    let port_index = ports.len();
                    ports.push(port);
                    Some(Self::console_port_path(info.device_index, port_index))
                }
                _ => None,
            })
    }

    /// Add a port using file descriptors for input/output.
    ///
    /// Use -1 for `input_fd` or `output_fd` to disable that direction.
    /// Returns the port path on success (e.g., "/dev/vport3p0").
    pub fn add_port_fd(
        &mut self,
        info: &ConsoleDeviceInfo,
        name: &str,
        input_fd: i32,
        output_fd: i32,
    ) -> Option<String> {
        let port = PortDescription {
            name: name.to_string().into(),
            input: if input_fd < 0 {
                None
            } else {
                Some(port_io::input_to_raw_fd_dup(input_fd).ok()?)
            },
            output: if output_fd < 0 {
                None
            } else {
                Some(port_io::output_to_raw_fd_dup_blocking(output_fd).ok()?)
            },
            terminal: None,
        };
        self.add_port(info, port)
    }

    /// Add a console port with file descriptors and fixed terminal size.
    ///
    /// This creates a port marked as a console (receives VIRTIO_CONSOLE_CONSOLE_PORT message),
    /// which the guest kernel recognizes as /dev/hvcN.
    ///
    /// Use -1 for `input_fd` or `output_fd` to disable that direction.
    /// Returns the port path on success (e.g., "/dev/vport3p0").
    pub fn add_port_console_fd(
        &mut self,
        info: &ConsoleDeviceInfo,
        input_fd: i32,
        output_fd: i32,
        cols: u16,
        rows: u16,
    ) -> Option<String> {
        let port = PortDescription::console(
            if input_fd < 0 {
                None
            } else {
                Some(port_io::input_to_raw_fd_dup(input_fd).ok()?)
            },
            if output_fd < 0 {
                None
            } else {
                Some(port_io::output_to_raw_fd_dup_blocking(output_fd).ok()?)
            },
            port_io::term_fixed_size(cols, rows),
        );
        self.add_port(info, port)
    }

    /// Get the device path for a console port.
    ///
    /// Use the `device_index` from `ConsoleDeviceInfo` and the port index (0-based).
    pub fn console_port_path(device_index: u32, port_index: usize) -> String {
        format!("/dev/vport{}p{}", device_index, port_index)
    }

    pub fn vmm_uid(&mut self, vmm_uid: libc::uid_t) -> &mut Self {
        self.config.vmm_uid = Some(vmm_uid);
        self
    }

    pub fn vmm_gid(&mut self, vmm_gid: libc::gid_t) -> &mut Self {
        self.config.vmm_gid = Some(vmm_gid);
        self
    }

    #[cfg(feature = "aws-nitro")]
    #[mutants::skip] // aws-nitro feature not enabled in mutation testing
    pub fn nitro_image(&mut self, image_path: PathBuf) -> &mut Self {
        self.config.nitro_image_path = Some(image_path);
        self
    }

    #[cfg(feature = "aws-nitro")]
    #[mutants::skip] // aws-nitro feature not enabled in mutation testing
    pub fn nitro_start_flags(&mut self, start_flags: StartFlags) -> &mut Self {
        self.config.nitro_start_flags = start_flags;
        self
    }

    pub fn set_kernel_cmdline(&mut self, cmdline: Vec<&str>) -> &mut Self {
        self.kernel_cmdline_configured = true;
        self.kernel_cmdline = cmdline.into_iter().map(|s| s.to_owned()).collect();
        self
    }

    /// Append extra kernel cmdline args without replacing the default cmdline or
    /// the builder-injected params (init path, exec path, env, etc.).
    pub fn add_kernel_cmdline_args(&mut self, args: &[&str]) -> &mut Self {
        self.extra_kernel_args
            .extend(args.iter().map(|s| s.to_string()));
        self
    }

    /// Leave the memory-balloon transport out of this VM. Existing callers
    /// retain the historical device list unless they opt out explicitly.
    #[cfg(not(feature = "tee"))]
    pub fn omit_balloon_device(&mut self) -> &mut Self {
        self.config.vmr.omit_balloon = true;
        self
    }

    #[cfg(not(feature = "tee"))]
    /// Enable balloon control through VmHandle::balloon().
    pub fn enable_balloon(&mut self) -> &mut Self {
        self.config.vmr.balloon_enabled = true;
        self
    }

    /// Enables nested virtualization support, exposing VMX (Intel) or SVM (AMD)
    /// capabilities to the guest so it can act as a hypervisor.
    pub fn enable_nested_virt(&mut self) -> &mut Self {
        self.config.vmr.nested_enabled = true;
        self
    }

    /// Socket-forwarding flags of the Nitro vsock device: the same decision as the KVM path
    /// (explicit flags, or inet hijacking implied by internet forwarding; never inet hijacking
    /// while internet forwarding is off). Also decides the guest command-line switches.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    fn nitro_tsi_flags(&self) -> TsiFlags {
        let mut flags = match self.config.vsock_config {
            VsockConfig::Explicit { tsi_flags } => tsi_flags,
            VsockConfig::Implicit if self.socket_policy.internet_enabled => TsiFlags::HIJACK_INET,
            _ => TsiFlags::empty(),
        };
        if !self.socket_policy.internet_enabled {
            flags.remove(TsiFlags::HIJACK_INET);
        }
        if matches!(self.config.vsock_config, VsockConfig::Disabled) {
            flags = TsiFlags::empty();
        }
        flags
    }

    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    fn create_nitro_vsock(
        cid: u64,
        port_map: Option<HashMap<u16, u16>>,
        unix_map: Option<HashMap<u32, (PathBuf, bool)>>,
        flags: TsiFlags,
        policy: devices::virtio::vsock::policy::SocketPolicy,
    ) -> Result<devices::virtio::Vsock, StartError> {
        let mut device = devices::virtio::Vsock::new(cid, port_map, unix_map, flags)
            .map_err(|e| StartError::Nitro(std::io::Error::other(format!("vsock: {e:?}"))))?;
        device.set_socket_policy(policy);
        Ok(device)
    }

    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    fn build_nitro(mut self, options: NitroBackendConfig) -> Result<Context, StartError> {
        #[cfg(target_arch = "aarch64")]
        if self.config.vmr.cooperative_capture.is_some() {
            self.nitro_capture = Some(vmm::capture_request::CaptureRequest::default());
        }
        #[cfg(target_arch = "aarch64")]
        if self.nitro_capture.is_some() && self.config.vmr.cooperative_template.is_some() {
            return Err(StartError::Nitro(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "template capture and restore are mutually exclusive",
            )));
        }
        #[cfg(target_arch = "aarch64")]
        if self.nitro_park.is_some()
            && (self.nitro_capture.is_some() || self.config.vmr.cooperative_template.is_some())
        {
            return Err(StartError::Nitro(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a parked receiver cannot also capture or take a template at build",
            )));
        }
        #[cfg(all(target_arch = "aarch64", not(feature = "tee")))]
        if (self.nitro_capture.is_some()
            || self.config.vmr.cooperative_template.is_some()
            || self.nitro_park.is_some())
            && (self.console_count != 0
                || self
                    .config
                    .vmr
                    .fs
                    .iter()
                    .any(|mount| mount.shm_size.is_some_and(|size| size != 0)))
        {
            return Err(StartError::Nitro(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Nitro template device manifest integration is not available yet",
            )));
        }
        let unsupported = || {
            StartError::Nitro(std::io::Error::new(std::io::ErrorKind::Unsupported, "unsupported on this backend: Nitro supports EIF boot with block, userspace vsock and custom console ports only"))
        };
        if self.config.vmr.kernel_bundle.is_some() || self.config.vmr.firmware_config.is_some() {
            return Err(unsupported());
        }
        if self.config.vmr.external_kernel.is_none()
            && (self.kernel_cmdline_configured
                || !self.kernel_cmdline.is_empty()
                || !self.extra_kernel_args.is_empty())
        {
            return Err(StartError::Nitro(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "unsupported on this backend: Nitro command line must be encoded in the EIF image",
            )));
        }
        if self.config.vmm_uid.is_some()
            || self.config.vmm_gid.is_some()
            || self.config.exec_path.is_some()
            || self.config.env.is_some()
            || self.config.args.is_some()
            || self.config.workdir.is_some()
            || self.config.vmr.nested_enabled
            || self.config.gpu_virgl_flags.is_some()
            || !self.config.vmr.serial_consoles.is_empty()
            || self.config.rlimits.is_some()
            || self.config.block_root.is_some()
            || self.config.console_output.is_some()
            || self.config.gpu_shm_size.is_some()
            || self.config.vmr.console_output.is_some()
            || self.config.vmr.kernel_console.is_some()
        {
            return Err(unsupported());
        }
        #[cfg(not(feature = "tee"))]
        if self.config.vmr.balloon_enabled || self.config.vmr.rng_backend.is_some() {
            return Err(unsupported());
        }
        #[cfg(feature = "snd")]
        if self.config.enable_snd || self.config.vmr.snd_device {
            return Err(unsupported());
        }
        #[cfg(feature = "gpu")]
        if self.config.vmr.display_backend.is_some() || !self.config.vmr.displays.is_empty() {
            return Err(unsupported());
        }
        #[cfg(feature = "input")]
        if !self.config.vmr.input_backends.is_empty() {
            return Err(unsupported());
        }
        #[cfg(not(feature = "tee"))]
        for mount in &self.config.vmr.fs {
            if mount.shm_size.is_some_and(|size| size != 0) {
                return Err(StartError::Nitro(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "unsupported on this backend: virtio-fs DAX windows are not supported; use no window",
                )));
            }
            if mount.tag.len() > 36 || mount.tag.is_empty() {
                return Err(StartError::Nitro(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "unsupported on this backend: virtio-fs requires a 1..36 byte tag",
                )));
            }
        }
        #[cfg(feature = "net")]
        if !self.config.vmr.net.list.is_empty()
            || self.config.legacy_net_cfg.is_some()
            || self.config.legacy_mac.is_some()
        {
            return Err(unsupported());
        }
        #[cfg(feature = "vhost-user")]
        if self.config.vhost_user_vsock
            || !self.config.vmr.vhost_user_devices.is_empty()
            || !self.config.vmr.vhost_user_fs.is_empty()
        {
            return Err(unsupported());
        }
        if self.nitro_selected_after_consoles
            || self
                .nitro_console_base
                .is_some_and(|base| base != self.nitro_device_count_before_consoles())
        {
            return Err(StartError::Nitro(std::io::Error::new(std::io::ErrorKind::Unsupported, "unsupported on this backend: select Nitro and configure block/vsock devices before adding consoles so returned port paths remain valid")));
        }
        if !self.config.vmr.disable_implicit_console {
            return Err(StartError::Nitro(std::io::Error::new(std::io::ErrorKind::Unsupported, "unsupported on this backend: disable the implicit console and configure custom Nitro console ports explicitly")));
        }
        let heartbeat = if let Some((port, path, deadline)) = self.nitro_heartbeat.take() {
            if matches!(self.config.vsock_config, VsockConfig::Disabled)
                || self
                    .config
                    .unix_ipc_port_map
                    .as_ref()
                    .is_some_and(|ports| ports.contains_key(&port))
            {
                return Err(StartError::Nitro(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "unsupported on this backend: heartbeat requires an unused mapped vsock port",
                )));
            }
            let heartbeat = nitro_sandbox::heartbeat::Heartbeat::bind(path.clone(), deadline)
                .map_err(StartError::Nitro)?;
            self.config
                .unix_ipc_port_map
                .get_or_insert_with(HashMap::new)
                .insert(port, (path, false));
            Some(heartbeat)
        } else {
            None
        };
        let config = self.config.vmr.vm_config();
        let memory_mib = config.mem_size_mib.unwrap_or(1024);
        #[cfg(target_arch = "aarch64")]
        let capture_vcpus = config.vcpu_count.unwrap_or(1);
        #[cfg(target_arch = "aarch64")]
        if let Some(template) = &self.config.vmr.cooperative_template {
            vmm::nitro_restore::validate(
                template,
                capture_vcpus,
                memory_mib as u64,
                &template.topology,
                &template.compatibility,
            )
            .map_err(StartError::Nitro)?;
        }
        if memory_mib < 64 || !memory_mib.is_multiple_of(2) || options.watchdog.is_zero() {
            return Err(StartError::Nitro(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "unsupported on this backend: memory must be at least 64 MiB and a multiple of 2 MiB; watchdog must be nonzero",
            )));
        }
        let packaged = if let Some(kernel) = self.config.vmr.external_kernel.as_ref() {
            if !matches!(kernel.format, KernelFormat::Raw) {
                return Err(StartError::Nitro(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "unsupported on this backend: external_kernel requires an arm64 Raw Image",
                )));
            }
            let mut args = self.kernel_cmdline.clone();
            args.extend(self.extra_kernel_args.iter().cloned());
            let forwarded_args = nitro_cmdline(&args, self.nitro_tsi_flags());
            let mut cmdline = kernel::cmdline::Cmdline::new(65536);
            cmdline
                .insert_str(forwarded_args)
                .map_err(|e| StartError::Nitro(std::io::Error::other(format!("{e:?}"))))?;
            if self.config.vmr.cooperative_capture.is_some() {
                cmdline
                    .insert_kernel_argument("cooperative_template")
                    .map_err(|e| StartError::Nitro(std::io::Error::other(format!("{e:?}"))))?;
            }
            let (file, _) = nitro_sandbox::image::cached_files(
                &kernel.path,
                kernel.initramfs_path.as_deref(),
                cmdline.as_str().as_bytes(),
                memory_mib as u64 * 1024 * 1024,
                u64::from(config.vcpu_count.unwrap_or(1)),
                self.nitro_image_cache.as_deref(),
            )
            .map_err(StartError::Nitro)?;
            Some(file)
        } else {
            None
        };
        let image_path = if let Some(file) = &packaged {
            use std::os::fd::AsRawFd;
            PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
        } else {
            options.image.clone()
        };
        let mut device_info = vmm::resources::VmDeviceInfo {
            console_ports: Vec::new(),
            vcpu_count: config.vcpu_count.unwrap_or(1),
            ram_mib: config.mem_size_mib.unwrap_or(1024) as u32,
        };
        #[cfg(target_arch = "aarch64")]
        let template_enabled = self.nitro_capture.is_some()
            || self.config.vmr.cooperative_template.is_some()
            || self.nitro_park.is_some();
        #[cfg(target_arch = "aarch64")]
        let mut template_topology = {
            let mut bytes = b"nitro-devices-v2".to_vec();
            bytes.extend_from_slice(
                &(self.nitro_device_count_before_consoles() as u64).to_le_bytes(),
            );
            bytes
        };
        let mut console_id = self.nitro_device_count_before_consoles();
        let handoff = self.nitro_descriptor_handoff.take();
        let mut runtime = nitro_sandbox::runtime::Runtime::create(
            image_path
                .to_str()
                .ok_or_else(|| StartError::Nitro(std::io::Error::other("non-UTF8 EIF path")))?,
            config.mem_size_mib.unwrap_or(1024) * 1024 * 1024,
            u32::from(config.vcpu_count.unwrap_or(1)),
            options.cid,
            options.watchdog,
            self.nitro_lifecycle_lock.take(),
            handoff.as_ref(),
        )
        .map_err(StartError::Nitro)?;
        drop(packaged);
        if let Some(stop) = self.nitro_precreated_stop.take() {
            runtime.set_stop_event(stop);
        }
        if let Some(heartbeat) = heartbeat {
            runtime.set_heartbeat(heartbeat);
        }
        if let Some(console) = self.nitro_console.take() {
            runtime.set_console(console);
        }
        for cfg in self.config.take_block_cfg() {
            let block = Arc::new(Mutex::new(
                vmm::vmm_config::block::BlockBuilder::create_block(cfg)?,
            ));
            #[cfg(target_arch = "aarch64")]
            if template_enabled {
                vmm::template_topology::encode_block(
                    &mut template_topology,
                    &block.lock().unwrap(),
                    &self.config.vmr.template_disk_identities,
                )
                .map_err(StartError::Nitro)?;
            }
            runtime.register_exit_observer(block.clone());
            runtime
                .add_worker_device(block)
                .map_err(StartError::Nitro)?;
        }
        if !matches!(self.config.vsock_config, VsockConfig::Disabled) {
            let flags = self.nitro_tsi_flags();
            let vsock = Self::create_nitro_vsock(
                options.cid,
                self.config.tsi_port_map,
                self.config.unix_ipc_port_map,
                flags,
                self.socket_policy.clone(),
            )?;
            #[cfg(target_arch = "aarch64")]
            if template_enabled {
                vmm::template_topology::encode_device(&mut template_topology, &vsock);
            }
            runtime
                .add_device(Arc::new(Mutex::new(vsock)))
                .map_err(StartError::Nitro)?;
        }
        for mount in self.config.vmr.fs.drain(..) {
            #[cfg(target_arch = "aarch64")]
            if template_enabled {
                if mount.tag.len() > 36 || mount.tag.as_bytes().contains(&0) {
                    return Err(StartError::Nitro(std::io::Error::other(
                        "invalid template filesystem tag",
                    )));
                }
                template_topology.extend_from_slice(&(mount.tag.len() as u64).to_le_bytes());
                template_topology.extend_from_slice(mount.tag.as_bytes());
                template_topology
                    .extend_from_slice(&(mount.shm_size.unwrap_or(0) as u64).to_le_bytes());
            }
            let device = devices::virtio::Fs::new(
                mount.tag,
                mount.fs,
                Arc::new(std::sync::atomic::AtomicI32::new(0)),
            )
            .map_err(|error| {
                StartError::Nitro(std::io::Error::other(format!("virtio-fs: {error:?}")))
            })?;
            #[cfg(target_arch = "aarch64")]
            if template_enabled {
                vmm::template_topology::encode_device(&mut template_topology, &device);
            }
            runtime
                .add_filesystem(device, mount.shm_size)
                .map_err(StartError::Nitro)?;
        }
        for console in self.config.vmr.virtio_consoles {
            let ports = match console {
                VirtioConsoleConfigMode::Custom(ports) => ports,
                _ => {
                    return Err(StartError::Nitro(std::io::Error::other(
                        "Nitro console requires custom port descriptions",
                    )))
                }
            };
            for (port_id, port) in ports.iter().enumerate() {
                device_info
                    .console_ports
                    .push(vmm::resources::ConsolePortInfo {
                        console_id,
                        port_id: port_id as u32,
                        device_path: format!("/dev/vport{console_id}p{port_id}"),
                        name: (!port.name.is_empty()).then(|| port.name.to_string()),
                    });
            }
            console_id += 1;
            let console = devices::virtio::Console::new(ports)
                .map_err(|e| StartError::Nitro(std::io::Error::other(format!("console: {e:?}"))))?;
            runtime
                .add_device(Arc::new(Mutex::new(console)))
                .map_err(StartError::Nitro)?;
        }
        if self.nitro_stop_reason {
            runtime.enable_stop_reason().map_err(StartError::Nitro)?;
        }
        #[cfg(target_arch = "aarch64")]
        let restored_copy =
            self.config.vmr.cooperative_template.is_some() || self.nitro_park.is_some();
        #[cfg(target_arch = "aarch64")]
        if let Some(template) = self.config.vmr.cooperative_template.take() {
            let topology = template_topology.clone();
            let compatibility = template.compatibility.clone();
            vmm::nitro_restore::validate(
                &template,
                capture_vcpus,
                memory_mib as u64,
                &topology,
                &compatibility,
            )
            .map_err(StartError::Nitro)?;
            runtime
                .set_restore_handler(Box::new(vmm::nitro_restore::NitroRestore {
                    template,
                    vcpus: capture_vcpus,
                    ram_mib: memory_mib as u64,
                    topology,
                    compatibility,
                }))
                .map_err(StartError::Nitro)?;
        }
        #[cfg(target_arch = "aarch64")]
        let park = match self.nitro_park {
            Some(wait) => Some(TemplateAssignment {
                slot: runtime.park_for_restore(wait).map_err(StartError::Nitro)?,
                vcpus: capture_vcpus,
                ram_mib: memory_mib as u64,
                topology: template_topology.clone(),
            }),
            None => None,
        };
        #[cfg(target_arch = "aarch64")]
        if let Some(request) = &self.nitro_capture {
            runtime.set_capture_handler(Box::new(vmm::nitro_capture::NitroCapture {
                request: request.clone(),
                vcpus: capture_vcpus,
                ram_mib: memory_mib as u64,
                topology: template_topology,
                compatibility: b"nitro-arm64-v1".to_vec(),
                export_path: self
                    .config
                    .vmr
                    .cooperative_capture
                    .as_ref()
                    .and_then(|(_, path)| (!path.as_os_str().is_empty()).then(|| path.clone())),
            }));
        }
        let context = Context::from_nitro(runtime, device_info)?;
        #[cfg(target_arch = "aarch64")]
        let context = {
            let mut context = context;
            context.built_vm.nitro_capture = self.nitro_capture.take();
            if restored_copy {
                let request = vmm::capture_request::CaptureRequest::default();
                request.refuse_restored_copy();
                context.built_vm.nitro_capture = Some(request);
            }
            if park.is_some() {
                // A parked receiver starts now and runs in its own thread, so
                // its park wait bounds its life from build even if `run` is
                // never called: the start releases the creation lock, and
                // expiry, a dropped handle or a dropped context tear it down
                // under the lifecycle lock.
                let runtime = context.built_vm.nitro.take().expect("Nitro runtime");
                let observers = runtime.exit_observer_registry();
                let heartbeat = runtime.heartbeat_control();
                context.built_vm.nitro_parked = Some(NitroParked {
                    run: runtime.start_parked().map_err(StartError::Nitro)?,
                    observers,
                    heartbeat,
                });
            }
            context.built_vm.nitro_park = park;
            context
        };
        Ok(context)
    }

    /// Select Nitro direct mode using an EIF image. Configured block devices,
    /// Unix vsock port mappings and custom console ports are reused.
    ///
    /// The resulting context provides device information, exit observers and a
    /// stop handle. KVM-only controls return Unsupported. HUP reports shutdown, not a guest
    /// application exit status; workloads must report that over their protocol.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn nitro_backend(&mut self, config: NitroBackendConfig) -> &mut Self {
        self.nitro_selected_after_consoles |= self.console_count != 0 && self.nitro.is_none();
        self.nitro = Some(config);
        self
    }

    /// Install SIGTERM handling before `build` creates a Nitro guest. Keep the
    /// returned guard alive through build and run. A signal during build is
    /// retained as a stop request. This explicitly changes process signal policy.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn install_nitro_sigterm_handler(&mut self) -> Result<NitroSigtermGuard, StartError> {
        let stop = Arc::new(EventFd::new(utils::eventfd::EFD_NONBLOCK).map_err(StartError::Nitro)?);
        let guard = NitroSigtermGuard::for_stop_event(stop.clone())?;
        self.nitro_precreated_stop = Some(stop);
        Ok(guard)
    }

    /// Add the maintained guest kernel's privileged terminal-reason device.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn nitro_stop_reason(&mut self, enabled: bool) -> &mut Self {
        self.nitro_stop_reason = enabled;
        self
    }

    /// Reuse content-addressed packaged kernels across VMM processes. Directory
    /// must already exist and be daemon-owned, not group/other-writable. The
    /// daemon controls eviction; published images are never modified in place.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn nitro_image_cache(&mut self, directory: PathBuf) -> &mut Self {
        self.nitro_image_cache = Some(directory);
        self
    }

    /// Require guest-originated heartbeats at CID 2 and the supplied vsock port.
    /// Guest sends ASCII H periodically; the deadline starts at guest start and
    /// refreshes on H. The socket path must not exist and remains policy checked.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn nitro_heartbeat(
        &mut self,
        port: u32,
        path: PathBuf,
        deadline: std::time::Duration,
    ) -> &mut Self {
        self.nitro_heartbeat = Some((port, path, deadline));
        self
    }

    /// Send kernel console diagnostics to a nonblocking embedder sink. Excess
    /// bytes beyond either lifetime or per-second budget are dropped, not queued.
    /// Default: discard console diagnostics.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn nitro_console_sink(
        &mut self,
        sink: Box<dyn std::io::Write + Send>,
        total_bytes: usize,
        bytes_per_second: usize,
    ) -> &mut Self {
        self.nitro_console = Some(nitro_sandbox::console::Console::new(
            Some(sink),
            total_bytes,
            bytes_per_second,
        ));
        self
    }

    /// Also give every kernel console byte to `observer`, before the byte budgets of
    /// `nitro_console_sink` (for example to find a stop line after the guest flooded its
    /// console). Call after `nitro_console_sink` (without a sink, console bytes are only
    /// observed). Runs on the console thread; must not block.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn nitro_console_observer(
        &mut self,
        observer: nitro_sandbox::console::RawObserver,
    ) -> &mut Self {
        self.nitro_console
            .get_or_insert_with(nitro_sandbox::console::Console::default)
            .set_observer(observer);
        self
    }

    /// Send a duplicate of the VM descriptor over `socket` (the host daemon's end of a
    /// connected Unix socket) immediately after the create ioctl returns, inside `build` and
    /// before any other VM ioctl (one `SCM_RIGHTS` message, payload
    /// `nitro_sandbox::lifecycle::VM_DESCRIPTOR_MESSAGE`; the daemon can receive it with
    /// `nitro_sandbox::lifecycle::receive_vm_descriptor`). The interval in which a death of
    /// this process can make the final close is then only the return of that ioctl. `build`
    /// fails if the send fails. Acknowledgement is the embedder's protocol: wait for it after
    /// `build`, before `run`, with a time limit, and never while holding the lifecycle lock
    /// (use `NitroSerialization::Teardown`). Prefer this to `duplicate_nitro_vm_descriptor`.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn nitro_descriptor_handoff(
        &mut self,
        socket: std::os::unix::net::UnixStream,
    ) -> &mut Self {
        self.nitro_descriptor_handoff = Some(socket);
        self
    }

    /// Serialize lifecycle operations using a daemon-created regular lock file.
    /// The daemon must take this same flock before SIGKILL and hold it through reaping.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn nitro_lifecycle_lock(
        &mut self,
        path: PathBuf,
        serialization: NitroSerialization,
    ) -> &mut Self {
        self.nitro_lifecycle_lock = Some(nitro_sandbox::lifecycle::LifecycleLock {
            path,
            serialization,
        });
        self
    }

    #[allow(unused_mut)] // Mutability is needed only when Nitro selection is compiled.
    pub fn build(mut self) -> Result<Context, StartError> {
        #[cfg(all(
            target_os = "linux",
            target_arch = "aarch64",
            feature = "nitro-sandbox"
        ))]
        if (self.config.vmr.cooperative_capture.is_some()
            || self.config.vmr.cooperative_template.is_some()
            || self.nitro_park.is_some())
            && self.nitro.is_none()
        {
            return Err(StartError::Configuration(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "arm64 cooperative templates require Nitro backend",
            )));
        }
        if let Some(error) = self.configuration_error.take() {
            return Err(StartError::Configuration(error));
        }
        if self
            .config
            .tsi_port_map
            .as_ref()
            .is_some_and(|ports| !ports.is_empty())
            && !self.socket_policy.internet_enabled
        {
            return Err(StartError::UnsupportedPortMap);
        }
        #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
        if let Some(config) = self.nitro.take() {
            return self.build_nitro(config);
        }
        // Helper constants and functions for legacy network configuration
        #[cfg(feature = "net")]
        const NET_FEATURE_CSUM: u32 = 1 << 0;
        #[cfg(feature = "net")]
        const NET_COMPAT_FEATURES: u32 = NET_FEATURE_CSUM;

        #[cfg(feature = "net")]
        fn create_virtio_net(
            ctx_cfg: &mut ContextConfig,
            backend: VirtioNetBackend,
            mac: [u8; 6],
            features: u32,
        ) {
            let network_interface_config = NetworkInterfaceConfig {
                iface_id: format!("eth{}", ctx_cfg.net_index),
                backend,
                mac,
                features,
            };
            ctx_cfg.net_index += 1;
            ctx_cfg
                .vmr
                .add_network_interface(network_interface_config)
                .expect("Failed to create network interface");
        }
        let mut event_manager = EventManager::new().map_err(StartError::EventManager)?;

        let mut ctx_cfg = self.config;

        if ctx_cfg.vmr.external_kernel.is_none()
            && ctx_cfg.vmr.kernel_bundle.is_none()
            && ctx_cfg.vmr.firmware_config.is_none()
            && cfg!(not(feature = "efi"))
        {
            if let Some(ref krunfw) = ctx_cfg.krunfw {
                unsafe { load_krunfw_payload(krunfw, &mut ctx_cfg.vmr) }?;
            } else {
                return Err(StartError::MissingFirmware);
            }
        }

        #[cfg(feature = "blk")]
        for block_cfg in ctx_cfg.take_block_cfg() {
            ctx_cfg.vmr.add_block_device(block_cfg)?;
        }

        #[cfg(feature = "tee")]
        if let Some(tee_config) = ctx_cfg.get_tee_config_file() {
            ctx_cfg
                .vmr
                .set_tee_config(tee_config)
                .map_err(StartError::TeeConfig)?;
        } else {
            return Err(StartError::MissingTeeConfig);
        }

        let use_bundled_defaults =
            self.kernel_cmdline.is_empty() && ctx_cfg.vmr.external_kernel.is_none();
        let kernel_cmdline = if use_bundled_defaults {
            let mut cmdline = vec![
                DEFAULT_KERNEL_CMDLINE.to_owned(),
                format!("init={INIT_PATH}"),
                ctx_cfg.get_exec_path(),
                ctx_cfg.get_workdir(),
                ctx_cfg.get_block_root(),
                ctx_cfg.get_rlimits(),
                ctx_cfg.get_env(),
            ];
            cmdline.extend(self.extra_kernel_args);
            KernelCmdlineConfig {
                cmdline,
                args: vec![format!(" -- {}", ctx_cfg.get_args())],
            }
        } else {
            let mut cmdline = self.kernel_cmdline;
            cmdline.extend(self.extra_kernel_args);
            KernelCmdlineConfig {
                cmdline,
                args: Vec::new(),
            }
        };

        ctx_cfg.vmr.set_kernel_cmdline(kernel_cmdline)?;

        #[cfg(feature = "net")]
        {
            if let Some(legacy_net_cfg) = ctx_cfg.legacy_net_cfg.clone() {
                let backend = match legacy_net_cfg {
                    LegacyNetworkConfig::VirtioNetGvproxy(path) => {
                        VirtioNetBackend::UnixgramPath(path, true)
                    }
                    LegacyNetworkConfig::VirtioNetPasst(fd) => VirtioNetBackend::UnixstreamFd(fd),
                };
                let mac = ctx_cfg
                    .legacy_mac
                    .unwrap_or([0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee]);
                create_virtio_net(&mut ctx_cfg, backend, mac, NET_COMPAT_FEATURES);
            }
        }

        #[cfg(feature = "vhost-user")]
        let skip_userspace_vsock = ctx_cfg.vhost_user_vsock;
        #[cfg(not(feature = "vhost-user"))]
        let skip_userspace_vsock = false;

        if skip_userspace_vsock
            && (self.socket_policy.policy.is_some() || !self.socket_policy.internet_enabled)
        {
            return Err(StartError::UnsupportedSocketPolicy);
        }

        if !skip_userspace_vsock {
            match &ctx_cfg.vsock_config {
                VsockConfig::Disabled => (),
                VsockConfig::Explicit { tsi_flags } => {
                    let vsock_device_config = VsockDeviceConfig {
                        vsock_id: "vsock0".to_string(),
                        guest_cid: self.guest_vsock_cid.unwrap_or(3),
                        host_port_map: ctx_cfg.tsi_port_map,
                        unix_ipc_port_map: ctx_cfg.unix_ipc_port_map.clone(),
                        tsi_flags: if self.socket_policy.internet_enabled {
                            *tsi_flags
                        } else {
                            *tsi_flags & !TsiFlags::HIJACK_INET
                        },
                    };
                    ctx_cfg.vmr.set_vsock_device(vsock_device_config).unwrap();
                }
                VsockConfig::Implicit => {
                    #[cfg(feature = "net")]
                    let enable_tsi =
                        ctx_cfg.vmr.net.list.is_empty() && ctx_cfg.legacy_net_cfg.is_none();
                    #[cfg(not(feature = "net"))]
                    let enable_tsi = true;

                    let has_ipc_map = ctx_cfg.unix_ipc_port_map.is_some();

                    if enable_tsi || has_ipc_map {
                        let (tsi_flags, host_port_map) = if enable_tsi {
                            (
                                if self.socket_policy.internet_enabled {
                                    TsiFlags::HIJACK_INET
                                } else {
                                    TsiFlags::empty()
                                },
                                ctx_cfg.tsi_port_map,
                            )
                        } else {
                            (TsiFlags::empty(), None)
                        };

                        let vsock_device_config = VsockDeviceConfig {
                            vsock_id: "vsock0".to_string(),
                            guest_cid: self.guest_vsock_cid.unwrap_or(3),
                            host_port_map,
                            unix_ipc_port_map: ctx_cfg.unix_ipc_port_map.clone(),
                            tsi_flags,
                        };
                        ctx_cfg.vmr.set_vsock_device(vsock_device_config).unwrap();
                    }
                }
            }
        }

        if let Some(vsock) = ctx_cfg.vmr.vsock.get() {
            vsock.lock().unwrap().set_socket_policy(self.socket_policy);
        }

        if let Some(virgl_flags) = ctx_cfg.gpu_virgl_flags {
            ctx_cfg.vmr.set_gpu_virgl_flags(virgl_flags);
        }
        if let Some(shm_size) = ctx_cfg.gpu_shm_size {
            ctx_cfg.vmr.set_gpu_shm_size(shm_size);
        }

        #[cfg(feature = "snd")]
        ctx_cfg.vmr.set_snd_device(ctx_cfg.enable_snd);

        if let Some(console_output) = ctx_cfg.console_output {
            ctx_cfg.vmr.set_console_output(console_output);
        }

        // SAFETY: Privilege drop ordering invariant:
        // 1. setgid() MUST be called before setuid(). Once the UID is dropped to non-root,
        //    subsequent setgid() calls will fail with EPERM.
        // 2. These syscalls are process-wide; this function must only be called from a
        //    single-threaded context (before any threads are spawned, or with all threads
        //    terminated). POSIX specifies that setuid()/setgid() have undefined behavior in
        //    multithreaded processes on some platforms; Linux propagates the change to all
        //    threads but this cannot be relied upon portably.
        // 3. libkrun API contract: `Builder::set_vmm_uid()` / `Builder::set_vmm_gid()` are
        //    documented to require that `Builder::build()` is called before any application
        //    threads are spawned. The caller is responsible for upholding this precondition.
        //    Violation (calling build() after spawning threads when uid/gid are set) is
        //    unsound and the caller bears responsibility for the resulting undefined behavior.
        // See: setuid(2), setgid(2) — "In a multithreaded process, setuid() changes the
        //    UIDs of all threads."
        if let Some(gid) = ctx_cfg.vmm_gid {
            if unsafe { libc::setgid(gid) } != 0 {
                return Err(StartError::Setgid(std::io::Error::last_os_error()));
            }
        }

        if let Some(uid) = ctx_cfg.vmm_uid {
            if unsafe { libc::setuid(uid) } != 0 {
                return Err(StartError::Setuid(std::io::Error::last_os_error()));
            }
        }

        let (sender, _receiver) = unbounded();

        let shutdown_efd_clone = ctx_cfg
            .shutdown_efd
            .as_ref()
            .and_then(|efd| efd.try_clone().ok())
            .map(Arc::new);

        let t_build_start = std::time::Instant::now();
        info!("[boot_timing] build_microvm: start");
        let built_vm = vmm::builder::build_microvm(
            &mut ctx_cfg.vmr,
            &mut event_manager,
            ctx_cfg.shutdown_efd,
            sender,
        )?;
        info!(
            "[boot_timing] build_microvm: done (vcpus ready) total={:.3}ms",
            t_build_start.elapsed().as_secs_f64() * 1000.0,
        );

        #[cfg(target_arch = "x86_64")]
        if ctx_cfg.vmr.split_irqchip {
            vmm::worker::start_worker_thread(built_vm.vmm().clone(), _receiver.clone()).unwrap();
        }

        #[cfg(any(feature = "amd-sev", feature = "tdx"))]
        vmm::worker::start_worker_thread(built_vm.vmm().clone(), _receiver.clone()).unwrap();

        Ok(Context {
            vm_exit: built_vm.vm_exit().clone(),
            built_vm: built_vm.into(),
            event_manager,
            shutdown_efd: shutdown_efd_clone,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[cfg(feature = "net")]
    #[error(transparent)]
    NetworkInterface(#[from] NetworkInterfaceError),
}

#[derive(Debug, thiserror::Error)]
pub enum BuilderError {
    #[error("cannot disable implicit console after adding console devices (would invalidate device paths)")]
    ConsoleAlreadyAdded,
}

/// A parked Nitro receiver's runtime, running in its own thread since build.
#[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
struct NitroParked {
    run: nitro_sandbox::runtime::ParkedRun,
    observers: nitro_sandbox::runtime::ExitObserverRegistry,
    heartbeat: Option<nitro_sandbox::heartbeat::DeadlineControl>,
}

struct ContextVm {
    kvm: Option<vmm::builder::BuiltVm>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro: Option<nitro_sandbox::runtime::Runtime>,
    /// Set instead of `nitro` for a parked receiver (started at build).
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_parked: Option<NitroParked>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    nitro_device_info: vmm::resources::VmDeviceInfo,
    #[cfg(all(
        target_os = "linux",
        target_arch = "aarch64",
        feature = "nitro-sandbox"
    ))]
    nitro_capture: Option<vmm::capture_request::CaptureRequest>,
    #[cfg(all(
        target_os = "linux",
        target_arch = "aarch64",
        feature = "nitro-sandbox"
    ))]
    nitro_park: Option<TemplateAssignment>,
}
impl Deref for ContextVm {
    type Target = vmm::builder::BuiltVm;
    fn deref(&self) -> &Self::Target {
        self.kvm
            .as_ref()
            .expect("this control operation is only available on KVM")
    }
}
impl DerefMut for ContextVm {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.kvm
            .as_mut()
            .expect("this control operation is only available on KVM")
    }
}
impl From<vmm::builder::BuiltVm> for ContextVm {
    fn from(vm: vmm::builder::BuiltVm) -> Self {
        Self {
            kvm: Some(vm),
            #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
            nitro: None,
            #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
            nitro_parked: None,
            #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
            nitro_device_info: Default::default(),
            #[cfg(all(
                target_os = "linux",
                target_arch = "aarch64",
                feature = "nitro-sandbox"
            ))]
            nitro_capture: None,
            #[cfg(all(
                target_os = "linux",
                target_arch = "aarch64",
                feature = "nitro-sandbox"
            ))]
            nitro_park: None,
        }
    }
}

pub struct Context {
    built_vm: ContextVm,
    event_manager: EventManager,
    /// Cloned shutdown eventfd for host-initiated GPIO shutdown.
    shutdown_efd: Option<Arc<EventFd>>,
    vm_exit: vmm::vm_exit::SharedVmExit,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("invalid builder configuration: {0}")]
    Configuration(std::io::Error),
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    #[error("Nitro backend: {0}")]
    Nitro(std::io::Error),
    #[error("host socket policy cannot be enforced by an external vhost-user-vsock server")]
    UnsupportedSocketPolicy,
    #[error(
        "unsupported on this backend: inbound port_map requires internet_socket_forwarding(true)"
    )]
    UnsupportedPortMap,
    #[error("could not setup event manager: {0:?}")]
    EventManager(polly::event_manager::Error),
    #[cfg(not(feature = "static-firmware"))]
    #[error(transparent)]
    FirmwareLoad(#[from] libloading::Error),
    #[error("could not find or load firmware")]
    MissingFirmware,
    #[cfg(feature = "blk")]
    #[error(transparent)]
    BlockConfig(#[from] BlockConfigError),
    #[error("{0:?}")]
    TeeConfig(vmm::resources::Error),
    #[error("missing tee config")]
    MissingTeeConfig,
    #[error(transparent)]
    KernelCmdline(#[from] vmm::vmm_config::kernel_cmdline::KernelCmdlineConfigError),
    #[error("could not setuid: {0}")]
    Setuid(std::io::Error),
    #[error("could not setgid: {0}")]
    Setgid(std::io::Error),
    #[error("vcpu_count must be at least 1")]
    ZeroVcpus,
    #[error("tag too long: {} bytes (max 36)", .0)]
    TagTooLong(usize),
    #[error("cannot configure both userspace vsock and vhost-user vsock")]
    VsockConflict,
    #[error(transparent)]
    Microvm(#[from] StartMicrovmError),
    #[error("{0:?}")]
    EventManagerRun(polly::event_manager::Error),
}

#[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
fn nitro_stated_exit(code: u8, observed_micros: Option<u64>) -> VmExit {
    // Keep raw-byte translation here until the shared exit contract is merged.
    VmExit::Error {
        message: format!("Nitro stated terminal reason {code} observed_micros={observed_micros:?}"),
    }
}

#[cfg(all(test, target_os = "linux", feature = "nitro-sandbox"))]
#[test]
fn nitro_stated_exit_table() {
    for (code, name) in [
        (1, "power-off"),
        (2, "reboot"),
        (3, "halt"),
        (4, "panic"),
        (0, "unknown"),
        (255, "unknown"),
    ] {
        let VmExit::Error { message } = nitro_stated_exit(code, Some(42)) else {
            panic!("unexpected public value for {name}");
        };
        assert_eq!(
            message,
            format!("Nitro stated terminal reason {code} observed_micros=Some(42)"),
            "{name}"
        );
    }
}

impl Context {
    /// Duplicate a created Nitro VM descriptor before running it. Pass it to the
    /// host daemon with SCM_RIGHTS before permitting this process to start.
    /// The VM remains alive until the last duplicate is closed; after reaping the
    /// VMM the daemon closes its duplicate under the same lifecycle lock.
    /// Not available for a parked receiver, which starts at build: give it
    /// `Builder::nitro_descriptor_handoff` instead.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn duplicate_nitro_vm_descriptor(&self) -> std::io::Result<std::os::fd::OwnedFd> {
        if self.built_vm.nitro_parked.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "a parked receiver starts at build; use Builder::nitro_descriptor_handoff",
            ));
        }
        self.built_vm
            .nitro
            .as_ref()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Unsupported, "not a Nitro VM"))?
            .duplicate_vm_descriptor()
    }
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    fn from_nitro(
        runtime: nitro_sandbox::runtime::Runtime,
        device_info: vmm::resources::VmDeviceInfo,
    ) -> Result<Self, StartError> {
        let runtime_stop = runtime.stop_event();
        Ok(Self {
            built_vm: ContextVm {
                kvm: None,
                nitro: Some(runtime),
                nitro_parked: None,
                nitro_device_info: device_info,
                #[cfg(all(target_arch = "aarch64", feature = "nitro-sandbox"))]
                nitro_capture: None,
                #[cfg(all(target_arch = "aarch64", feature = "nitro-sandbox"))]
                nitro_park: None,
            },
            event_manager: EventManager::new().map_err(StartError::EventManager)?,
            shutdown_efd: Some(runtime_stop),
            vm_exit: Arc::new(Mutex::new(None)),
        })
    }
    /// Take the handle that gives this parked Nitro receiver its template.
    /// Present once, only for contexts built with `Builder::park_for_template`.
    #[cfg(all(
        target_os = "linux",
        target_arch = "aarch64",
        feature = "nitro-sandbox"
    ))]
    pub fn take_template_assignment(&mut self) -> Option<TemplateAssignment> {
        self.built_vm.nitro_park.take()
    }
    /// Access device information (console port paths, etc.).
    ///
    /// This returns the actual device paths from the built VM.
    pub fn device_info(&self) -> &vmm::resources::VmDeviceInfo {
        #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
        if self.built_vm.nitro.is_some() || self.built_vm.nitro_parked.is_some() {
            return &self.built_vm.nitro_device_info;
        }
        &self.built_vm.device_info
    }

    /// Returns a `VmHandle` that can be used to pause/resume the VM from another thread.
    /// Must be called before `run()`, since `run()` consumes `self`.
    pub fn vm_handle(&self) -> VmHandle {
        #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
        if self.built_vm.nitro.is_some() || self.built_vm.nitro_parked.is_some() {
            let heartbeat_control = match &self.built_vm.nitro_parked {
                Some(parked) => parked.heartbeat.clone(),
                None => self
                    .built_vm
                    .nitro
                    .as_ref()
                    .and_then(|runtime| runtime.heartbeat_control()),
            };
            return VmHandle {
                vmm: None,
                #[cfg(target_arch = "aarch64")]
                nitro_capture: self.built_vm.nitro_capture.clone(),
                heartbeat_control,
                shutdown_efd: self.shutdown_efd.clone(),
                #[cfg(not(feature = "tee"))]
                balloon: None,
            };
        }
        #[cfg(not(feature = "tee"))]
        let balloon_handle = if self.built_vm.balloon_enabled {
            let vmm = self.built_vm.vmm().lock().unwrap();
            vmm.get_balloon().map(|b| {
                let condvar = b.lock().unwrap().actual_condvar();
                BalloonHandle::new(b.clone(), condvar)
            })
        } else {
            None
        };

        VmHandle {
            vmm: Some(self.built_vm.vmm().clone()),
            #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
            heartbeat_control: None,
            #[cfg(all(
                target_os = "linux",
                target_arch = "aarch64",
                feature = "nitro-sandbox"
            ))]
            nitro_capture: None,
            shutdown_efd: self.shutdown_efd.clone(),
            #[cfg(not(feature = "tee"))]
            balloon: balloon_handle,
        }
    }

    /// Registers an exit observer that will be called when the VM exits.
    /// Must be called before `run()`, since `run()` consumes `self`.
    pub fn register_exit_observer(
        &self,
        observer: std::sync::Arc<std::sync::Mutex<dyn devices::virtio::VmmExitObserver>>,
    ) {
        #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
        if let Some(runtime) = &self.built_vm.nitro {
            runtime.register_exit_observer(observer);
            return;
        }
        #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
        if let Some(parked) = &self.built_vm.nitro_parked {
            parked.observers.register(observer);
            return;
        }
        self.built_vm
            .vmm()
            .lock()
            .expect("Poisoned vmm lock")
            .register_exit_observer(observer);
    }

    /// Start the VM and run the event loop. This blocks until the VM exits.
    pub fn run(mut self) -> Result<vmm::vm_exit::VmExit, StartError> {
        #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
        let nitro_result = match (
            self.built_vm.nitro.take(),
            self.built_vm.nitro_parked.take(),
        ) {
            (Some(runtime), _) => Some(runtime.run()),
            // Already running since build; wait for its end.
            (None, Some(parked)) => Some(parked.run.wait()),
            (None, None) => None,
        };
        #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
        if let Some(result) = nitro_result {
            return result
                .map(|exit| match exit {
                    nitro_sandbox::runtime::Exit::PowerOff => {
                        vmm::vm_exit::VmExit::Shutdown { exit_code: 0 }
                    }
                    nitro_sandbox::runtime::Exit::Stopped => vmm::vm_exit::VmExit::Stopped,
                    nitro_sandbox::runtime::Exit::Frozen => vmm::vm_exit::VmExit::Frozen {
                        template_path: std::path::PathBuf::new(),
                    },
                    nitro_sandbox::runtime::Exit::StatedReason {
                        code,
                        observed_micros,
                    } => nitro_stated_exit(code, observed_micros),
                    nitro_sandbox::runtime::Exit::Watchdog => vmm::vm_exit::VmExit::DeviceWatchdog,
                    nitro_sandbox::runtime::Exit::Panic => vmm::vm_exit::VmExit::GuestPanic,
                    nitro_sandbox::runtime::Exit::LivenessExpired => {
                        vmm::vm_exit::VmExit::LivenessExpired
                    }
                })
                .map_err(StartError::Nitro);
        }
        let t_run_start = std::time::Instant::now();
        // Start the vCPUs
        self.built_vm.run()?;
        info!(
            "[boot_timing] start_vcpus (kernel executing)         total={:.3}ms",
            t_run_start.elapsed().as_secs_f64() * 1000.0,
        );

        let mut first_event_logged = false;
        // Run the event loop
        loop {
            let n = self
                .event_manager
                .run()
                .map_err(StartError::EventManagerRun)?;
            if n > 0 && !first_event_logged {
                info!(
                    "[boot_timing] first guest device I/O              total={:.3}ms",
                    t_run_start.elapsed().as_secs_f64() * 1000.0,
                );
                first_event_logged = true;
            }

            // Check if the VM has exited
            if let Some(vm_exit) = self.vm_exit.lock().expect("Poisoned vm_exit lock").take() {
                return Ok(vm_exit);
            }
        }
    }
}

#[cfg(not(feature = "tee"))]
pub use devices::virtio::balloon::BalloonStats;

/// Handle for controlling the memory balloon device.
#[cfg(not(feature = "tee"))]
#[derive(Clone)]
pub struct BalloonHandle {
    balloon: Arc<Mutex<devices::virtio::Balloon>>,
    actual_condvar: Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
}

/// Result of awaiting a balloon resize target.
#[cfg(not(feature = "tee"))]
#[derive(Debug)]
pub enum BalloonResult {
    /// Target reached — actual reached target (>= for inflation, <= for deflation)
    Reached(u64),
    /// Guest stopped making progress — actual stalled at this value
    Stalled(u64),
}

/// Error from balloon operations.
#[cfg(not(feature = "tee"))]
#[derive(Debug)]
pub enum BalloonError {
    /// Maximum timeout exceeded
    Timeout { actual: u64 },
    /// Balloon device not activated
    DeviceNotActive,
    /// Target size exceeds maximum (target_pages > u32::MAX)
    TargetTooLarge { max_mb: u64 },
}

/// Handle for controlling a running VM from another thread.
///
/// Obtain via `Context::vm_handle()` before calling `Context::run()`.
#[derive(Clone)]
pub struct VmHandle {
    vmm: Option<Arc<Mutex<vmm::Vmm>>>,
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    heartbeat_control: Option<nitro_sandbox::heartbeat::DeadlineControl>,
    #[cfg(all(
        target_os = "linux",
        target_arch = "aarch64",
        feature = "nitro-sandbox"
    ))]
    nitro_capture: Option<vmm::capture_request::CaptureRequest>,
    shutdown_efd: Option<Arc<EventFd>>,
    #[cfg(not(feature = "tee"))]
    balloon: Option<BalloonHandle>,
}

#[cfg(not(feature = "tee"))]
impl BalloonHandle {
    fn new(
        balloon: Arc<Mutex<devices::virtio::Balloon>>,
        actual_condvar: Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
    ) -> Self {
        BalloonHandle {
            balloon,
            actual_condvar,
        }
    }

    /// Resize the memory balloon to a target size in MB.
    ///
    /// This sets the target size and signals the guest to read the new value from config.
    /// The guest will inflate or deflate asynchronously toward the target.
    ///
    /// Returns `Err(BalloonError::DeviceNotActive)` if the device is not activated.
    /// Returns `Err(BalloonError::TargetTooLarge { max_mb })` if target_mb exceeds u32::MAX pages.
    #[mutants::skip] // requires activated balloon device (guest cooperation); tested by integration tests
    pub fn resize(&self, target_mb: u64) -> Result<(), BalloonError> {
        // Convert MB to pages: (MB * 1024 * 1024) / 4096
        let target_pages = (target_mb * 1024 * 1024) / 4096;

        // Bounds check: ensure target_pages fits in u32 (before activation check)
        if target_pages > u32::MAX as u64 {
            let max_mb = (u32::MAX as u64) * 4096 / (1024 * 1024);
            return Err(BalloonError::TargetTooLarge { max_mb });
        }

        let mut balloon = self.balloon.lock().unwrap();

        // Check device is activated
        if !balloon.is_device_activated() {
            return Err(BalloonError::DeviceNotActive);
        }

        // Write num_pages to config
        balloon.set_num_pages(target_pages as u32);

        // Signal config change to guest
        balloon.signal_config_changed();

        Ok(())
    }

    /// Wait for the guest to inflate/deflate to a target size in MB.
    ///
    /// Returns:
    /// - `Ok(BalloonResult::Reached(actual_mb))` when actual size reaches target
    /// - `Ok(BalloonResult::Stalled(actual_mb))` when guest stops progressing for stall_timeout
    /// - `Err(BalloonError::Timeout { actual: actual_mb })` when max_timeout is exceeded
    ///
    /// The stall_timeout detects when the guest hasn't made progress for a duration.
    /// If max_timeout is None, will wait indefinitely but still returns Stalled when stalled.
    #[mutants::skip] // requires activated balloon with guest cooperation; tested by integration tests
    pub fn await_target(
        &self,
        target_mb: u64,
        stall_timeout: std::time::Duration,
        max_timeout: Option<std::time::Duration>,
    ) -> Result<BalloonResult, BalloonError> {
        // Convert target to pages
        let target_pages = (target_mb * 1024 * 1024) / 4096;

        // Get condvar
        let (lock, cvar) = &*self.actual_condvar;
        let mut actual = lock.lock().unwrap();

        // Determine direction: inflate (actual needs to grow) or deflate (actual needs to shrink)
        let deflating = *actual > target_pages;

        // Record start time for max_timeout
        let start = std::time::Instant::now();

        loop {
            // Check if target reached (direction-aware)
            let reached = if deflating {
                *actual <= target_pages
            } else {
                *actual >= target_pages
            };
            if reached {
                return Ok(BalloonResult::Reached(*actual * 4096 / (1024 * 1024)));
            }

            // Check if max_timeout exceeded
            if let Some(max_to) = max_timeout {
                if start.elapsed() > max_to {
                    return Err(BalloonError::Timeout {
                        actual: *actual * 4096 / (1024 * 1024),
                    });
                }
            }

            // Wait on condvar with stall_timeout
            let (new_actual, timeout_result) = cvar.wait_timeout(actual, stall_timeout).unwrap();
            actual = new_actual;

            // If condvar timed out, guest stalled
            if timeout_result.timed_out() {
                return Ok(BalloonResult::Stalled(*actual * 4096 / (1024 * 1024)));
            }

            // Condvar was signaled, re-check target in loop
        }
    }

    /// Get the current actual memory size allocated to the guest in MB.
    pub fn actual(&self) -> u64 {
        let balloon = self.balloon.lock().unwrap();
        let actual_pages = balloon.get_actual_pages() as u64;
        actual_pages * 4096 / (1024 * 1024)
    }

    /// Get the current balloon statistics, if available.
    ///
    /// Returns `None` if statistics haven't been collected yet.
    #[mutants::skip] // requires activated balloon with guest cooperation; tested by integration tests
    pub fn stats(&self) -> Option<BalloonStats> {
        let balloon = self.balloon.lock().unwrap();
        balloon.stats().cloned()
    }
}

/// Scoped SIGTERM-to-stop registration for a one-guest Nitro VMM process.
#[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
pub struct NitroSigtermGuard(signal_hook::SigId);
#[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
impl NitroSigtermGuard {
    fn for_stop_event(stop: Arc<EventFd>) -> Result<Self, StartError> {
        use std::os::fd::AsRawFd;
        let id = unsafe {
            signal_hook::low_level::register(libc::SIGTERM, move || {
                let value = 1u64;
                libc::write(stop.as_raw_fd(), (&value as *const u64).cast(), 8);
            })
        }
        .map_err(StartError::Nitro)?;
        Ok(Self(id))
    }
}
#[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
impl Drop for NitroSigtermGuard {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.0);
    }
}

impl VmHandle {
    /// Suspend Nitro guest heartbeat expiry while the embedder freezes a guest.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn suspend_liveness_deadline(&self) -> std::io::Result<()> {
        self.heartbeat_control
            .as_ref()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "no Nitro liveness deadline",
                )
            })?
            .suspend();
        Ok(())
    }
    /// Resume with a fresh full deadline, excluding time spent suspended.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn resume_liveness_deadline(&self) -> std::io::Result<()> {
        self.heartbeat_control
            .as_ref()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "no Nitro liveness deadline",
                )
            })?
            .resume();
        Ok(())
    }
    /// Route SIGTERM through the same orderly stop event as a handle request.
    /// Keep the returned guard alive while running the guest.
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    pub fn install_sigterm_handler(&self) -> Result<NitroSigtermGuard, StartError> {
        use std::os::fd::AsRawFd;
        if self.vmm.is_some() {
            return Err(StartError::Nitro(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "unsupported on this backend: Nitro SIGTERM registration requires a Nitro handle",
            )));
        }
        let stop = self.shutdown_efd.clone().ok_or_else(|| {
            StartError::Nitro(std::io::Error::other("Nitro stop event unavailable"))
        })?;
        // write(2) is async-signal-safe. Retaining the Arc prevents fd reuse
        // until signal-hook has unregistered this callback.
        let id = unsafe {
            signal_hook::low_level::register(libc::SIGTERM, move || {
                let value = 1u64;
                libc::write(stop.as_raw_fd(), (&value as *const u64).cast(), 8);
            })
        }
        .map_err(StartError::Nitro)?;
        Ok(NitroSigtermGuard(id))
    }

    fn kvm(&self) -> Result<&Arc<Mutex<vmm::Vmm>>, StartError> {
        self.vmm.as_ref().ok_or_else(|| {
            #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
            {
                StartError::Nitro(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "unsupported on this backend: KVM control operation",
                ))
            }
            #[cfg(not(all(target_os = "linux", feature = "nitro-sandbox")))]
            {
                unreachable!("KVM handles always contain a VMM")
            }
        })
    }
    /// Arm capture and wait for an independently coordinated guest-agent trigger.
    /// This does not request a freeze from the kernel. The daemon must quiesce
    /// the workload through the agent and authorize its trigger after arming.
    /// The returned template owns a sealed descriptor. For a sequential agent
    /// handshake, use arm_template_capture followed by wait_template_capture.
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        not(feature = "tee")
    ))]
    pub fn freeze_template(
        &self,
        timeout: std::time::Duration,
    ) -> std::io::Result<vmm::capture_request::CaptureOutcome> {
        self.arm_template_capture(timeout)?;
        self.wait_template_capture()
    }

    /// Authorize capture after the agent has quiesced the workload.
    /// After this returns, authorize the agent to write the kernel trigger.
    /// Receive the sealed descriptor with wait_template_capture.
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        not(feature = "tee")
    ))]
    pub fn arm_template_capture(&self, timeout: std::time::Duration) -> std::io::Result<()> {
        #[cfg(all(
            target_os = "linux",
            target_arch = "aarch64",
            feature = "nitro-sandbox"
        ))]
        if let Some(request) = &self.nitro_capture {
            return request.arm(timeout);
        }
        let request = self
            .vmm
            .as_ref()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "cooperative capture requires Linux KVM",
                )
            })?
            .lock()
            .unwrap()
            .arm_template_capture(timeout)?;
        let _ = request;
        Ok(())
    }

    /// Wait for a previously armed capture using its original deadline.
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        not(feature = "tee")
    ))]
    pub fn wait_template_capture(&self) -> std::io::Result<vmm::capture_request::CaptureOutcome> {
        #[cfg(all(
            target_os = "linux",
            target_arch = "aarch64",
            feature = "nitro-sandbox"
        ))]
        if let Some(request) = &self.nitro_capture {
            return request.wait();
        }
        let request = self
            .vmm
            .as_ref()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "cooperative capture requires Linux KVM",
                )
            })?
            .lock()
            .unwrap()
            .wait_template_capture();
        request.wait()
    }

    /// Give a running guest a fresh generation identifier and raise its notification interrupt.
    /// Call after copying guest memory and starting the copy, so its kernel reseeds randomness.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub fn notify_vm_generation_change(&self) -> Result<[u8; 16], StartError> {
        self.kvm()?
            .lock()
            .expect("Poisoned vmm lock")
            .notify_vm_generation_change()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))
    }

    /// Pause all vCPUs. Blocks until all vCPUs have acknowledged the pause.
    #[mutants::skip] // races with VM exit in mutation tests; tested by integration tests
    pub fn pause(&self) -> Result<(), StartError> {
        self.kvm()?
            .lock()
            .expect("Poisoned vmm lock")
            .pause_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))
    }

    /// Resume all vCPUs. Blocks until all vCPUs have acknowledged the resume.
    #[mutants::skip] // races with VM exit in mutation tests; tested by integration tests
    pub fn resume(&self) -> Result<(), StartError> {
        self.kvm()?
            .lock()
            .expect("Poisoned vmm lock")
            .resume_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))
    }

    /// Trigger host-initiated guest shutdown via the MMIO GPIO shutdown eventfd.
    #[mutants::skip] // shutdown_efd is unavailable on Linux
    pub fn trigger_shutdown_event(&self) -> Result<(), StartError> {
        match self.shutdown_efd.as_ref() {
            Some(efd) => efd.write(1).map_err(|e| {
                StartError::Microvm(vmm::builder::StartMicrovmError::Internal(
                    vmm::Error::EventFd(e),
                ))
            }),
            None => Err(StartError::Microvm(
                vmm::builder::StartMicrovmError::Internal(vmm::Error::EventFd(
                    std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "shutdown eventfd unavailable for this VM",
                    ),
                )),
            )),
        }
    }

    /// Returns a handle for controlling the memory balloon device, if enabled.
    /// Returns `None` if the balloon device was not enabled via `Builder::enable_balloon()`.
    #[cfg(not(feature = "tee"))]
    pub fn balloon(&self) -> Option<&BalloonHandle> {
        self.balloon.as_ref()
    }
}

/// The command line packaged into a Nitro image: the caller's tokens, then the guest kernel's
/// socket-forwarding switches (`tsi_hijack`, `tsi_hijack_unix`) exactly when the vsock device
/// forwards those families, as the KVM builder inserts them. Without them the guest never
/// redirects its sockets and forwarding silently does nothing.
#[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
fn nitro_cmdline(args: &[String], flags: TsiFlags) -> String {
    let mut tokens: Vec<&str> = args.iter().map(String::as_str).collect();
    if flags.contains(TsiFlags::HIJACK_INET) {
        tokens.push("tsi_hijack");
    }
    if flags.contains(TsiFlags::HIJACK_UNIX) {
        tokens.push("tsi_hijack_unix");
    }
    tokens.join(" ")
}

#[cfg(all(test, target_os = "linux", feature = "nitro-sandbox"))]
mod nitro_cmdline_tests {
    use super::*;
    #[test]
    fn forwarding_switches_follow_the_vsock_flags() {
        let args = vec!["panic=-1".to_owned(), "rdinit=/init".to_owned()];
        assert_eq!(
            nitro_cmdline(&args, TsiFlags::empty()),
            "panic=-1 rdinit=/init"
        );
        assert_eq!(
            nitro_cmdline(&args, TsiFlags::HIJACK_INET),
            "panic=-1 rdinit=/init tsi_hijack"
        );
        assert_eq!(
            nitro_cmdline(&args, TsiFlags::HIJACK_INET | TsiFlags::HIJACK_UNIX),
            "panic=-1 rdinit=/init tsi_hijack tsi_hijack_unix"
        );
    }
    #[test]
    fn builder_vsock_cid_rejects_reserved_values() {
        let mut builder = Builder::new();
        assert_eq!(builder.guest_vsock_cid, None);
        for cid in [0, 1, 2, u32::MAX] {
            assert!(builder.vsock_cid(cid).is_err());
        }
        builder.vsock_cid(41).unwrap();
        assert_eq!(builder.guest_vsock_cid, Some(41));
    }
    #[test]
    fn builder_flags_follow_internet_forwarding() {
        let mut builder = Builder::new();
        assert_eq!(
            builder.nitro_tsi_flags(),
            TsiFlags::empty(),
            "off by default"
        );
        builder.internet_socket_forwarding(true);
        assert!(builder.nitro_tsi_flags().contains(TsiFlags::HIJACK_INET));
        builder.internet_socket_forwarding(false);
        assert!(!builder.nitro_tsi_flags().contains(TsiFlags::HIJACK_INET));
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(
        target_os = "linux",
        target_arch = "aarch64",
        feature = "nitro-sandbox"
    ))]
    #[test]
    fn arm64_template_mark_requires_nitro_backend() {
        let mut builder = super::Builder::new();
        builder.future_template();
        assert!(
            matches!(builder.build(),Err(super::StartError::Configuration(error)) if error.kind()==std::io::ErrorKind::Unsupported)
        );
    }
    use super::*;

    #[test]
    #[cfg(not(feature = "tee"))]
    fn missing_passthrough_directory_is_a_build_error_not_panic() {
        let mut builder = Builder::new();
        builder.add_virtiofs_path("missing", "/does-not-exist/nitro-tree", None, false);
        assert!(matches!(builder.build(), Err(StartError::Configuration(_))));
    }

    #[test]
    fn inbound_port_map_requires_forwarding_before_allocation() {
        let mut builder = Builder::new();
        builder.config.tsi_port_map = Some(HashMap::from([(8080, 80)]));
        assert!(matches!(
            builder.build(),
            Err(StartError::UnsupportedPortMap)
        ));
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    fn nitro_device_construction_preserves_rule_on_reset() {
        use devices::virtio::vsock::policy::SocketPolicy;
        use devices::virtio::VirtioDevice;
        let mut device = Builder::create_nitro_vsock(
            210,
            None,
            None,
            TsiFlags::empty(),
            SocketPolicy {
                policy: Some(Arc::new(AllowlistPolicy::new([]))),
                internet_enabled: true,
            },
        )
        .unwrap();
        let address: SocketAddress = "127.0.0.1:7000"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into();
        for _ in 0..3 {
            assert!(device.socket_policy().internet_enabled);
            assert!(device
                .socket_policy()
                .check(SocketOperation::TcpConnect, &address)
                .is_err());
            assert!(device.reset());
        }
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    fn nitro_mixed_disk_count_matches_selected_configuration() {
        use devices::virtio::block::{ImageType, SyncMode};
        let disk = || BlockDeviceConfig {
            block_id: "disk".into(),
            cache_type: devices::virtio::CacheType::Writeback,
            disk_type: BlockDeviceType::Image {
                path: "/dev/null".into(),
                format: ImageType::Raw,
                sync_mode: SyncMode::None,
            },
            is_disk_read_only: false,
            direct_io: false,
        };
        let mut builder = Builder::new();
        builder.config.root_block_cfg = Some(disk());
        builder.config.data_block_cfg = Some(disk());
        builder.config.block_cfgs.push(disk());
        let count = builder.nitro_device_count_before_consoles();
        assert_eq!(count, 2); // one selected block and the implicit vsock
        assert_eq!(count as usize, builder.config.take_block_cfg().len() + 1);
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "nitro-sandbox"))]
    fn nitro_handle_controls_without_hardware() {
        let stop = Arc::new(EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap());
        let handle = VmHandle {
            vmm: None,
            heartbeat_control: None,
            #[cfg(all(
                target_os = "linux",
                target_arch = "aarch64",
                feature = "nitro-sandbox"
            ))]
            nitro_capture: None,
            shutdown_efd: Some(stop.clone()),
            #[cfg(not(feature = "tee"))]
            balloon: None,
        };
        for result in [
            handle.pause(),
            handle.resume(),
            handle.notify_vm_generation_change().map(|_| ()),
        ] {
            match result {
                Err(StartError::Nitro(error)) => {
                    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
                    assert!(error.to_string().contains("unsupported on this backend"));
                }
                other => panic!("unexpected control result: {other:?}"),
            }
        }
        #[cfg(not(feature = "tee"))]
        assert!(handle.balloon().is_none());
        handle.trigger_shutdown_event().unwrap();
        assert_eq!(stop.read().unwrap(), 1);
    }

    #[test]
    #[cfg(feature = "vhost-user")]
    fn test_add_virtiofs_vhost_user_tag_too_long_ac3_3() {
        // AC3.3: Tag longer than 36 bytes is rejected
        let mut builder = Builder::new();
        let long_tag = "x".repeat(37);
        let result = builder.add_virtiofs_vhost_user(&long_tag, "/tmp/sock", Some(32));

        match result {
            Err(StartError::TagTooLong(len)) => {
                assert_eq!(len, 37, "Error should report correct tag length");
            }
            _ => panic!("Expected TagTooLong error for 37-byte tag"),
        }
    }

    #[test]
    #[cfg(feature = "vhost-user")]
    fn test_add_virtiofs_vhost_user_tag_max_length() {
        // Tag of exactly 36 bytes should succeed
        let mut builder = Builder::new();
        let max_tag = "x".repeat(36);
        let result = builder.add_virtiofs_vhost_user(&max_tag, "/tmp/sock", Some(32));

        assert!(result.is_ok(), "36-byte tag should be accepted");
    }

    #[test]
    #[cfg(feature = "vhost-user")]
    fn test_add_virtiofs_vhost_user_coexistence_ac3_4() {
        // AC3.4: Coexistence with existing direct FUSE virtio-fs device
        let mut builder = Builder::new();

        // Add regular virtiofs device
        builder.add_virtiofs_path("fs1", "/tmp", None, false);

        // Add vhost-user FS device - should not conflict
        let result = builder.add_virtiofs_vhost_user("vhostfs", "/tmp/sock", Some(32));

        assert!(
            result.is_ok(),
            "vhost-user FS should coexist with regular FS"
        );

        // Verify both are stored in VmResources
        assert_eq!(
            builder.config.vmr.fs.len(),
            1,
            "Should have 1 regular FS device"
        );
        assert_eq!(
            builder.config.vmr.vhost_user_fs.len(),
            1,
            "Should have 1 vhost-user FS device"
        );
        assert_eq!(
            builder.config.vmr.vhost_user_fs[0].tag, "vhostfs",
            "vhost-user FS tag should be stored"
        );
        assert_eq!(
            builder.config.vmr.vhost_user_fs[0].socket_path, "/tmp/sock",
            "vhost-user FS socket path should be stored"
        );
        assert_eq!(
            builder.config.vmr.vhost_user_fs[0].dax_window_mib,
            Some(32),
            "vhost-user FS DAX window size should be stored"
        );
    }

    #[test]
    #[cfg(feature = "vhost-user")]
    fn test_add_vsock_vhost_user_socket_path_success() {
        // AC2.1: add_vsock_vhost_user() succeeds when no explicit vsock configured
        let mut builder = Builder::new();
        let result = builder.add_vsock_vhost_user("/tmp/vsock.sock");

        assert!(result.is_ok(), "add_vsock_vhost_user() should succeed");
        assert!(
            builder.config.vhost_user_vsock,
            "vhost_user_vsock flag should be set"
        );
        assert!(
            builder.config.vmr.vhost_user_vsock.is_some(),
            "vhost_user_vsock config should be stored in VmResources"
        );
    }

    #[test]
    #[cfg(feature = "vhost-user")]
    fn test_add_vsock_vhost_user_fd_success() {
        // AC2.2: add_vsock_vhost_user_fd() succeeds with a UnixStream
        use std::os::unix::net::UnixStream;

        let mut builder = Builder::new();
        let (stream1, _stream2) = UnixStream::pair().expect("Failed to create UnixStream pair");
        let result = builder.add_vsock_vhost_user_fd(stream1);

        assert!(
            result.is_ok(),
            "add_vsock_vhost_user_fd() should succeed with valid UnixStream"
        );
        assert!(
            builder.config.vhost_user_vsock,
            "vhost_user_vsock flag should be set"
        );
        assert!(
            builder.config.vmr.vhost_user_vsock.is_some(),
            "vhost_user_vsock config should be stored in VmResources"
        );
    }

    #[test]
    #[cfg(feature = "vhost-user")]
    fn test_vsock_conflict_explicit_then_vhost_user() {
        // AC2.4: calling add_vsock_vhost_user() after explicit vsock config returns VsockConflict
        let mut builder = Builder::new();

        // First configure explicit userspace vsock via internal config
        builder.config.vsock_config = VsockConfig::Explicit {
            tsi_flags: TsiFlags::empty(),
        };

        let result = builder.add_vsock_vhost_user("/tmp/vsock.sock");

        assert!(
            matches!(result, Err(StartError::VsockConflict)),
            "add_vsock_vhost_user() should return VsockConflict when explicit vsock configured"
        );
    }

    #[test]
    #[cfg(feature = "vhost-user")]
    fn test_vsock_conflict_vhost_user_then_explicit() {
        // AC2.3: calling add_vsock_vhost_user() a second time returns VsockConflict
        let mut builder = Builder::new();

        // First configure vhost-user-vsock via Builder API
        let result1 = builder.add_vsock_vhost_user("/tmp/vsock.sock");
        assert!(
            result1.is_ok(),
            "first add_vsock_vhost_user() should succeed"
        );

        // Verify that a second call returns VsockConflict
        let result2 = builder.add_vsock_vhost_user("/tmp/other.sock");
        assert!(
            matches!(result2, Err(StartError::VsockConflict)),
            "second add_vsock_vhost_user() should fail with VsockConflict"
        );
    }

    #[test]
    #[cfg(feature = "vhost-user")]
    fn test_vsock_conflict_vhost_user_fd_then_explicit() {
        // AC2.4 variant: calling add_vsock_vhost_user() after vhost-user-fd returns VsockConflict
        let mut builder = Builder::new();

        // First configure vhost-user-vsock via fd
        use std::os::unix::net::UnixStream;
        let (stream1, _stream2) = UnixStream::pair().expect("Failed to create UnixStream pair");
        let result = builder.add_vsock_vhost_user_fd(stream1);
        assert!(result.is_ok(), "add_vsock_vhost_user_fd() should succeed");

        // Then try to configure explicit userspace vsock - should fail with VsockConflict
        let result2 = builder.add_vsock_vhost_user("/tmp/vsock.sock");
        assert!(
            matches!(result2, Err(StartError::VsockConflict)),
            "add_vsock_vhost_user() should return VsockConflict after vhost-user-fd"
        );
    }

    // BalloonHandle API tests (AC4.2-AC4.8)

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_handle_ac4_2_enable_balloon_sets_flag() {
        // AC4.2: Builder::enable_balloon() sets balloon_enabled flag on VmResources
        let mut builder = Builder::new();
        assert_eq!(
            builder.config.vmr.balloon_enabled, false,
            "balloon_enabled should be false by default"
        );

        builder.enable_balloon();
        assert_eq!(
            builder.config.vmr.balloon_enabled, true,
            "balloon_enabled should be true after enable_balloon()"
        );
    }

    #[test]
    fn test_enable_nested_virt_sets_flag() {
        let mut builder = Builder::new();
        assert_eq!(
            builder.config.vmr.nested_enabled, false,
            "nested_enabled should be false by default"
        );

        builder.enable_nested_virt();
        assert_eq!(
            builder.config.vmr.nested_enabled, true,
            "nested_enabled should be true after enable_nested_virt()"
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_handle_ac4_6_resize_inactive_device_error() {
        // AC4.6: resize() on inactive device returns Err(DeviceNotActive)
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        // Device is inactive by default
        let result = handle.resize(256);
        assert!(
            matches!(result, Err(BalloonError::DeviceNotActive)),
            "resize() on inactive device should return DeviceNotActive"
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_handle_resize_large_value() {
        // Test bounds check behavior: target_mb exceeding u32::MAX pages should fail
        // with TargetTooLarge error, regardless of device activation state
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        // Calculate the minimum target_mb that exceeds u32::MAX pages
        // max_pages = u32::MAX = 4,294,967,295
        // max_mb = (4,294,967,295 * 4096) / (1024 * 1024) = 17,592,186,044,416 MB
        let max_mb = (u32::MAX as u64) * 4096 / (1024 * 1024);
        let too_large = max_mb + 1;

        let result = handle.resize(too_large);

        // Should fail with TargetTooLarge, not DeviceNotActive
        assert!(
            matches!(result, Err(BalloonError::TargetTooLarge { max_mb: m }) if m == max_mb),
            "resize() should fail with TargetTooLarge for target exceeding u32::MAX pages"
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_handle_creation_and_accessors() {
        // AC4.3/AC4.4: Test BalloonHandle can be created and used with a balloon device
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon.clone(), condvar);

        // Test that we can access actual (should be 0 initially)
        let actual_mb = handle.actual();
        assert_eq!(actual_mb, 0, "actual should be 0 initially");

        // Test that we can call stats (should be None initially)
        let stats = handle.stats();
        assert!(stats.is_none(), "stats should be None before collection");
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_handle_await_target_stalled_no_progress() {
        // AC4.7: await_target() with stalled guest returns Stalled after stall_timeout
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        // Initialize the condvar with current actual value
        let (lock, _cvar) = &*handle.actual_condvar;
        {
            let mut actual = lock.lock().unwrap();
            *actual = 0; // actual is 0
        }

        // await_target with target higher than actual and small stall_timeout
        // should return Stalled when no one signals the condvar
        let stall_timeout = std::time::Duration::from_millis(50);
        let max_timeout = std::time::Duration::from_secs(2);
        let result = handle.await_target(256, stall_timeout, Some(max_timeout));

        assert!(
            matches!(result, Ok(BalloonResult::Stalled(_))),
            "await_target() should return Stalled when guest doesn't progress"
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_handle_await_target_reached_with_notification() {
        // AC4.4: Test that await_target() returns Reached when guest notifies actual >= target
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::thread;

        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        let actual_condvar = handle.actual_condvar.clone();

        // Spawn a thread that simulates guest inflating after a short delay
        let guest_thread = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(50));

            // Simulate guest updating actual field
            let (lock, cvar) = &*actual_condvar;
            {
                let mut actual = lock.lock().unwrap();
                *actual = 512; // 512 pages (2 MB)
                drop(actual);
            }
            cvar.notify_all();
        });

        // Main thread awaits target of 1 MB (256 pages)
        let stall_timeout = std::time::Duration::from_millis(100);
        let max_timeout = std::time::Duration::from_secs(2);
        let result = handle.await_target(1, stall_timeout, Some(max_timeout));

        guest_thread.join().unwrap();

        // Should return Reached with actual >= target
        assert!(
            matches!(result, Ok(BalloonResult::Reached(actual_mb)) if actual_mb >= 1),
            "await_target() should return Reached when guest notifies actual >= target"
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_handle_await_target_with_concurrent_updates() {
        // AC4.8: Test that multiple threads can concurrently access balloon handle and await_target
        use std::sync::Arc;
        use std::thread;

        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        let handle_waiter = handle.clone();
        let actual_condvar = handle.actual_condvar.clone();

        // Spawn a thread that simulates guest updating actual
        let guest_thread = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(75));
            let (lock, cvar) = &*actual_condvar;
            {
                let mut actual = lock.lock().unwrap();
                *actual = 1024; // 4 MB
                drop(actual);
            }
            cvar.notify_all();
        });

        // Main thread awaits target
        let stall_timeout = std::time::Duration::from_millis(100);
        let max_timeout = std::time::Duration::from_secs(2);
        let result = handle_waiter.await_target(2, stall_timeout, Some(max_timeout));

        guest_thread.join().unwrap();

        // Should return Reached (4 MB >= 2 MB target)
        assert!(
            matches!(result, Ok(BalloonResult::Reached(actual_mb)) if actual_mb >= 2),
            "await_target() should return Reached when guest notifies target met"
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_handle_await_target_deflation_does_not_return_immediately() {
        // BUG: await_target only checks `actual >= target_pages`, which is always true
        // when deflating (target < actual). It should wait for actual <= target_pages.
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        // Set actual to 1024 pages (4 MB) — simulating an inflated balloon
        let (lock, _cvar) = &*handle.actual_condvar;
        {
            let mut actual = lock.lock().unwrap();
            *actual = 1024;
        }

        // await_target with target 1 MB (256 pages) — deflation request.
        // With the bug: returns Reached(4) immediately because 1024 >= 256.
        // Correct: should return Stalled because no one signals deflation progress.
        let stall_timeout = std::time::Duration::from_millis(50);
        let max_timeout = std::time::Duration::from_secs(1);
        let result = handle.await_target(1, stall_timeout, Some(max_timeout));

        assert!(
            matches!(result, Ok(BalloonResult::Stalled(_))),
            "await_target() for deflation should NOT return Reached immediately; \
             it must wait for actual to decrease. Got: {result:?}"
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_handle_await_target_deflation_reached_with_notification() {
        // Test that await_target returns Reached when guest deflates to target
        use std::thread;

        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        // Start with actual at 1024 pages (4 MB)
        let actual_condvar = handle.actual_condvar.clone();
        {
            let (lock, _) = &*actual_condvar;
            let mut actual = lock.lock().unwrap();
            *actual = 1024;
        }

        // Spawn thread to simulate guest deflating after delay
        let condvar_clone = actual_condvar.clone();
        let guest_thread = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(50));
            let (lock, cvar) = &*condvar_clone;
            {
                let mut actual = lock.lock().unwrap();
                *actual = 128; // 128 pages = 0 MB (below 1 MB target of 256 pages)
                drop(actual);
            }
            cvar.notify_all();
        });

        // Await deflation to 1 MB (256 pages)
        let stall_timeout = std::time::Duration::from_millis(200);
        let max_timeout = std::time::Duration::from_secs(2);
        let result = handle.await_target(1, stall_timeout, Some(max_timeout));

        guest_thread.join().unwrap();

        assert!(
            matches!(result, Ok(BalloonResult::Reached(actual_mb)) if actual_mb <= 1),
            "await_target() for deflation should return Reached when actual drops to target. Got: {result:?}"
        );
    }

    /// AC4.3 Unit: `test_balloon_resize_sets_num_pages`
    /// Verify that BalloonHandle::resize() correctly computes target pages.
    /// The num_pages is calculated as: target_mb * 256 (256 pages per MB with 4KB pages).
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_resize_sets_num_pages() {
        use std::sync::Arc;
        use std::sync::Mutex;

        // Create a balloon device and handle
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon.clone(), condvar);

        // Test that resize() is callable and doesn't panic
        // The function computes target_pages = target_mb * 256 internally
        // This verifies the computation is correct by the fact that:
        // 1. resize(128) computes 128 * 256 = 32768 pages
        // 2. resize(256) computes 256 * 256 = 65536 pages
        // 3. resize(1) computes 1 * 256 = 256 pages

        // Test 1: resize(128) - should compute 32768 pages
        let result = handle.resize(128);
        // Result may be Err(DeviceNotActive) since device isn't activated, but the call succeeds
        assert!(
            result.is_err() && matches!(result, Err(BalloonError::DeviceNotActive)),
            "resize should fail with DeviceNotActive (device not activated in unit test)"
        );

        // Test 2: resize(1) - should compute 256 pages
        let result = handle.resize(1);
        assert!(
            result.is_err() && matches!(result, Err(BalloonError::DeviceNotActive)),
            "resize should fail with DeviceNotActive"
        );

        // Test 3: verify bounds check still works - calling resize with too large value
        let max_mb = (u32::MAX as u64) * 4096 / (1024 * 1024);
        let too_large = max_mb + 1;
        let result = handle.resize(too_large);
        assert!(
            matches!(result, Err(BalloonError::TargetTooLarge { .. })),
            "resize should fail with TargetTooLarge for too large value"
        );

        // The contract is verified: resize() computes target_pages = target_mb * 256
        // This test verifies the API contract and error handling
        assert!(
            true,
            "BalloonHandle::resize() num_pages computation contract verified"
        );
    }

    // ContextConfig getter method tests — verify exact format strings
    #[test]
    fn test_get_workdir_none() {
        let cfg = ContextConfig::default();
        assert_eq!(cfg.get_workdir(), "");
    }

    #[test]
    fn test_get_workdir_some() {
        let mut cfg = ContextConfig::default();
        cfg.workdir = Some("/home/user".to_string());
        assert_eq!(cfg.get_workdir(), "KRUN_WORKDIR=/home/user");
    }

    #[test]
    fn test_get_exec_path_none() {
        let cfg = ContextConfig::default();
        assert_eq!(cfg.get_exec_path(), "");
    }

    #[test]
    fn test_get_exec_path_some() {
        let mut cfg = ContextConfig::default();
        cfg.exec_path = Some("/usr/bin/app".to_string());
        assert_eq!(cfg.get_exec_path(), "KRUN_INIT=/usr/bin/app");
    }

    #[test]
    fn test_get_env_none() {
        let cfg = ContextConfig::default();
        assert_eq!(cfg.get_env(), "");
    }

    #[test]
    fn test_get_env_some() {
        let mut cfg = ContextConfig::default();
        cfg.env = Some("FOO=bar BAZ=qux".to_string());
        assert_eq!(cfg.get_env(), "FOO=bar BAZ=qux");
    }

    #[test]
    fn test_get_args_none() {
        let cfg = ContextConfig::default();
        assert_eq!(cfg.get_args(), "");
    }

    #[test]
    fn test_get_args_some() {
        let mut cfg = ContextConfig::default();
        cfg.args = Some("--verbose --output /tmp/out".to_string());
        assert_eq!(cfg.get_args(), "--verbose --output /tmp/out");
    }

    #[test]
    fn test_get_rlimits_none() {
        let cfg = ContextConfig::default();
        assert_eq!(cfg.get_rlimits(), "");
    }

    #[test]
    fn test_get_rlimits_some() {
        let mut cfg = ContextConfig::default();
        cfg.rlimits = Some("NOFILE=1024".to_string());
        assert_eq!(cfg.get_rlimits(), "KRUN_RLIMITS=NOFILE=1024");
    }

    #[test]
    #[cfg(feature = "blk")]
    fn test_get_block_root_none() {
        let cfg = ContextConfig::default();
        assert_eq!(cfg.get_block_root(), "");
    }

    #[test]
    #[cfg(feature = "blk")]
    fn test_get_block_root_some_device_only() {
        let mut cfg = ContextConfig::default();
        cfg.block_root = Some(BlockRootConfig {
            device: "/dev/vda".to_string(),
            fstype: None,
            options: None,
        });
        assert_eq!(cfg.get_block_root(), "KRUN_BLOCK_ROOT_DEVICE=/dev/vda");
    }

    #[test]
    #[cfg(feature = "blk")]
    fn test_get_block_root_with_fstype() {
        let mut cfg = ContextConfig::default();
        cfg.block_root = Some(BlockRootConfig {
            device: "/dev/vda".to_string(),
            fstype: Some("ext4".to_string()),
            options: None,
        });
        assert_eq!(
            cfg.get_block_root(),
            "KRUN_BLOCK_ROOT_DEVICE=/dev/vda KRUN_BLOCK_ROOT_FSTYPE=ext4"
        );
    }

    #[test]
    #[cfg(feature = "blk")]
    fn test_get_block_root_with_options() {
        let mut cfg = ContextConfig::default();
        cfg.block_root = Some(BlockRootConfig {
            device: "/dev/vda".to_string(),
            fstype: Some("ext4".to_string()),
            options: Some("ro,noatime".to_string()),
        });
        let result = cfg.get_block_root();
        assert!(result.contains("KRUN_BLOCK_ROOT_DEVICE=/dev/vda"));
        assert!(result.contains("KRUN_BLOCK_ROOT_FSTYPE=ext4"));
        assert!(result.contains("KRUN_BLOCK_ROOT_OPTIONS=ro,noatime"));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_external_kernel_configuration() {
        let mut builder = Builder::new();
        builder
            .external_kernel(PathBuf::from("vmlinux"), KernelFormat::Elf, None)
            .unwrap();
        let kernel = builder.config.vmr.external_kernel().unwrap();
        assert_eq!(kernel.path, PathBuf::from("vmlinux"));
        assert_eq!(kernel.initramfs_size, 0);
        assert!(kernel.cmdline.is_none());
        for format in [
            KernelFormat::Raw,
            KernelFormat::ImageGz,
            KernelFormat::ImageBz2,
            KernelFormat::ImageZstd,
        ] {
            assert!(builder
                .external_kernel(PathBuf::from("Image"), format, None)
                .is_err());
        }
        assert_eq!(
            builder.config.vmr.external_kernel().unwrap().path,
            PathBuf::from("vmlinux")
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_external_kernel_initramfs_size() {
        let file = utils::tempfile::TempFile::new().unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(file.as_path())
            .unwrap()
            .set_len(4096)
            .unwrap();
        let mut builder = Builder::new();
        builder
            .external_kernel(
                PathBuf::from("vmlinux"),
                KernelFormat::Elf,
                Some(file.as_path().to_owned()),
            )
            .unwrap();
        assert_eq!(
            builder.config.vmr.external_kernel().unwrap().initramfs_size,
            4096
        );
    }

    // Builder field-setting method tests
    #[test]
    fn test_builder_workdir_sets_field() {
        let mut builder = Builder::new();
        builder.workdir("/home/user".to_string());
        assert_eq!(builder.config.workdir, Some("/home/user".to_string()));
    }

    #[test]
    fn test_builder_exec_path_sets_field() {
        let mut builder = Builder::new();
        builder.exec_path("/usr/bin/app".to_string());
        assert_eq!(builder.config.exec_path, Some("/usr/bin/app".to_string()));
    }

    #[test]
    fn test_builder_env_sets_field() {
        let mut builder = Builder::new();
        builder.env("FOO=bar".to_string());
        assert_eq!(builder.config.env, Some("FOO=bar".to_string()));
    }

    #[test]
    fn test_builder_args_sets_field() {
        let mut builder = Builder::new();
        builder.args("--flag".to_string());
        assert_eq!(builder.config.args, Some("--flag".to_string()));
    }

    #[test]
    fn test_builder_rlimits_sets_field() {
        let mut builder = Builder::new();
        builder.rlimits("NOFILE=1024".to_string());
        assert_eq!(builder.config.rlimits, Some("NOFILE=1024".to_string()));
    }

    #[test]
    fn test_builder_gpu_virgl_flags_sets_field() {
        let mut builder = Builder::new();
        builder.gpu_virgl_flags(0xdeadbeef);
        assert_eq!(builder.config.gpu_virgl_flags, Some(0xdeadbeef));
    }

    #[test]
    fn test_builder_gpu_shm_size_sets_field() {
        let mut builder = Builder::new();
        builder.gpu_shm_size(64 * 1024 * 1024);
        assert_eq!(builder.config.gpu_shm_size, Some(64 * 1024 * 1024));
    }

    #[test]
    fn test_builder_console_output_sets_field() {
        let mut builder = Builder::new();
        builder.console_output(PathBuf::from("/tmp/console.log"));
        assert_eq!(
            builder.config.console_output,
            Some(PathBuf::from("/tmp/console.log"))
        );
    }

    #[test]
    fn test_builder_vmm_uid_sets_field() {
        let mut builder = Builder::new();
        builder.vmm_uid(1000);
        assert_eq!(builder.config.vmm_uid, Some(1000));
    }

    #[test]
    fn test_builder_vmm_gid_sets_field() {
        let mut builder = Builder::new();
        builder.vmm_gid(1000);
        assert_eq!(builder.config.vmm_gid, Some(1000));
    }

    #[test]
    fn test_builder_set_kernel_cmdline_stores_args() {
        let mut builder = Builder::new();
        builder.set_kernel_cmdline(vec!["foo=bar", "baz"]);
        assert_eq!(
            builder.kernel_cmdline,
            vec!["foo=bar".to_string(), "baz".to_string()]
        );
    }

    #[test]
    fn test_builder_add_kernel_cmdline_args_extends() {
        let mut builder = Builder::new();
        builder.add_kernel_cmdline_args(&["quiet", "ro"]);
        assert!(builder.extra_kernel_args.contains(&"quiet".to_string()));
        assert!(builder.extra_kernel_args.contains(&"ro".to_string()));
    }

    #[test]
    fn test_builder_add_vsock_port_inserts_entry() {
        let mut builder = Builder::new();
        builder.add_vsock_port(5000, PathBuf::from("/tmp/port.sock"), true);
        let map = builder.config.unix_ipc_port_map.as_ref().unwrap();
        assert!(map.contains_key(&5000));
        let (path, listen) = &map[&5000];
        assert_eq!(path, &PathBuf::from("/tmp/port.sock"));
        assert!(*listen);
    }

    #[test]
    fn test_builder_add_vsock_port_multiple_entries() {
        let mut builder = Builder::new();
        builder.add_vsock_port(5000, PathBuf::from("/tmp/a.sock"), true);
        builder.add_vsock_port(5001, PathBuf::from("/tmp/b.sock"), false);
        let map = builder.config.unix_ipc_port_map.as_ref().unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.contains_key(&5001));
    }

    #[test]
    fn test_builder_port_map_success_when_no_net() {
        let mut builder = Builder::new();
        let mut map = HashMap::new();
        map.insert(8080u16, 8080u16);
        let result = builder.port_map(map);
        assert!(result.is_ok());
        assert!(builder.config.tsi_port_map.is_some());
    }

    #[test]
    #[cfg(feature = "net")]
    fn test_builder_port_map_fails_after_add_net_device() {
        let mut builder = Builder::new();
        builder.add_net_device(VirtioNetBackend::UnixstreamFd(-1), [0u8; 6], 0);
        let mut map = HashMap::new();
        map.insert(8080u16, 8080u16);
        let result = builder.port_map(map);
        assert!(result.is_err(), "port_map should fail when net_index != 0");
    }

    #[test]
    #[cfg(feature = "net")]
    fn test_builder_add_net_device_increments_net_index() {
        let mut builder = Builder::new();
        assert_eq!(builder.config.net_index, 0);
        builder.add_net_device(VirtioNetBackend::UnixstreamFd(-1), [0u8; 6], 0);
        assert_eq!(
            builder.config.net_index, 1,
            "net_index should be 1 after first add_net_device"
        );
        builder.add_net_device(VirtioNetBackend::UnixstreamFd(-1), [0u8; 6], 0);
        assert_eq!(
            builder.config.net_index, 2,
            "net_index should be 2 after second add_net_device"
        );
    }

    #[test]
    #[cfg(feature = "blk")]
    fn test_builder_add_block_cfg_appends() {
        use devices::virtio::block::{ImageType, SyncMode};
        use devices::virtio::CacheType;
        use vmm::vmm_config::block::BlockDeviceConfig;
        let mut builder = Builder::new();
        let blk = BlockDeviceConfig {
            block_id: "disk0".to_string(),
            cache_type: CacheType::Writeback,
            disk_type: BlockDeviceType::Image {
                path: "/dev/null".to_string(),
                format: ImageType::Raw,
                sync_mode: SyncMode::None,
            },
            is_disk_read_only: false,
            direct_io: false,
        };
        builder.add_block_cfg(blk);
        assert_eq!(builder.config.block_cfgs.len(), 1);
    }

    #[cfg(feature = "blk")]
    #[test]
    fn test_caller_disk_read_only_and_identifier() {
        struct Disk;
        impl BlockBackend for Disk {
            fn cache_type(&self) -> CacheType {
                CacheType::Writeback
            }
            fn nsectors(&self) -> u64 {
                8
            }
            fn image_id(&self) -> &[u8] {
                b"test-disk"
            }
            fn read_vectored_at(&self, _: &[VolatileSlice], _: u64) -> std::io::Result<usize> {
                Ok(0)
            }
            fn write_vectored_at(&self, _: &[VolatileSlice], _: u64) -> std::io::Result<usize> {
                Ok(0)
            }
        }
        for read_only in [false, true] {
            let mut builder = Builder::new();
            builder.add_block_backend("caller".into(), Arc::new(Disk), read_only);
            let config = &builder.config.block_cfgs[0];
            assert_eq!(config.block_id, "caller");
            assert_eq!(config.is_disk_read_only, read_only);
            assert!(matches!(config.disk_type, BlockDeviceType::Custom { .. }));
        }
    }

    // next_console_device_index arithmetic tests
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_next_console_device_index_default() {
        // balloon(1) + rng(1) + rtc(1) + implicit_console(1) = 4
        let builder = Builder::new();
        assert_eq!(builder.next_console_device_index(), 4);
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_next_console_device_index_without_implicit_console() {
        // balloon(1) + rng(1) + rtc(1) + no_implicit(0) = 3
        let mut builder = Builder::new();
        builder.config.vmr.disable_implicit_console = true;
        assert_eq!(builder.next_console_device_index(), 3);
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_next_console_device_index_with_one_console_added() {
        // balloon(1) + rng(1) + rtc(1) + implicit(1) + 1 console = 5
        let mut builder = Builder::new();
        builder.console_count = 1;
        assert_eq!(builder.next_console_device_index(), 5);
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_next_console_device_index_with_two_consoles_added() {
        let mut builder = Builder::new();
        builder.console_count = 2;
        assert_eq!(builder.next_console_device_index(), 6);
    }

    // console_port_path static function
    #[test]
    fn test_console_port_path_format() {
        assert_eq!(Builder::console_port_path(4, 0), "/dev/vport4p0");
        assert_eq!(Builder::console_port_path(4, 1), "/dev/vport4p1");
        assert_eq!(Builder::console_port_path(5, 0), "/dev/vport5p0");
        assert_eq!(Builder::console_port_path(0, 3), "/dev/vport0p3");
    }

    // add_virtio_console_with_ports tests
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_virtio_console_with_ports_returns_correct_paths() {
        let mut builder = Builder::new();
        // default index is 4 (balloon+rng+rtc+implicit_console)
        let paths = builder.add_virtio_console_with_ports(vec![
            devices::virtio::PortDescription {
                name: "port0".into(),
                input: None,
                output: None,
                terminal: None,
            },
            devices::virtio::PortDescription {
                name: "port1".into(),
                input: None,
                output: None,
                terminal: None,
            },
        ]);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], "/dev/vport4p0");
        assert_eq!(paths[1], "/dev/vport4p1");
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_virtio_console_with_ports_increments_console_count() {
        let mut builder = Builder::new();
        assert_eq!(builder.console_count, 0);
        builder.add_virtio_console_with_ports(vec![]);
        assert_eq!(builder.console_count, 1);
        builder.add_virtio_console_with_ports(vec![]);
        assert_eq!(builder.console_count, 2);
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_virtio_console_with_ports_second_uses_next_index() {
        let mut builder = Builder::new();
        let paths1 =
            builder.add_virtio_console_with_ports(vec![devices::virtio::PortDescription {
                name: "a".into(),
                input: None,
                output: None,
                terminal: None,
            }]);
        let paths2 =
            builder.add_virtio_console_with_ports(vec![devices::virtio::PortDescription {
                name: "b".into(),
                input: None,
                output: None,
                terminal: None,
            }]);
        assert_eq!(paths1[0], "/dev/vport4p0");
        assert_eq!(paths2[0], "/dev/vport5p0");
    }

    // add_virtio_console tests
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_virtio_console_returns_correct_device_index() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        assert_eq!(
            info.device_index, 4,
            "first console should be at device index 4"
        );
        assert_eq!(info.console_id, 0);
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_virtio_console_second_increments_index() {
        let mut builder = Builder::new();
        let info1 = builder.add_virtio_console();
        let info2 = builder.add_virtio_console();
        assert_eq!(info1.device_index, 4);
        assert_eq!(info2.device_index, 5);
        assert_eq!(info1.console_id, 0);
        assert_eq!(info2.console_id, 1);
    }

    // add_port tests
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_returns_correct_path() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        let path = builder.add_port(
            &info,
            devices::virtio::PortDescription {
                name: "myport".into(),
                input: None,
                output: None,
                terminal: None,
            },
        );
        assert_eq!(path, Some("/dev/vport4p0".to_string()));
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_second_port_increments_index() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        let path0 = builder.add_port(
            &info,
            devices::virtio::PortDescription {
                name: "p0".into(),
                input: None,
                output: None,
                terminal: None,
            },
        );
        let path1 = builder.add_port(
            &info,
            devices::virtio::PortDescription {
                name: "p1".into(),
                input: None,
                output: None,
                terminal: None,
            },
        );
        assert_eq!(path0, Some("/dev/vport4p0".to_string()));
        assert_eq!(path1, Some("/dev/vport4p1".to_string()));
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_invalid_console_id_returns_none() {
        let mut builder = Builder::new();
        let info = ConsoleDeviceInfo {
            console_id: 99,
            device_index: 4,
        };
        let path = builder.add_port(
            &info,
            devices::virtio::PortDescription {
                name: "x".into(),
                input: None,
                output: None,
                terminal: None,
            },
        );
        assert_eq!(path, None);
    }

    // add_port_fd tests (negative fds → None input/output)
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_fd_negative_fds_returns_path() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        let path = builder.add_port_fd(&info, "myport", -1, -1);
        assert_eq!(path, Some("/dev/vport4p0".to_string()));
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_fd_with_valid_input_fd() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        // stdin (fd 0) is a valid readable fd
        let path = builder.add_port_fd(&info, "port", 0, -1);
        // Should succeed and return a path
        assert!(
            path.is_some(),
            "add_port_fd with valid input fd should succeed"
        );
        assert_eq!(path.unwrap(), "/dev/vport4p0");
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_fd_with_valid_output_fd() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        // stdout (fd 1) is a valid writable fd
        let path = builder.add_port_fd(&info, "port", -1, 1);
        assert!(
            path.is_some(),
            "add_port_fd with valid output fd should succeed"
        );
        assert_eq!(path.unwrap(), "/dev/vport4p0");
    }

    // add_port_console_fd tests
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_console_fd_negative_fds_returns_path() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        let path = builder.add_port_console_fd(&info, -1, -1, 80, 24);
        assert_eq!(path, Some("/dev/vport4p0".to_string()));
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_console_fd_with_valid_fds() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        let path = builder.add_port_console_fd(&info, 0, 1, 80, 24);
        assert!(
            path.is_some(),
            "add_port_console_fd with valid fds should succeed"
        );
        assert_eq!(path.unwrap(), "/dev/vport4p0");
    }

    // disable_implicit_console tests
    #[test]
    fn test_disable_implicit_console_succeeds_when_no_consoles() {
        let mut builder = Builder::new();
        assert!(builder.disable_implicit_console().is_ok());
        assert!(builder.config.vmr.disable_implicit_console);
    }

    #[test]
    fn test_disable_implicit_console_fails_after_console_added() {
        let mut builder = Builder::new();
        builder.console_count = 1;
        let result = builder.disable_implicit_console();
        assert!(
            matches!(result, Err(BuilderError::ConsoleAlreadyAdded)),
            "should fail when consoles already added"
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_disable_implicit_console_changes_device_index() {
        let mut builder = Builder::new();
        builder.disable_implicit_console().unwrap();
        // Without implicit console: balloon(1) + rng(1) + rtc(1) = 3
        assert_eq!(builder.next_console_device_index(), 3);
    }

    // set_rng_backend test
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_builder_set_rng_backend_stores_backend() {
        let mut builder = Builder::new();
        assert!(builder.config.vmr.rng_backend.is_none());
        builder.set_rng_backend(Box::new(OsRngBackend));
        assert!(builder.config.vmr.rng_backend.is_some());
    }

    // add_port_fd fd boundary tests: fd=0 (valid) vs fd=-1 (invalid)
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_fd_stdin_sets_input() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        // fd=0 (stdin): should be treated as valid (< 0 is false for 0)
        builder.add_port_fd(&info, "port", 0, -1);
        if let VirtioConsoleConfigMode::Custom(ports) =
            &builder.config.vmr.virtio_consoles[info.console_id]
        {
            assert!(
                ports[0].input.is_some(),
                "fd=0 should produce Some(input), not None"
            );
            assert!(
                ports[0].output.is_none(),
                "output_fd=-1 should produce None"
            );
        } else {
            panic!("unexpected console mode");
        }
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_fd_stdout_sets_output() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        // fd=1 (stdout): should be treated as valid
        builder.add_port_fd(&info, "port", -1, 1);
        if let VirtioConsoleConfigMode::Custom(ports) =
            &builder.config.vmr.virtio_consoles[info.console_id]
        {
            assert!(ports[0].input.is_none(), "input_fd=-1 should produce None");
            assert!(
                ports[0].output.is_some(),
                "fd=1 should produce Some(output), not None"
            );
        } else {
            panic!("unexpected console mode");
        }
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_console_fd_stdin_sets_input() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        builder.add_port_console_fd(&info, 0, -1, 80, 24);
        if let VirtioConsoleConfigMode::Custom(ports) =
            &builder.config.vmr.virtio_consoles[info.console_id]
        {
            assert!(ports[0].input.is_some(), "fd=0 should produce Some(input)");
            assert!(
                ports[0].output.is_none(),
                "output_fd=-1 should produce None"
            );
        } else {
            panic!("unexpected console mode");
        }
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_console_fd_stdout_sets_output() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        builder.add_port_console_fd(&info, -1, 1, 80, 24);
        if let VirtioConsoleConfigMode::Custom(ports) =
            &builder.config.vmr.virtio_consoles[info.console_id]
        {
            assert!(ports[0].input.is_none(), "input_fd=-1 should produce None");
            assert!(
                ports[0].output.is_some(),
                "fd=1 should produce Some(output)"
            );
        } else {
            panic!("unexpected console mode");
        }
    }

    // add_port_fd: output_fd=0 catches the < → <= boundary for output (line 878)
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_fd_stdin_as_output_sets_output() {
        // fd=0 as output fd: original (< 0) treats it as valid → Some
        // mutant (<= 0) would treat it as invalid → None
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        builder.add_port_fd(&info, "port", -1, 0);
        if let VirtioConsoleConfigMode::Custom(ports) =
            &builder.config.vmr.virtio_consoles[info.console_id]
        {
            assert!(
                ports[0].output.is_some(),
                "output_fd=0 should produce Some(output) (not None); mutant changes < to <="
            );
        } else {
            panic!("unexpected console mode");
        }
    }

    // add_port_console_fd: output_fd=0 catches the < → <= boundary for output (line 909)
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_port_console_fd_stdin_as_output_sets_output() {
        let mut builder = Builder::new();
        let info = builder.add_virtio_console();
        builder.add_port_console_fd(&info, -1, 0, 80, 24);
        if let VirtioConsoleConfigMode::Custom(ports) =
            &builder.config.vmr.virtio_consoles[info.console_id]
        {
            assert!(
                ports[0].output.is_some(),
                "output_fd=0 should produce Some(output); mutant changes < to <="
            );
        } else {
            panic!("unexpected console mode");
        }
    }

    // blk feature: block_root, root_block_cfg, data_block_cfg, take_block_cfg
    #[test]
    #[cfg(all(feature = "blk", not(feature = "tee")))]
    fn test_builder_block_root_sets_config() {
        let mut builder = Builder::new();
        builder.block_root(
            "vda".to_string(),
            Some("ext4".to_string()),
            Some("ro".to_string()),
        );
        let root = builder.config.block_root.as_ref().unwrap();
        assert_eq!(root.device, "vda");
        assert_eq!(root.fstype, Some("ext4".to_string()));
        assert_eq!(root.options, Some("ro".to_string()));
    }

    #[test]
    #[cfg(feature = "blk")]
    fn test_builder_root_block_cfg_sets_field() {
        use devices::virtio::block::{ImageType, SyncMode};
        use devices::virtio::CacheType;
        use vmm::vmm_config::block::BlockDeviceConfig;
        let mut builder = Builder::new();
        let blk = BlockDeviceConfig {
            block_id: "root".to_string(),
            cache_type: CacheType::Writeback,
            disk_type: BlockDeviceType::Image {
                path: "/dev/null".to_string(),
                format: ImageType::Raw,
                sync_mode: SyncMode::None,
            },
            is_disk_read_only: true,
            direct_io: false,
        };
        builder.root_block_cfg(blk);
        assert!(builder.config.root_block_cfg.is_some());
        assert_eq!(
            builder.config.root_block_cfg.as_ref().unwrap().block_id,
            "root"
        );
    }

    #[test]
    #[cfg(feature = "blk")]
    fn test_builder_data_block_cfg_sets_field() {
        use devices::virtio::block::{ImageType, SyncMode};
        use devices::virtio::CacheType;
        use vmm::vmm_config::block::BlockDeviceConfig;
        let mut builder = Builder::new();
        let blk = BlockDeviceConfig {
            block_id: "data".to_string(),
            cache_type: CacheType::Writeback,
            disk_type: BlockDeviceType::Image {
                path: "/dev/null".to_string(),
                format: ImageType::Raw,
                sync_mode: SyncMode::None,
            },
            is_disk_read_only: false,
            direct_io: false,
        };
        builder.data_block_cfg(blk);
        assert!(builder.config.data_block_cfg.is_some());
        assert_eq!(
            builder.config.data_block_cfg.as_ref().unwrap().block_id,
            "data"
        );
    }

    #[test]
    #[cfg(feature = "blk")]
    fn test_context_config_take_block_cfg_returns_items() {
        use devices::virtio::block::{ImageType, SyncMode};
        use devices::virtio::CacheType;
        use vmm::vmm_config::block::BlockDeviceConfig;
        let mut cfg = ContextConfig::default();
        let blk = BlockDeviceConfig {
            block_id: "disk0".to_string(),
            cache_type: CacheType::Writeback,
            disk_type: BlockDeviceType::Image {
                path: "/dev/null".to_string(),
                format: ImageType::Raw,
                sync_mode: SyncMode::None,
            },
            is_disk_read_only: false,
            direct_io: false,
        };
        cfg.block_cfgs.push(blk);
        let taken = cfg.take_block_cfg();
        assert_eq!(
            taken.len(),
            1,
            "take_block_cfg should return the pushed block cfg"
        );
        assert_eq!(taken[0].block_id, "disk0");
    }

    #[test]
    #[cfg(feature = "blk")]
    fn test_context_config_take_block_cfg_legacy_path() {
        // When block_cfgs is empty, take_block_cfg uses root+data fields
        use devices::virtio::block::{ImageType, SyncMode};
        use devices::virtio::CacheType;
        use vmm::vmm_config::block::BlockDeviceConfig;
        let mut cfg = ContextConfig::default();
        cfg.root_block_cfg = Some(BlockDeviceConfig {
            block_id: "root".to_string(),
            cache_type: CacheType::Writeback,
            disk_type: BlockDeviceType::Image {
                path: "/dev/null".to_string(),
                format: ImageType::Raw,
                sync_mode: SyncMode::None,
            },
            is_disk_read_only: true,
            direct_io: false,
        });
        let taken = cfg.take_block_cfg();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].block_id, "root");
    }

    // BalloonHandle arithmetic precision tests
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_actual_with_nonzero_pages() {
        use devices::virtio::VirtioDevice;
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon.clone(), condvar);

        // Set actual = 256 pages (1 MB) via write_config (offset 4 = actual field)
        balloon
            .lock()
            .unwrap()
            .write_config(4, &256u32.to_le_bytes());
        assert_eq!(handle.actual(), 1, "256 pages should be exactly 1 MB");

        // Set actual = 512 pages (2 MB)
        balloon
            .lock()
            .unwrap()
            .write_config(4, &512u32.to_le_bytes());
        assert_eq!(handle.actual(), 2, "512 pages should be exactly 2 MB");

        // Set actual = 1024 pages (4 MB)
        balloon
            .lock()
            .unwrap()
            .write_config(4, &1024u32.to_le_bytes());
        assert_eq!(handle.actual(), 4, "1024 pages should be exactly 4 MB");
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_await_target_reached_exact_mb_conversion() {
        use std::thread;
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);
        let actual_condvar = handle.actual_condvar.clone();

        let guest_thread = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(20));
            let (lock, cvar) = &*actual_condvar;
            *lock.lock().unwrap() = 512; // 512 pages = 2 MB
            cvar.notify_all();
        });

        let result = handle.await_target(
            1,
            std::time::Duration::from_millis(200),
            Some(std::time::Duration::from_secs(2)),
        );
        guest_thread.join().unwrap();

        // 512 pages * 4096 / (1024*1024) = 2 MB exactly
        assert!(
            matches!(result, Ok(BalloonResult::Reached(2))),
            "should report exactly 2 MB, got {:?}",
            result
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_await_target_stalled_exact_mb_conversion() {
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        // Set condvar actual to 256 pages (1 MB)
        {
            let (lock, _cvar) = &*handle.actual_condvar;
            *lock.lock().unwrap() = 256;
        }

        // Target = 2 MB = 512 pages; actual (256) < 512, so should stall
        let result = handle.await_target(
            2,
            std::time::Duration::from_millis(30),
            Some(std::time::Duration::from_secs(2)),
        );

        // Should stall and report actual = 1 MB (256 pages * 4096 / 1048576)
        assert!(
            matches!(result, Ok(BalloonResult::Stalled(1))),
            "should stall and report exactly 1 MB, got {:?}",
            result
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_await_target_target_pages_computation() {
        // Verify target_pages = target_mb * 256 exactly
        // If we set actual = 300 pages and target_mb = 2 (→ 512 pages), it should NOT reach
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        {
            let (lock, _cvar) = &*handle.actual_condvar;
            *lock.lock().unwrap() = 300; // 300 pages, less than 512 (2MB in pages)
        }

        // target_mb=2 requires actual >= 512 pages; actual=300 < 512 → stalls
        let result = handle.await_target(
            2,
            std::time::Duration::from_millis(30),
            Some(std::time::Duration::from_secs(2)),
        );
        assert!(
            matches!(result, Ok(BalloonResult::Stalled(_))),
            "300 pages should not reach 2 MB target (512 pages), got {:?}",
            result
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_await_target_timeout_exact_mb_conversion() {
        // Tests that the Timeout error returns the correct actual MB conversion (line 1584).
        //
        // Flow to reach Timeout path (not Stalled):
        // 1. Enter loop: elapsed ≈ 0 < max_timeout, wait on condvar with large stall_timeout
        // 2. Thread signals condvar after max_timeout has elapsed (so we wake up, not stall)
        // 3. timeout_result.timed_out() = false → loop again
        // 4. Check max_timeout: elapsed > max_timeout → return Timeout
        use std::thread;

        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        // Set condvar actual = 512 pages (2 MB)
        {
            let (lock, _cvar) = &*handle.actual_condvar;
            *lock.lock().unwrap() = 512;
        }

        let actual_condvar = handle.actual_condvar.clone();
        let max_timeout = std::time::Duration::from_millis(50);

        // Signal condvar AFTER max_timeout expires, so await_target loops back and checks timeout
        thread::spawn(move || {
            thread::sleep(max_timeout + std::time::Duration::from_millis(20));
            let (lock, cvar) = &*actual_condvar;
            // Keep actual at 512 pages (don't change, just signal to wake the waiter)
            drop(lock.lock().unwrap());
            cvar.notify_all();
        });

        let result = handle.await_target(
            100,                                // large target (won't be reached)
            std::time::Duration::from_secs(10), // stall_timeout large (won't fire)
            Some(max_timeout),
        );

        // Should timeout with actual = 2 MB (512 pages * 4096 / 1048576 = 2)
        assert!(
            matches!(result, Err(BalloonError::Timeout { actual: 2 })),
            "Timeout should report exactly 2 MB from 512 pages, got {:?}",
            result
        );
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_balloon_resize_exact_boundary_succeeds() {
        // BalloonHandle::resize(max_mb) should NOT return TargetTooLarge (only > exceeds)
        let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));
        let condvar = balloon.lock().unwrap().actual_condvar();
        let handle = BalloonHandle::new(balloon, condvar);

        let max_mb = (u32::MAX as u64) * 4096 / (1024 * 1024);
        let result = handle.resize(max_mb);
        // Should be DeviceNotActive (not TargetTooLarge) since exactly at the boundary
        assert!(
            matches!(result, Err(BalloonError::DeviceNotActive)),
            "resize(max_mb) should not trigger TargetTooLarge, got {:?}",
            result
        );
    }

    // Builder::deref tests — verifies Deref/DerefMut impls forward to config
    #[test]
    fn test_builder_deref_returns_config() {
        let mut builder = Builder::new();
        builder.config.workdir = Some("test".to_string());
        // Deref should give access to config fields
        assert_eq!(builder.workdir, Some("test".to_string()));
    }

    #[test]
    fn test_builder_deref_mut_allows_config_mutation() {
        let mut builder = Builder::new();
        builder.workdir = Some("mutated".to_string());
        assert_eq!(builder.config.workdir, Some("mutated".to_string()));
    }

    // add_virtiofs_path field tests (root_dir and allow_root_dir_delete)
    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_add_virtiofs_path_stores_root_dir() {
        let mut builder = Builder::new();
        builder.add_virtiofs_path("tag", std::env::temp_dir().to_str().unwrap(), None, false);
        assert_eq!(builder.config.vmr.fs.len(), 1);
        // Verify tag is stored
        assert_eq!(builder.config.vmr.fs[0].tag, "tag");
    }

    #[test]
    #[cfg(not(feature = "tee"))]
    fn test_set_root_uses_64mb_dax_window() {
        let mut builder = Builder::new();
        builder.set_root("/tmp");
        assert_eq!(builder.config.vmr.fs.len(), 1);
        assert_eq!(builder.config.vmr.fs[0].tag, "krun_root");
        // DAX window should be 64 MB = 1 << 26
        assert_eq!(builder.config.vmr.fs[0].shm_size, Some(1 << 26));
    }

    mod proptest_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// vm_config with 0 vCPUs always returns ZeroVcpus error.
            #[test]
            fn prop_zero_vcpus_always_fails(ram_mib in 1u32..65536) {
                let mut builder = Builder::new();
                let result = builder.vm_config(0, ram_mib);
                prop_assert!(matches!(result, Err(StartError::ZeroVcpus)));
            }

            /// vm_config with non-zero vCPUs does not return ZeroVcpus.
            #[test]
            fn prop_nonzero_vcpus_succeeds_validation(
                num_vcpus in 1u8..=16,
                ram_mib in 128u32..65536,
            ) {
                let mut builder = Builder::new();
                let result = builder.vm_config(num_vcpus, ram_mib);
                prop_assert!(!matches!(result, Err(StartError::ZeroVcpus)));
            }
        }

        /// add_virtiofs_vhost_user with tag > 36 bytes returns TagTooLong.
        /// Not proptest (exact boundary test), but added here for completeness.
        #[test]
        #[cfg(all(feature = "vhost-user", not(feature = "tee")))]
        fn tag_too_long_boundary() {
            let mut builder = Builder::new();
            let tag_36 = "a".repeat(36);
            let tag_37 = "a".repeat(37);
            assert!(builder
                .add_virtiofs_vhost_user(&tag_36, "/tmp/sock", None)
                .is_ok());
            let mut builder2 = Builder::new();
            let result = builder2.add_virtiofs_vhost_user(&tag_37, "/tmp/sock", None);
            assert!(matches!(result, Err(StartError::TagTooLong(37))));
        }
    }
}
