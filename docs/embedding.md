# Embedding through the Rust API

Configure `krun::Builder`, call `build()` for a `Context`, and consume it with
`Context::run()`. The VM owns its device backends. A caller may retain an `Arc`
to observe shared block storage after exit.

## External kernel, initramfs and command line

`external_kernel(&mut self, path: PathBuf, format: KernelFormat, initramfs_path: Option<PathBuf>) -> std::io::Result<&mut Self>`
selects external images without requiring `libkrunfw` on the library path.
Keep files available through build. Contents are copied into owned guest memory;
no borrowed buffers or open files need to survive through run.

On x86-64 only `Elf` is supported: ELF64 Linux vmlinux with a physical 64-bit
entry. `Raw`, `ImageGz`, `ImageBz2`, and `ImageZstd` return unsupported from the
setter. On arm64 `Raw` accepts uncompressed Linux Image at 0x80000000 and `PeGz`
a PE image containing gzip Image; decode/read errors propagate. Architecture
formats not listed are unsupported. Arm64 loading is not runtime-tested here.

The initramfs length used at build is the length actually read, not cached
metadata. Placement is page aligned, above the loaded kernel, and on x86 below
the MMIO gap even for 4/8 GiB guests. Oversized or overlapping images return a
build error. External boot without an explicit command line starts from an empty
command line rather than bundled-init defaults.

`set_kernel_cmdline(&mut self, Vec<&str>) -> &mut Self` replaces the command line;
`add_kernel_cmdline_args(&mut self, &[&str]) -> &mut Self` appends caller tokens.
An explicit command line receives no KRUN environment, execution arguments or
`--` separator. The VMM adds only device-required configuration:
- x86 virtio-mmio transport discovery entries for configured MMIO devices;
- `tsi_hijack` when internet socket forwarding is enabled, and
  `tsi_hijack_unix` for explicitly configured Unix forwarding;
- a caller-configured kernel console replaces/adds the `console=` token safely.

Named-console `KRUN_STDIN_DEV`, `KRUN_STDOUT_DEV`, and `KRUN_STDERR_DEV` additions
are only used with bundled-init defaults, never explicit overrides. Without an
override, bundled-kernel callers retain bundled-init defaults and arguments.

## Guest stop reasons

`Context::run()` returns `VmExit`. Handle `PowerOffRequested` as declared guest
power-off, `RebootRequested` as requested replacement/reboot, `Halted` as a
terminal halt, `Panic` as a kernel panic, and `Fault` as an undeclared terminal
fault. Preserve `Shutdown { exit_code }` for legacy device/guest exit-code paths
and `Error { message }` for VMM/KVM failures. Existing exhaustive matches must
add the four new variants; do not treat every `Ok` result as clean power-off.
Keep console collection for diagnostics, not classification.

On x86 KVM, reserved I/O port **0xe8** accepts exactly one byte: 0 none,
1 power-off, 2 reboot, 3 halt, 4 panic. Invalid values or non-byte writes are
ignored. Reads return zero. This does not overlap emulated CMOS, serial or
i8042 ports, or cooperative-copy ports 0xe6/0xe7. Only privileged guest code
can issue the transaction. The last valid byte is retained until stop; writing
it does not itself terminate execution. It takes precedence over the later
terminal mechanism, but does not hide a host/VMM error. Guest kernels must write
panic intent before console operations and preserve it through panic restart.
No pvpanic device is required.

Raw KVM shutdown/triple fault without a reason is `Fault`. A 10 ms event-loop
timer pauses CPUs, samples KVM MP state and RFLAGS, then resumes them; only when
all created CPUs are HALTED with IF clear does it stop the VM. Interruptible
idle does not count. This bounded sampling also detects a no-reason halt as
`Fault`; it adds periodic host sampling even for legacy guests. The maintained
kernel halts rather than resetting after reporting reason 3. Older kernels
still boot: their power-off terminal fault now returns `Fault`; i8042 reset
without a reason retains legacy `Shutdown`. Never infer declared intent from
a legacy result or console marker.

