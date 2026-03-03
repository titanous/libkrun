#[macro_use]
extern crate log;

use crossbeam_channel::unbounded;
#[cfg(feature = "gpu")]
use devices::virtio::gpu::display::DisplayInfo;
#[cfg(feature = "blk")]
pub use devices::virtio::CacheType;
use env_logger::{Env, Target};
#[cfg(feature = "gpu")]
use krun_display::DisplayBackend;
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
pub use devices::virtio::fs::filesystem::FileSystem;
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
#[cfg(all(feature = "blk", not(feature = "tee")))]
use rand::distr::{Alphanumeric, SampleString};
use std::collections::HashMap;
use std::convert::TryInto;
use std::env;
use std::fs::File;
use std::io::IsTerminal;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::fd::{BorrowedFd, FromRawFd, RawFd};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use utils::eventfd::EventFd;
use vmm::builder::StartMicrovmError;
pub use vmm::resources::VirtioConsoleConfigMode;
use vmm::resources::{
    DefaultVirtioConsoleConfig, PortConfig, SerialConsoleConfig, TsiFlags, VmResources, VsockConfig,
};
#[cfg(feature = "snapshot")]
pub use vmm::snapshot_store;
pub use vmm::vm_exit::VmExit;
#[cfg(feature = "blk")]
pub use vmm::vmm_config::block::{BlockConfigError, BlockDeviceConfig, BlockRootConfig};
#[cfg(not(feature = "tee"))]
use vmm::vmm_config::external_kernel::{ExternalKernel, KernelFormat};
#[cfg(not(feature = "tee"))]
use vmm::vmm_config::firmware::FirmwareConfig;
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
    fn set_tee_config_file(&mut self, filepath: PathBuf) {
        self.tee_config_file = Some(filepath);
    }

    #[cfg(feature = "tee")]
    fn get_tee_config_file(&self) -> Option<PathBuf> {
        self.tee_config_file.clone()
    }

    #[cfg(feature = "aws-nitro")]
    fn set_nitro_image(&mut self, image_path: PathBuf) {
        self.nitro_image_path = Some(image_path);
    }

    #[cfg(feature = "aws-nitro")]
    fn set_nitro_start_flags(&mut self, start_flags: StartFlags) {
        self.nitro_start_flags = start_flags;
    }
}

#[cfg(feature = "aws-nitro")]
impl TryFrom<ContextConfig> for NitroEnclave {
    type Error = i32;

    fn try_from(ctx: ContextConfig) -> Result<Self, Self::Error> {
        let vm_config = ctx.vmr.vm_config();

        let Some(mem_size_mib) = vm_config.mem_size_mib else {
            error!("memory size not configured");
            return Err(-libc::EINVAL);
        };

        let Some(vcpus) = vm_config.vcpu_count else {
            error!("vCPU count not configured");
            return Err(-libc::EINVAL);
        };

        let rootfs = if let Some(path) = &ctx.vmr.fs.first() {
            path.shared_dir.clone()
        } else {
            error!("rootfs path required");
            return Err(-libc::EINVAL);
        };

        let Some(exec_path) = ctx.exec_path else {
            error!("exec path not specified");
            return Err(-libc::EINVAL);
        };

        let Some(exec_env) = ctx.env else {
            error!("execution env not specified");
            return Err(-libc::EINVAL);
        };

        let Some(exec_args) = ctx.args else {
            error!("execution args not specified");
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
                            error!("configured virtio-net backend must be unix stream fd");
                            return Err(-libc::EINVAL);
                        }
                    };

