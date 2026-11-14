# libkrun crate

## Purpose
Expose `Builder`, `Context`, `VmHandle`, `StartError`, `VmExit` and the optional balloon handle.

## Contracts
- `Builder::vm_config` rejects zero vCPUs; filesystem tags are bounded at 36 bytes.
- `Builder::build` creates a `Context`; obtain a `VmHandle` before moving the context into its run loop.
- `Context::run` returns `VmExit` without terminating the host process.
- `VmHandle::pause` and `resume` wait for vCPU acknowledgements.
- `Builder::enable_balloon` must precede build; `VmHandle::balloon` returns None otherwise.
- Balloon resize rejects inactive devices and targets exceeding representable page counts.
- `Builder::enable_nested_virt` precedes build and requires host nested KVM support.
- `add_virtiofs` accepts a generic boxed filesystem; `add_virtiofs_path` is the passthrough convenience method.
- vhost-user vsock accepts a socket path or pre-connected UnixStream, mutually exclusive with userspace vsock.
- Firmware is dynamically loaded by default; `static-firmware` supports static musl launchers.
- `external_kernel` configures external kernel/initramfs loading without requiring libkrunfw.
- `add_block_backend` attaches caller storage with an identifier and read-only flag.
- `outbound_policy` and `internet_socket_forwarding` configure userspace socket forwarding, not virtio-net or external servers.
- See `../../docs/embedding.md` for image formats, ownership, DAX callbacks and policy errors.

All API implementation lives in `src/lib.rs`. VMM lifecycle belongs to the vmm crate; device queue ownership/reset belongs to devices. `VmHandle::notify_vm_generation_change()` is the copy-notification operation: call on the running copy after memory replacement/start; it writes a fresh ID and raises the device interrupt, returning the new ID or an error. The KVM guest test verifies the kernel reseeds its random pool twice. Use the root Nix/just tooling; the integration workspace has a separate lockfile.