arm64 KVM PSCI SYSTEM_OFF maps to `PowerOffRequested`, SYSTEM_RESET to
`RebootRequested`. No arm64 panic/halt reason register is implemented: those
remain fault/unknown-class, not declared power-off. ARM is compile-checked only;
known baseline dependency/VMGENID errors may prevent a complete compile.
The AWS backend must transport these same codes over its own privileged channel
and combine them with its liveness signal; missing intent after confirmed loss
of liveness is fault-class. This branch does not modify that backend.

Console device reset consumes pending control messages and synchronously drains
published TX descriptors whose ports have not started workers; active workers
drain and join before `run()` returns. Guest programs must first have a valid
console descriptor: the test init explicitly opens `/dev/hvc0` with a bounded
readiness retry, since an early inherited `/dev/console` may not yet be bound.
`console_output_before_stop_50_times` checks all 4096 numbered lines immediately
before each declared stop, 50 times per action (200 guests), without a drain sleep.

Real KVM tests are in `tests/stop.rs`. Build `tests/stop-fixture.S` with
`-nostdlib -static -no-pie -Wl,-Ttext=0x100000,-e,_start` and respectively
`-DSTOP_FAULT`, `-DSTOP_MISMATCH`, and `-DSTOP_HALT` to files `fault`, `mismatch`,
`halt` in `STOP_FIXTURES`. Set `EMBED_KERNEL` to the protocol kernel and
`EMBED_INITRAMFS` to the static embedding initramfs; run with an outer timeout
and `--ignored --test-threads=1`.

## Caller-supplied block storage

With `blk`:
`add_block_backend(&mut self, id: String, backend: Arc<dyn BlockBackend + Send + Sync>, read_only: bool) -> &mut Self`.
`BlockBackend`, `CacheType`, and volatile I/O slice types are re-exported. The
backend supplies capacity, image identifier, cache behavior and I/O. Sync and
async workers reject writes, discard, write-zeroes and flush on read-only
devices with virtio I/O errors before calling the supplied object, regardless of
guest feature negotiation. Those features are not advertised on read-only disks.
`add_block_cfg(BlockDeviceConfig)` retains image and async-factory support.
Backends must bounds-check requests and must not block indefinitely.

## Caller-supplied filesystem and readiness

Outside TEE builds:
`add_virtiofs(&mut self, tag: &str, fs: Box<dyn FileSystem + Send + Sync>, shm_size: Option<usize>) -> &mut Self`.
The guest mounts by tag; `add_virtiofs_path` retains host-directory passthrough.
The FileSystem implementation enforces read-only semantics; virtio-fs has no
separate device-level read-only setting.

Virtio-fs supports DAX: `shm_size` is window bytes, `None` disables it. Implement
`setupmapping`/`removemapping` using the supplied `DaxMapper` (`map_file`,
`map_data`, `unmap`); do not retain it beyond the request. Ordinary reads need no
DAX. The mapper's runtime implementation must match the selected backend.

`FileSystem::init` errors are returned in the FUSE INIT reply, not swallowed.
Linux virtio-fs queues INIT asynchronously and returns from mount without waiting
(`virtio_fs_fill_super` calls `fuse_send_init`, then returns zero). Therefore an
INIT failure may follow successful mount; the supported kernel reports
`ECONNREFUSED` on the first access. The server cannot retroactively fail mount.
Embedders must perform a readiness operation (for example stat/open a known
file) and treat failure as failed attachment, not treat mount success as ready.
The KVM regression verifies failure isolation and rejected access. Driver source:
https://github.com/torvalds/linux/blob/v6.18/fs/fuse/virtio_fs.c .

## Host socket rules, inbound and outbound

`outbound_policy(&mut self, Arc<dyn OutboundPolicy>) -> &mut Self` installs a
per-guest rule covering both outbound access and inbound exposure:
`fn allow(&self, operation: SocketOperation, destination: &SocketAddress) -> bool`.
`SocketAddress` is re-exported with IPv4/IPv6/Unix accessors.

IPv4-mapped IPv6 destinations are normalized to IPv4 before every rule call.
Native IPv6 scope IDs, flow info and ports are passed as supplied (scope IDs
select host interfaces). Unix paths and abstract names are passed as supplied:
treat them as untrusted strings. Relative paths, `..` and symlink traversal are
not canonicalized. Prefix checks are unsafe; deny tenant Unix forwarding unless
the embedding environment independently controls path resolution and directories.
**Guest 127.0.0.1 destinations are the host's loopback when forwarding is on**;
allowing them exposes host-local services, not a private guest loopback.

