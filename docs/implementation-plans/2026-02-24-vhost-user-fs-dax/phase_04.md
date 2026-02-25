# VhostUserFs with DAX Implementation Plan - Phase 4

**Goal:** Wire `VhostUserFs` into the VM construction pipeline via a Rust Builder API method and VMM integration.

**Architecture:** Follows the existing `Builder::add_virtiofs()` pattern: configuration stored in `VmResources`, device created and attached in `build_microvm()` via a new `attach_vhost_user_fs_device()` function. The DAX window gets a GPA range from `ShmManager`, the memfd host address is resolved from guest memory, and the device is attached to the MMIO bus.

**Tech Stack:** Rust

**Scope:** 8 phases from original design (phase 4 of 8)

**Codebase verified:** 2026-02-24

**Reference files:**
- Builder::add_virtiofs(): `src/libkrun/src/lib.rs:2545-2554`
- FsDeviceConfig: `src/vmm/src/vmm_config/fs.rs:1-8`
- attach_fs_devices(): `src/vmm/src/builder.rs:2078-2126`
- attach_mmio_device(): `src/vmm/src/builder.rs:2050-2075`
- ShmManager::create_fs_region(): `src/vmm/src/device_manager/shm.rs:86-90`
- build_microvm() SHM allocation: `src/vmm/src/builder.rs:1671-1688`
- CLAUDE.md: `src/libkrun/CLAUDE.md`, `src/vmm/CLAUDE.md`

---

## Acceptance Criteria Coverage

This phase implements and tests:

### vhost-user-fs-dax.AC3: Builder API configures the device
- **vhost-user-fs-dax.AC3.1 Success:** `add_virtiofs_vhost_user(tag, socket_path, Some(32))` results in a bootable VM with the device visible to the guest kernel
- **vhost-user-fs-dax.AC3.2 Success:** `add_virtiofs_vhost_user(tag, socket_path, None)` configures device without DAX window
- **vhost-user-fs-dax.AC3.3 Failure:** Tag longer than 36 bytes is rejected
- **vhost-user-fs-dax.AC3.4 Success:** Coexists with existing direct FUSE virtio-fs device (both can be configured on same VM)

---

<!-- START_SUBCOMPONENT_A (tasks 1-2) -->

<!-- START_TASK_1 -->
### Task 1: Add VhostUserFsConfig storage to VmResources

**Files:**
- Modify: `src/vmm/src/resources.rs` (add field and accessor)
- Already created: `src/vmm/src/vmm_config/vhost_user_fs.rs` (from Phase 2)

**Implementation:**

Add import and field to `VmResources` (gated on `vhost-user` feature):

```rust
// At top of resources.rs:
#[cfg(feature = "vhost-user")]
use crate::vmm_config::vhost_user_fs::VhostUserFsConfig;

// In VmResources struct:
#[cfg(feature = "vhost-user")]
pub vhost_user_fs: Vec<VhostUserFsConfig>,
```

Initialize to `Vec::new()` in Default impl.

Add accessor method:
```rust
#[cfg(feature = "vhost-user")]
pub fn add_vhost_user_fs_device(&mut self, config: VhostUserFsConfig) {
    self.vhost_user_fs.push(config);
}
```

**Verification:**
```bash
cargo build --features vhost-user
```

**Commit:** `feat(vmm): add VhostUserFsConfig storage to VmResources`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Add Builder::add_virtiofs_vhost_user() method

**Verifies:** vhost-user-fs-dax.AC3.3

**Files:**
- Modify: `src/libkrun/src/lib.rs`

**Implementation:**

Add method to `Builder` following the pattern of `add_virtiofs()` at line 2545:

```rust
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
```

**Tag validation (AC3.3):** Returns `Result<&mut Self, StartError>` rather than the infallible `&mut Self` used by `add_virtiofs()`. The `Result` return was chosen because: (a) vhost-user-fs has an explicit validation requirement (tag length) that the existing `add_virtiofs()` does not enforce, and (b) it matches the `vm_config()` and other Builder methods that validate input (per `src/libkrun/CLAUDE.md` convention). Add a `TagTooLong(usize)` variant to `StartError` enum.

**Testing:**
- **vhost-user-fs-dax.AC3.3:** Tag validation — a unit test that calls `add_virtiofs_vhost_user` with a 37-byte tag and expects panic/error

**Verification:**
```bash
cargo build --features vhost-user
```

**Commit:** `feat(libkrun): add Builder::add_virtiofs_vhost_user() method`
<!-- END_TASK_2 -->

<!-- END_SUBCOMPONENT_A -->

<!-- START_SUBCOMPONENT_B (tasks 3-4) -->

<!-- START_TASK_3 -->
### Task 3: Add attach_vhost_user_fs_device() to builder.rs

