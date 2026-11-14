# VMM crate

## Purpose
Construct and run Linux KVM guests, attach shared virtio devices, and report guest exit without terminating the host process.

## Contracts
- `build_microvm` consumes structured `VmResources` and returns `BuiltVm` with device information and shared exit state.
- `BuiltVm::run` starts vCPU threads in their initial paused state and resumes them. Pause/resume remain public lifecycle operations.
- `Vmm::stop` invokes exit observers, signals vCPU termination, and stores `VmExit` for `Context::run`.
- Production `VcpuHandle::drop` kicks and joins its thread; test cleanup has a separate implementation.
- `register_memory_region` allocates additional KVM slots for DAX windows. vhost-user configurations use memfd-backed guest RAM.
- `Vmm::get_balloon` exposes the optional balloon device to the public handle API.
- VM generation ID construction/registration is retained on x86-64 and aarch64 for copy notification.
- Kernel-bundle host addresses are checked against the host page size during normal resource configuration.
- Nested virtualization is threaded through VM resources and CPU configuration; boot-time MSR setup remains in the architecture crate.

## Key files
- `builder.rs`: payload selection, memory/device construction and `BuiltVm`.
- `linux/vstate.rs`: KVM VM/vCPU setup, run/pause/resume state machine and thread teardown.
- `lib.rs`, `vm_exit.rs`: lifecycle, event subscriber and exit dispatch.
- `device_manager`: legacy port I/O and KVM MMIO registration.
- `resources.rs`, `vmm_config`: validated resource/device configuration.

`net` and `blk` forward to devices features so this crate can be tested independently. Use the root justfile inside Nix for build/check/test. The crate does not know about a C API.
