# Embedding through the Rust API

The public crate is `krun`. Configure a `Builder`, call `build()` to obtain a
`Context`, and consume it with `Context::run()`. Device backends are owned by the
VM; callers can retain an `Arc` to observe shared storage after the VM exits.

## External kernel and initramfs

```rust,ignore
builder.external_kernel(kernel_path, krun::KernelFormat::Elf, Some(initramfs_path))?;
builder.set_kernel_cmdline(vec!["console=hvc0", "rdinit=/init", "panic=-1"]);
builder.add_kernel_cmdline_args(&["quiet"]);
```

`external_kernel(&mut self, PathBuf, KernelFormat, Option<PathBuf>) ->
std::io::Result<&mut Self>` selects an external kernel and optional initramfs.
It measures the initramfs for guest-memory placement. Keep the files available
and unchanged until `build()` completes. The loader copies their contents into
owned guest memory, which remains alive through `run()`; no borrowed image
buffers or open files are required afterwards. External boot does not require
`libkrunfw` on the library path.

On x86-64, `Elf` accepts an uncompressed ELF64 Linux `vmlinux` with a physical
64-bit entry point. `ImageGz`, `ImageBz2`, and `ImageZstd` accept images containing
respectively gzip, bzip2, or zstd compressed ELF payloads; these loaders search
for the compression signature. `Raw` is not accepted by this public setter on
x86-64. On arm64, `Raw` accepts an uncompressed Linux `Image`, loaded at
0x80000000; `PeGz` accepts a PE image containing a gzip-compressed Linux Image.
Other architecture/format combinations return `InvalidInput`.

`set_kernel_cmdline(Vec<&str>)` overrides the command line and
`add_kernel_cmdline_args(&[&str])` appends arguments. With an external kernel and
an explicit override, bundled-init environment and execution arguments are not
appended. Without an override, the existing bundled-init defaults remain.

## Caller-owned block storage

With feature `blk`, use
`add_block_backend(&mut self, id: String, backend: Arc<dyn BlockBackend + Send + Sync>, read_only: bool) -> &mut Self`.
`BlockBackend`, `CacheType`, and volatile I/O slice types are re-exported from
`krun`. The device identifier and read-only feature are guest-visible; the
backend supplies its capacity, image identifier, cache behavior, and I/O.
`add_block_cfg(BlockDeviceConfig)` continues to accept file images and custom
asynchronous backend factories.

## Caller-owned filesystem

`add_virtiofs(&mut self, tag: &str, fs: Box<dyn FileSystem + Send + Sync>, shm_size: Option<usize>) -> &mut Self`
attaches a FUSE server mounted in the guest by tag. It is available outside TEE
builds. `add_virtiofs_path` provides host-directory passthrough.

Virtio-fs supports a DAX shared-memory window: `shm_size` is its size in bytes;
`None` disables it. The filesystem implements `setupmapping` and `removemapping`
using the supplied `DaxMapper`; `map_file` and `map_data` populate the window.
Ordinary reads work without DAX. The backend must not retain a mapper beyond
the lifetime of its mapping request.

## Host socket authorization

`outbound_policy(&mut self, Arc<dyn OutboundPolicy>) -> &mut Self` installs a
per-guest callback:

```rust,ignore
fn allow(&self, operation: SocketOperation, destination: &SocketAddress) -> bool;
```

`SocketAddress` preserves IPv4/IPv6 addresses and ports and Unix paths/abstract
names. `SocketOperation` distinguishes TCP connect, UDP connect/send, bind,
listen, Unix connect/bind, and Unix path removal. Callbacks run on device threads
and must return promptly. Connect/listen denial returns `EACCES`; UDP send denial
resets the guest socket because the forwarding protocol has no per-send error
response. Connected UDP destinations are rechecked on every send.

`internet_socket_forwarding(&mut self, enabled: bool) -> &mut Self` disables host
IPv4/IPv6 socket operations when false. Unix-backed ports configured with
`add_vsock_port` remain available and are subject to the policy's Unix decisions.
Both controls default to today's unrestricted forwarding behavior.

These controls govern the userspace vsock forwarding device, not virtio-net or
an external vhost-user-vsock server; those transports require their own host
network isolation. Combining these controls with vhost-user-vsock returns
`StartError::UnsupportedSocketPolicy` rather than ignoring the controls.
Device configuration is independent of the hypervisor.