**Files:**
- Modify: `src/vmm/src/builder.rs`
- Modify: `src/vmm/src/linux/vstate.rs` (add public method to `Vm`)

**Implementation:**

**Step 1: Add `Vm::register_memory_region()` to vstate.rs**

`Vm.next_mem_slot` is private and `set_user_memory_region` is called internally. Add a public method on `Vm` that encapsulates slot allocation and KVM registration, following the existing pattern in `Vm::set_memory()`:

```rust
/// Register an additional memory region with KVM (e.g., DAX window).
/// Allocates a KVM memory slot and maps guest_phys_addr → userspace_addr.
pub fn register_memory_region(
    &mut self,
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
) -> Result<()> {
    let memory_region = kvm_userspace_memory_region {
        slot: self.next_mem_slot,
        guest_phys_addr,
        memory_size,
        userspace_addr,
        flags: 0,
    };
    unsafe {
        self.fd
            .set_user_memory_region(memory_region)
            .map_err(Error::SetUserMemoryRegion)?;
    };
    self.next_mem_slot += 1;
    Ok(())
}
```

**Note on dirty page tracking:** The existing `Vm::memory_region_set()` method pushes to `self.mem_slots` (behind `#[cfg(feature = "snapshot")]`) for incremental snapshot dirty bitmap enumeration. `register_memory_region()` intentionally does NOT push to `mem_slots` because DAX regions are volatile caches — their contents are not preserved across snapshots (see Phase 6 AC4.5). Add a comment in the implementation explaining this design decision. If future use cases need dirty tracking for additional KVM slots, extend `register_memory_region()` to accept an option to track the slot.

**Step 2: Create `attach_vhost_user_fs_device()` in builder.rs**

Following the pattern of `attach_fs_devices()` at line 2078:

```rust
#[cfg(not(feature = "tee"))]
#[cfg(feature = "vhost-user")]
fn attach_vhost_user_fs_device(
    vmm: &mut Vmm,
    config: &VhostUserFsConfig,
    shm_manager: &mut ShmManager,
    shm_index: usize,
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    // 1. Create VhostUserFs device
    let mut vhost_fs = VhostUserFs::new(
        &config.tag,
        &config.socket_path,
        config.dax_window_mib,
    ).map_err(StartMicrovmError::RegisterVhostUserDevice)?;

    // 2. Wire up DAX window SHM region (if configured)
    //    The DAX memfd was created in VhostUserFs::new(). We need to:
    //    a. Get the GPA range from ShmManager
    //    b. mmap the memfd into the VMM's host address space
    //    c. Register the mapping with KVM (so guest GPA accesses hit the memfd)
    //    d. Tell the device about the region for MMIO capability advertisement
    //
    //    NOTE: Unlike the existing attach_fs_devices() which uses
    //    guest_memory.get_host_address() (because that FS device's backing
    //    memory IS part of GuestMemoryMmap), the DAX window is a separate
    //    memfd NOT part of guest memory. We mmap it directly and register
    //    with KVM as an additional memory slot.
    if let Some(shm_region) = shm_manager.fs_region(shm_index) {
        if let Some(dax_fd) = vhost_fs.dax_window_fd() {
            let dax_size = vhost_fs.dax_window_size().unwrap();

            // 2a. mmap the DAX memfd into VMM host address space
            let host_addr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    dax_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    dax_fd,
                    0,
                )
            };
            if host_addr == libc::MAP_FAILED {
                return Err(StartMicrovmError::MmapDaxWindow(
                    std::io::Error::last_os_error()
                ));
            }

            // 2b. Register with KVM via Vm's public method
            vmm.vm.register_memory_region(
                shm_region.guest_addr.raw_value(),
                dax_size as u64,
                host_addr as u64,
            ).map_err(StartMicrovmError::RegisterDaxMemoryRegion)?;

            // 2c. Tell device about the region (for MMIO SHM cap advertisement)
            vhost_fs.set_shm_region(VirtioShmRegion {
                host_addr: host_addr as u64,
                guest_addr: shm_region.guest_addr.raw_value(),
                size: dax_size,
            });
        }
    }

    // 3. Attach to MMIO bus
    let device = Arc::new(Mutex::new(vhost_fs));
    let id = format!("virtio-fs-vhost-{}", shm_index);
    attach_mmio_device(vmm, id, intc, device)?;

    Ok(())
}
```

**DAX memory backing architecture:** The DAX window requires three mappings:
1. **Host mmap**: `mmap(MAP_SHARED, dax_fd)` maps the memfd into VMM process address space
2. **KVM registration**: `Vm::register_memory_region()` maps guest GPA → host address via `KVM_SET_USER_MEMORY_REGION`, so guest accesses to the DAX GPA range hit the memfd memory
3. **Daemon sharing**: `ADD_MEM_REGION` (in Phase 3 activate()) passes the memfd fd to the daemon, which mmaps it independently