Operations distinguish `TcpConnect`, `UdpConnect`, `UdpSend`,
`UnixDatagramConnect`, `UnixDatagramSend`, `Bind`, `Listen`, `UnixConnect`,
`UnixBind`, `UnixRemove`, `Accept`, and `ReceiveDatagram`. Bind/listen checks see
the actual host address after port mapping. Every accepted stream checks its
peer before guest exposure; denial closes it. Every received datagram checks its
source before delivery; denial consumes/drops it. Accepted streams retain the
creation decision; ordinary stream reads do not reauthorize established peers.
Unix listening port-map configuration denied by the rule is skipped with a
warning, and denied cleanup leaves its socket path for the embedder to remove.

Rules run synchronously on the guest's vsock device/TX or worker thread while
proxy locks are held. They **must be nonblocking, in-memory and return promptly**:
a slow rule can stall every stream of that guest, including mapped agent ports.
No partially authorized host operation starts before the rule returns; slow
callbacks do not mutate proxy state midway through authorization. Do not call
back into VM controls or wait for another guest operation from a rule.

TSI connect/listen refusal returns `EACCES` from the rule result, never thread
errno. Plain mapped AF_VSOCK ports use reset refusal; the supported guest kernel
reports `ECONNRESET` from connect. UDP connect refusal returns `EACCES`.
UDP send/sendto has no per-send result response in the forwarding protocol:
the initial send can return its byte count even when refused. No host datagram is
sent; a reset is queued asynchronously, affecting later socket operations where
the guest kernel exposes it. Do not interpret send success as delivery.

Use a closed rule instead of writing a permissive default:
```rust,ignore
let rule = krun::AllowlistPolicy::new([
    (krun::SocketOperation::TcpConnect, "127.0.0.1:9000".parse::<std::net::SocketAddr>()?.into()),
]);
builder.outbound_policy(std::sync::Arc::new(rule));
```
`AllowlistPolicy` refuses everything except exact operation/address pairs;
`AllowlistPolicy::new([])` refuses all. Bind, accept and received-datagram sources
need their own entries. Unix entries are exact raw names, not prefix rules.

`internet_socket_forwarding(&mut self, enabled: bool) -> &mut Self` when false
clears internet hijacking in the guest and refuses explicit internet proxy
operations. Guest-local loopback/listen/connect continue to work; mapped vsock
ports still work subject to Unix rules. This is not a firewall for virtio-net.
External vhost-user-vsock plus these controls fails build with
`UnsupportedSocketPolicy`, not silent omission.

## Contract for every hypervisor backend

Every backend must honour each configured method, or fail build with a clear
`unsupported on this backend` error. It must never silently ignore configuration:
- external kernel/initramfs: boot the supplied bytes with checked placement, or
  reject them (an EIF-only backend must reject separate images);
- command-line override/append: deliver exact caller tokens plus listed required
  device tokens, or reject unsupported command-line configuration;
- block backend: serve the supplied object with identifier and enforced access
  mode; filesystem: serve the supplied object/tag and requested DAX behavior, or
  reject attachment;
- socket rule: apply the same rule object at all listed operations; forwarding
  switch: preserve guest-local sockets and mapped ports while disabling host
  internet forwarding, or reject build.

The separate Nitro branch currently needs integration work for external images,
command line and socket controls. This lane does not modify that branch.
`backend_must_enforce_configured_rule` is a public-API KVM conformance test using
a live denied host listener; it fails if the configured rule is ignored. Backend
integration must run that same semantic test or assert explicit unsupported
build failure; merely retaining a setter field is not compliance.

Recommend internet forwarding **off by default on both backends**, enabled only
explicitly by the embedder. This branch retains KVM's existing on default for
compatibility; changing the shared default belongs to backend integration.
Current-default dependencies include `tsi-tcp-guest-connect`,
`tsi-tcp-guest-listen`, the forwarding portions of `src/libkrun/tests/embed.rs`
(TCP/UDP/IPv6 and deny-rule conformance), and host-daemon tests using implicit
TSI networking. Mapped `vsock-guest-connect` does not require internet forwarding.
