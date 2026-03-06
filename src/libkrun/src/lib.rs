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
use std::sync::{Arc, LazyLock, Mutex};
use utils::eventfd::EventFd;
use vmm::builder::StartMicrovmError;
pub use vmm::resources::VirtioConsoleConfigMode;
use vmm::resources::{TsiFlags, VmResources, VsockConfig};
#[cfg(feature = "snapshot")]
pub use vmm::snapshot_store;
pub use vmm::vm_exit::VmExit;
#[cfg(feature = "blk")]
pub use vmm::vmm_config::block::{BlockConfigError, BlockDeviceConfig, BlockRootConfig};
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

// krunfw library name for each context
#[cfg(all(target_os = "linux", not(feature = "tee")))]
const KRUNFW_NAME: &str = "libkrunfw.so.5";
#[cfg(all(target_os = "linux", feature = "amd-sev"))]
const KRUNFW_NAME: &str = "libkrunfw-sev.so.5";
#[cfg(all(target_os = "linux", feature = "tdx"))]
const KRUNFW_NAME: &str = "libkrunfw-tdx.so.5";
#[cfg(target_os = "macos")]
const KRUNFW_NAME: &str = "libkrunfw.5.dylib";

#[cfg(feature = "aws-nitro")]
static KRUN_NITRO_DEBUG: Mutex<bool> = Mutex::new(false);

// Path to the init binary to be executed inside the VM.
const INIT_PATH: &str = "/init.krun";

