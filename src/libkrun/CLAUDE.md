# Public Rust API crate

## Purpose
Expose `Builder`, `Context`, `VmHandle`, `StartError`, `VmExit` and the optional balloon handle. The library is a Rust `lib`, not a C shared library.

## Contracts
- `Builder::vm_config` rejects zero vCPUs; filesystem tags are bounded at 36 bytes.
- `Builder::build` creates a `Context`; obtain a `VmHandle` before moving the context into its event loop.
- `Context::run` returns `VmExit` (shutdown, reboot request or error) without terminating the host process.
- `VmHandle::pause` and `resume` wait for vCPU acknowledgements.
- `Builder::enable_balloon` must precede build; `VmHandle::balloon` returns None otherwise.
- Balloon resize rejects inactive devices and targets exceeding representable page counts. `await_target` uses the device condvar and distinguishes reached, stalled and timed out states.
- `Builder::enable_nested_virt` precedes build and requires host nested KVM support.
- `add_virtiofs` accepts a generic boxed filesystem; `add_virtiofs_path` is the passthrough convenience API.
- vhost-user vsock accepts a socket path or pre-connected UnixStream, mutually exclusive with userspace vsock.
- Firmware is dynamically loaded by default; `static-firmware` supports static musl launchers.

All API implementation lives in `src/lib.rs`. VMM lifecycle belongs to the vmm crate; device queue ownership/reset belongs to devices. Keep VM generation ID support and guest copy notification. Use the root Nix/just tooling; the integration workspace has a separate lockfile.