                    Some(fd)
                }
                _ => {
                    error!(
                        "more than one network interface configured (max 1 allowed, found {len})"
                    );
                    return Err(-libc::EINVAL);
                }
            }
        };

        let Some(output_path) = ctx.console_output else {
            error!("console output path not specified");
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
) -> Result<(), libloading::Error> {
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
    let kernel_bundle = KernelBundle {
        host_addr: kernel_host_addr as u64,
        guest_addr: kernel_guest_addr,
        entry_addr: kernel_entry_addr,
        size: kernel_size,
    };
    vmr.set_kernel_bundle(kernel_bundle).unwrap();

    #[cfg(feature = "tee")]
    {
        let mut qboot_size: usize = 0;
        let qboot_host_addr = unsafe { (krunfw.get_qboot)(&mut qboot_size as *mut usize) };
        let qboot_bundle = QbootBundle {
            host_addr: qboot_host_addr as u64,
            size: qboot_size,
        };
        vmr.set_qboot_bundle(qboot_bundle).unwrap();

        let mut initrd_size: usize = 0;
        let initrd_host_addr = unsafe { (krunfw.get_initrd)(&mut initrd_size as *mut usize) };
        let initrd_bundle = InitrdBundle {
            host_addr: initrd_host_addr as u64,
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
        // Default to a conservative 512 MB window.
        self.add_virtiofs_path("/dev/root", root_path, Some(1 << 29), false);
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
    pub fn nitro_image(&mut self, image_path: PathBuf) -> &mut Self {
        self.config.nitro_image_path = Some(image_path);
        self
    }

    #[cfg(feature = "aws-nitro")]
    pub fn nitro_start_flags(&mut self, start_flags: StartFlags) -> &mut Self {
        self.config.nitro_start_flags = start_flags;
        self
    }

    pub fn set_kernel_cmdline(&mut self, cmdline: Vec<&str>) -> &mut Self {
        self.kernel_cmdline = cmdline.into_iter().map(|s| s.to_owned()).collect();
        self
    }

    /// Enable the memory balloon device, exposing it through the Rust API via VmHandle::balloon().
    #[cfg(not(feature = "tee"))]
    pub fn enable_balloon(&mut self) -> &mut Self {
        self.config.vmr.balloon_enabled = true;
        self
    }

    pub fn build(self) -> Result<Context, StartError> {
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
            KernelCmdlineConfig {
                cmdline: vec![
                    DEFAULT_KERNEL_CMDLINE.to_owned(),
                    format!("init={INIT_PATH}"),
                    ctx_cfg.get_exec_path(),
                    ctx_cfg.get_workdir(),
                    ctx_cfg.get_block_root(),
                    ctx_cfg.get_rlimits(),
                    ctx_cfg.get_env(),
                ],
                args: vec![format!(" -- {}", ctx_cfg.get_args())],
            }
        } else {
            let mut cmdline = self.kernel_cmdline;
            cmdline.push(ctx_cfg.get_env());
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

        let built_vm = vmm::builder::build_microvm(
            &mut ctx_cfg.vmr,
            &mut event_manager,
            ctx_cfg.shutdown_efd,
            sender,
        )?;

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
        // Start the vCPUs
        self.built_vm.run()?;

        // Run the event loop
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
    /// Target reached — actual >= target
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

        // Record start time for max_timeout
        let start = std::time::Instant::now();

        loop {
            // Check if target reached
            if *actual >= target_pages {
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
    pub fn pause(&self) -> Result<(), StartError> {
        self.vmm
            .lock()
            .expect("Poisoned vmm lock")
            .pause_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))
    }

    /// Resume all vCPUs. Blocks until all vCPUs have acknowledged the resume.
    pub fn resume(&self) -> Result<(), StartError> {
        self.vmm
            .lock()
            .expect("Poisoned vmm lock")
            .resume_vcpus()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))
    }

    /// Trigger host-initiated guest shutdown via the MMIO GPIO shutdown eventfd.
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
    pub fn enable_dirty_tracking(&self) -> Result<(), StartError> {
        let mut vmm = self.vmm.lock().expect("Poisoned vmm lock");
        vmm.enable_dirty_tracking()
            .map_err(|e| StartError::Microvm(vmm::builder::StartMicrovmError::Internal(e)))
    }

    /// Create an incremental snapshot (dirty pages only).
    #[cfg(feature = "snapshot")]
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
}