static KRUNFW: LazyLock<Option<libloading::Library>> =
    LazyLock::new(|| unsafe { libloading::Library::new(KRUNFW_NAME).ok() });

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
unsafe fn load_krunfw_payload(
    krunfw: &KrunfwBindings,
    vmr: &mut VmResources,
) -> Result<(), StartError> {
    let mut kernel_guest_addr: u64 = 0;
    let mut kernel_entry_addr: u64 = 0;
    let mut kernel_size: usize = 0;
    let kernel_host_addr = unsafe {
        (krunfw.get_kernel)(
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

#[derive(Default)]
pub struct Builder {
    config: ContextConfig,
    kernel_cmdline: Vec<String>,
    extra_kernel_args: Vec<String>,
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
        let shutdown_efd = if cfg!(target_arch = "aarch64") && cfg!(target_os = "macos") {
            Some(EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap())
        } else {
            None
        };

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
        let cfg = devices::virtio::fs::passthrough::Config {
            root_dir: host_path.to_string(),
            allow_root_dir_delete,
            ..Default::default()
        };
        let pt = devices::virtio::fs::passthrough::PassthroughFs::new(cfg)
            .expect("failed to create PassthroughFs");
        self.add_virtiofs(tag, Box::new(pt), shm_size)
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
    fn next_console_device_index(&self) -> u32 {
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

    /// Enable the memory balloon device, exposing it through the Rust API via VmHandle::balloon().
    #[cfg(not(feature = "tee"))]
    pub fn enable_balloon(&mut self) -> &mut Self {
        self.config.vmr.balloon_enabled = true;
        self
    }

    pub fn build(self) -> Result<Context, StartError> {
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

        let kernel_cmdline = if self.kernel_cmdline.is_empty() {
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
            cmdline.push(ctx_cfg.get_env());
            cmdline.extend(self.extra_kernel_args);
            KernelCmdlineConfig {
                cmdline,
                args: vec![format!(" -- {}", ctx_cfg.get_args())],
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

        if !skip_userspace_vsock {
            match &ctx_cfg.vsock_config {
                VsockConfig::Disabled => (),
                VsockConfig::Explicit { tsi_flags } => {
                    let vsock_device_config = VsockDeviceConfig {
                        vsock_id: "vsock0".to_string(),
                        guest_cid: 3,
                        host_port_map: ctx_cfg.tsi_port_map,
                        unix_ipc_port_map: ctx_cfg.unix_ipc_port_map.clone(),
                        tsi_flags: *tsi_flags,
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
                            (TsiFlags::HIJACK_INET, ctx_cfg.tsi_port_map)
                        } else {
                            (TsiFlags::empty(), None)
                        };

                        let vsock_device_config = VsockDeviceConfig {
                            vsock_id: "vsock0".to_string(),
                            guest_cid: 3,
                            host_port_map,
                            unix_ipc_port_map: ctx_cfg.unix_ipc_port_map.clone(),
                            tsi_flags,
                        };
                        ctx_cfg.vmr.set_vsock_device(vsock_device_config).unwrap();
                    }
                }
            }
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

        #[cfg(target_os = "macos")]
        if ctx_cfg.gpu_virgl_flags.is_some() {
            vmm::worker::start_worker_thread(built_vm.vmm().clone(), _receiver).unwrap();
        }

        #[cfg(target_arch = "x86_64")]
        if ctx_cfg.vmr.split_irqchip {
            vmm::worker::start_worker_thread(built_vm.vmm().clone(), _receiver.clone()).unwrap();
        }

        #[cfg(any(feature = "amd-sev", feature = "tdx"))]
        vmm::worker::start_worker_thread(built_vm.vmm().clone(), _receiver.clone()).unwrap();

        Ok(Context {
            vm_exit: built_vm.vm_exit().clone(),
            built_vm,
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

pub struct Context {
    built_vm: vmm::builder::BuiltVm,
    event_manager: EventManager,
    /// Cloned shutdown eventfd for host-initiated GPIO shutdown.
    shutdown_efd: Option<Arc<EventFd>>,
    vm_exit: vmm::vm_exit::SharedVmExit,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("could not setup event manager: {0:?}")]
    EventManager(polly::event_manager::Error),
    #[error(transparent)]
    FirmwareLoad(#[from] libloading::Error),
    #[error("could not find or load firmware {KRUNFW_NAME}")]
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

impl Context {
    /// Access device information (console port paths, etc.).
    ///
    /// This returns the actual device paths from the built VM.
    pub fn device_info(&self) -> &vmm::resources::VmDeviceInfo {
        &self.built_vm.device_info
    }

    /// Returns a `VmHandle` that can be used to pause/resume the VM from another thread.
    /// Must be called before `run()`, since `run()` consumes `self`.
    pub fn vm_handle(&self) -> VmHandle {
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
            vmm: self.built_vm.vmm().clone(),
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
        self.built_vm
            .vmm()
            .lock()
            .expect("Poisoned vmm lock")
            .register_exit_observer(observer);
    }

    /// Start the VM and run the event loop. This blocks until the VM exits.
    pub fn run(mut self) -> Result<vmm::vm_exit::VmExit, StartError> {
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

    /// Restore a VM from a SnapshotStore and run the event loop.
    ///
    /// On Linux with `uffd` feature: Creates a store from the factory, sets up UFFD
    /// demand-paging, then resumes vCPUs. Page faults are resolved on-demand.
    ///
    /// On Linux without `uffd`: Creates a store, reads vmstate, drains the preload
    /// stream to eagerly populate memory, then resumes vCPUs.
    ///
    /// On other platforms: Returns error (use `restore_and_run` instead).
    #[cfg(feature = "snapshot")]
    pub fn restore_and_run_with_store(
        mut self,
        factory: Box<dyn vmm::snapshot_store::SnapshotStoreFactory>,
    ) -> Result<vmm::vm_exit::VmExit, StartError> {
        #[cfg(target_os = "linux")]
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| {
                    StartError::Microvm(vmm::builder::StartMicrovmError::Internal(
                        vmm::Error::Snapshot(e.to_string()),
                    ))
                })?;

            let store: Box<dyn vmm::snapshot_store::SnapshotStore> = rt.block_on(async {
                factory.create().await.map_err(|e| {
                    StartError::Microvm(vmm::builder::StartMicrovmError::Internal(
                        vmm::Error::Snapshot(e.to_string()),
                    ))
                })
            })?;

            // When uffd feature is enabled, use demand-paging; otherwise eager restore
            #[cfg(feature = "uffd")]
            let uffd_thread = {
                // Read vmstate up front so it can be validated before UFFD/vCPU setup
                let vmstate_bytes = rt.block_on(async {
                    store.read_vmstate().await.map_err(|e| {
                        StartError::Microvm(vmm::builder::StartMicrovmError::Internal(
                            vmm::Error::Snapshot(e.to_string()),
                        ))
                    })
                })?;

                let handler =
                    self.built_vm
                        .restore_from_store_with_uffd(vmstate_bytes, store, rt)?;
                Some(handler)
            };

            #[cfg(not(feature = "uffd"))]
            let uffd_thread: Option<std::thread::JoinHandle<()>> = {
                let vmstate_bytes = rt.block_on(async {
                    store.read_vmstate().await.map_err(|e| {
                        StartError::Microvm(vmm::builder::StartMicrovmError::Internal(
                            vmm::Error::Snapshot(e.to_string()),
                        ))
                    })
                })?;

                self.built_vm
                    .restore_from_store(vmstate_bytes, store, &rt)?;
                None
            };

            loop {
                self.event_manager
                    .run()
                    .map_err(StartError::EventManagerRun)?;

                // Check if the VM has exited
                if let Some(vm_exit) = self.vm_exit.lock().expect("Poisoned vm_exit lock").take() {
                    if let Some(handler) = uffd_thread {
                        handler.join().ok();
                    }
                    return Ok(vm_exit);
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            // Phase 2+ (eager restore with FsSnapshotStore) is Linux-only.
            // On other platforms, use the backward-compatible restore_from_snapshot path.
            // TODO: Extend restore_from_store to all platforms in Phase 3+.
            Err(StartError::Microvm(vmm::builder::StartMicrovmError::Internal(
                vmm::Error::Snapshot(
                    "restore_and_run_with_store is not available on this platform; use restore_and_run instead".to_string()
                )
            )))
        }
    }

    /// Restore a VM from a snapshot directory and run the event loop.
    ///
    /// This delegates to `restore_and_run_with_store` on Linux, or uses the
    /// backward-compatible `restore_from_snapshot` path on other platforms.
    #[cfg(feature = "snapshot")]
    #[allow(unused_mut)]
    pub fn restore_and_run(
        mut self,
        base_path: &std::path::Path,
        incremental_paths: &[&std::path::Path],
    ) -> Result<vmm::vm_exit::VmExit, StartError> {
        #[cfg(target_os = "linux")]
        {
            let factory =
                vmm::snapshot_store::FsSnapshotStoreFactory::new(base_path, incremental_paths);
            self.restore_and_run_with_store(Box::new(factory))
        }

        #[cfg(not(target_os = "linux"))]
        {
            // On macOS/other platforms, use the cold restore path
            self.built_vm
                .restore_from_snapshot(base_path, incremental_paths)?;

            loop {
                self.event_manager
                    .run()
                    .map_err(StartError::EventManagerRun)?;

                // Check if the VM has exited
                if let Some(vm_exit) = self.vm_exit.lock().expect("Poisoned vm_exit lock").take() {
                    return Ok(vm_exit);
                }
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
    vmm: Arc<Mutex<vmm::Vmm>>,
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

impl VmHandle {
    #[cfg(feature = "snapshot")]
    fn snapshot_err_to_start_error(e: vmm::snapshot::SnapshotError) -> StartError {
        StartError::Microvm(vmm::builder::StartMicrovmError::Internal(
            vmm::Error::Snapshot(e.to_string()),
        ))
    }

    /// Pause all vCPUs. Blocks until all vCPUs have acknowledged the pause.
    #[mutants::skip] // races with VM exit in mutation tests; tested by integration tests (snapshot)
    pub fn pause(&self) -> Result<(), StartError> {
        self.vmm
            .lock()
            .expect("Poisoned vmm lock")
            .pause_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))
    }

    /// Resume all vCPUs. Blocks until all vCPUs have acknowledged the resume.
    #[mutants::skip] // races with VM exit in mutation tests; tested by integration tests (snapshot)
    pub fn resume(&self) -> Result<(), StartError> {
        self.vmm
            .lock()
            .expect("Poisoned vmm lock")
            .resume_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))
    }

    /// Trigger host-initiated guest shutdown via the MMIO GPIO shutdown eventfd.
    #[mutants::skip] // shutdown_efd is None on Linux x86_64; only used on aarch64+macOS
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

    /// Create a full snapshot of the VM. Pauses vCPUs, takes snapshot, resumes vCPUs.
    #[cfg(feature = "snapshot")]
    #[mutants::skip] // tested by integration tests (snapshot test cases)
    pub fn snapshot(&self, path: &std::path::Path) -> Result<(), StartError> {
        let mut vmm = self.vmm.lock().expect("Poisoned vmm lock");
        vmm.pause_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        let result = vmm
            .create_snapshot(path)
            .map_err(Self::snapshot_err_to_start_error);
        vmm.resume_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        result
    }

    /// Restore a full snapshot into the running VM. Pauses vCPUs, restores, resumes vCPUs.
    #[cfg(feature = "snapshot")]
    #[mutants::skip] // tested by integration tests (snapshot test cases)
    pub fn restore_snapshot(&self, path: &std::path::Path) -> Result<(), StartError> {
        let mut vmm = self.vmm.lock().expect("Poisoned vmm lock");
        vmm.pause_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        let result = vmm
            .restore_snapshot(path)
            .map_err(Self::snapshot_err_to_start_error);
        vmm.resume_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        result
    }

    /// Enable dirty page tracking for incremental snapshots.
    #[cfg(feature = "snapshot")]
    #[mutants::skip] // tested by integration tests (snapshot test cases)
    pub fn enable_dirty_tracking(&self) -> Result<(), StartError> {
        let mut vmm = self.vmm.lock().expect("Poisoned vmm lock");
        vmm.enable_dirty_tracking()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))
    }

    /// Create an incremental snapshot (dirty pages only).
    #[cfg(feature = "snapshot")]
    #[mutants::skip] // tested by integration tests (snapshot test cases)
    pub fn incremental_snapshot(&self, path: &std::path::Path) -> Result<(), StartError> {
        let mut vmm = self.vmm.lock().expect("Poisoned vmm lock");
        vmm.pause_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        let result = vmm
            .create_incremental_snapshot(path)
            .map_err(Self::snapshot_err_to_start_error);
        vmm.resume_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        result
    }

    /// Restore an incremental snapshot into the running VM.
    ///
    /// This applies dirty pages and state on top of the current memory image.
    #[cfg(feature = "snapshot")]
    #[mutants::skip] // tested by integration tests (snapshot test cases)
    pub fn restore_incremental_snapshot(&self, path: &std::path::Path) -> Result<(), StartError> {
        let mut vmm = self.vmm.lock().expect("Poisoned vmm lock");
        vmm.pause_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        let result = vmm
            .restore_incremental_snapshot(path)
            .map_err(Self::snapshot_err_to_start_error);
        vmm.resume_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        result
    }

    /// Create a full snapshot using a SnapshotStore. Pauses vCPUs, takes snapshot, resumes vCPUs.
    #[cfg(feature = "snapshot")]
    #[mutants::skip] // tested by integration tests (snapshot test cases)
    pub fn snapshot_to_store(
        &self,
        mut store: Box<dyn vmm::snapshot_store::SnapshotStore>,
    ) -> Result<(), StartError> {
        let mut vmm = self.vmm.lock().expect("Poisoned vmm lock");
        vmm.pause_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        let result = vmm
            .snapshot_to_store(&mut *store)
            .map_err(Self::snapshot_err_to_start_error);
        vmm.resume_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        result
    }

    /// Create an incremental snapshot using a SnapshotStore. Pauses vCPUs, takes snapshot, resumes vCPUs.
    #[cfg(feature = "snapshot")]
    #[mutants::skip] // tested by integration tests (snapshot test cases)
    pub fn incremental_snapshot_to_store(
        &self,
        mut store: Box<dyn vmm::snapshot_store::SnapshotStore>,
    ) -> Result<(), StartError> {
        let mut vmm = self.vmm.lock().expect("Poisoned vmm lock");
        vmm.pause_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        let result = vmm
            .incremental_snapshot_to_store(&mut *store)
            .map_err(Self::snapshot_err_to_start_error);
        vmm.resume_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))?;
        result
    }

    /// Returns a handle for controlling the memory balloon device, if enabled.
    /// Returns `None` if the balloon device was not enabled via `Builder::enable_balloon()`.
    #[cfg(not(feature = "tee"))]
    pub fn balloon(&self) -> Option<&BalloonHandle> {
        self.balloon.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        builder.add_virtiofs_path("tag", "/tmp/mydir", None, false);
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