All three processes (VMM, guest via KVM, daemon) share the same underlying memfd, enabling zero-copy DAX access. This is different from the existing `attach_fs_devices()` pattern where the FS backing memory IS part of `GuestMemoryMmap` — the DAX memfd is a separately managed region.

**Note:** Add error variants `MmapDaxWindow(io::Error)` and `RegisterDaxMemoryRegion(linux::vstate::Error)` to `StartMicrovmError`.

**Step 2: Wire into build_microvm()**

**Critical: GPA allocation must happen AFTER `shm_manager.regions()` is consumed.**

The existing `build_microvm()` flow is:
1. Lines 1674-1680: Create regular FS SHM regions in ShmManager
2. Lines 1681-1686: Create GPU SHM region in ShmManager
3. Line 1688: `arch_mem_regions.extend(shm_manager.regions())` — adds ALL SHM regions to GuestMemoryMmap
4. Line 1690: `GuestMemoryMmap::from_ranges(&arch_mem_regions)` — registers KVM slots for all regions

Regular FS SHM regions are anonymous-backed memory inside GuestMemoryMmap. The existing `attach_fs_devices()` gets the host_addr from `vmm.guest_memory.get_host_address()`.

For vhost-user FS DAX, the memory is backed by a **separate memfd** (for fd-passing to the daemon). It MUST NOT be part of GuestMemoryMmap, or KVM will register two overlapping slots at the same GPA. The DAX region GPA is allocated by ShmManager but the KVM slot is registered separately by `attach_vhost_user_fs_device()`.

**Allocate DAX GPAs AFTER `shm_manager.regions()` is consumed (after line 1688):**

```rust
// Line 1688: arch_mem_regions.extend(shm_manager.regions());
// Line 1690: GuestMemoryMmap::from_ranges(&arch_mem_regions);

// AFTER GuestMemoryMmap creation — allocate DAX GPAs without adding to GuestMemoryMmap
#[cfg(not(feature = "tee"))]
#[cfg(feature = "vhost-user")]
{
    let fs_count = vm_resources.fs.len();  // offset past regular FS regions
    for (i, vhost_fs_config) in vm_resources.vhost_user_fs.iter().enumerate() {
        if let Some(dax_mib) = vhost_fs_config.dax_window_mib {
            let size = (dax_mib as usize) * 1024 * 1024;
            shm_manager
                .create_fs_region(fs_count + i, size)
                .map_err(StartMicrovmError::ShmCreate)?;
        }
    }
}
```

Since `shm_manager.regions()` was already consumed, these new regions are only used for GPA range queries (`shm_manager.fs_region(shm_index)`) in `attach_vhost_user_fs_device()`. They are NOT added to GuestMemoryMmap and do NOT get automatic KVM slots.

Then, after existing FS device attachment, attach vhost-user FS devices:

```rust
#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
#[cfg(feature = "vhost-user")]
{
    let fs_count = vm_resources.fs.len();
    for (i, vhost_fs_config) in vm_resources.vhost_user_fs.iter().enumerate() {
        attach_vhost_user_fs_device(
            &mut vmm,
            vhost_fs_config,
            &mut shm_manager,
            fs_count + i,
            intc.clone(),
        )?;
    }
}
```

This ensures:
- SHM region indices don't collide between regular FS and vhost-user FS devices (AC3.4)
- DAX regions get GPA ranges from ShmManager but are NOT in GuestMemoryMmap
- Only one KVM slot per DAX GPA (registered in `attach_vhost_user_fs_device`)

**Verification:**
```bash
cargo build --features vhost-user
```

**Commit:** `feat(vmm): wire VhostUserFs into build_microvm pipeline`
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Builder API unit tests

**Verifies:** vhost-user-fs-dax.AC3.3, vhost-user-fs-dax.AC3.4

**Files:**
- Modify: `src/libkrun/src/lib.rs` (add tests module if not present)

**Testing:**

- **vhost-user-fs-dax.AC3.3:** Tag too long — call `add_virtiofs_vhost_user` with tag of 37 bytes, assert it returns `Err(StartError::TagTooLong(37))`
- **vhost-user-fs-dax.AC3.4:** Coexistence — construct a Builder, call both `add_virtiofs("fs1", "/shared")` and `add_virtiofs_vhost_user("vhostfs", "/tmp/sock", Some(32))`, verify both configs are stored in VmResources without conflict. This is a config-level test; runtime coexistence is verified in Phase 8 integration tests.

AC3.1 and AC3.2 require a running daemon and VM — tested in Phase 8.

**Verification:**
```bash
cargo test -p libkrun --features vhost-user
```

**Commit:** `test(libkrun): add Builder API validation tests for vhost-user FS`
<!-- END_TASK_4 -->

<!-- END_SUBCOMPONENT_B -->
