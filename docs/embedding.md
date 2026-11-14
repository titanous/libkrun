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

The Nitro backend (`nitro-sandbox` feature) applies the same socket rule and
forwarding switch to its shared userspace vsock device. With external-kernel
packaging it also appends the guest kernel's forwarding switches
(`tsi_hijack`, `tsi_hijack_unix`) to the packaged command line exactly when the
device forwards those families, as the KVM builder inserts them; a prepackaged
EIF must carry them itself. It boots an EIF v4 image
through `nitro_backend`: the image contains the arm64 Linux Image, initramfs and
command line, with flags matching PCI virtio and CPU-overcommit mode.
Alternatively, `external_kernel` accepts an arm64 Raw Image, optional initramfs,
and the configured replacement/appended command line: the fork packages EIF v4
internally. Packaged-image command lines cannot be overridden; other
unsupported device/configuration methods fail build with
`unsupported on this backend`, never silently discarded. The Rust packager is
byte-for-byte tested against the kernel lane's deterministic Python builder.
The external-kernel path streams opened inputs into a memfd with a 64 KiB buffer,
without retaining full kernel/initramfs or packaged byte vectors. Without a disk
cache that sealed image is released after load. Disk-cache entries are named by
the final EIF SHA-256 and verified against the expected content on every hit;
readers take no exclusive cache lock. Publication uses unique temporary files and
atomic rename, so killed writers leave no published partial image. The current
verification path regenerates one expected image even on hits; this costs CPU
and memory bandwidth, but keeps transient userspace storage near one image size.
The older byte-slice packaging API retains at most 64 MiB in its process cache;
images larger than that are not retained there. The packaged EIF path bypasses
packaging. For cross-process reuse call `nitro_image_cache(directory)` with a
pre-created VMM-uid-owned directory that is not group/other-writable. The daemon
owns disk-cache eviction and must not modify published files in place. The cache
directory is not a tenant input; symlink entries are refused. Caller-supplied block storage and its read-only setting
are shared with KVM unchanged. Select Nitro and configure block/vsock devices
before adding custom consoles, and call `disable_implicit_console()`. Returned
console paths follow Nitro's actual block/vsock/console registration order; build
rejects selecting the backend after console paths were returned or changing that
layout afterward. An implicit console is rejected rather than silently omitted.
When new `add_block_cfg` entries and legacy root/data setters are mixed, the new
entries take precedence, matching KVM; console indices count only selected disks.

`Context::device_info()` returns configured CPUs, RAM and actual console paths.
`Context::vm_handle()` works without KVM state: `pause`, `resume` and
`notify_vm_generation_change` return an Unsupported error, and `balloon` returns
None. `trigger_shutdown_event` requests runtime teardown and `run` returns
`VmExit::Stopped`; it is not a guest workload success report. Exit observers and
caller-supplied block `on_exit` callbacks run once after workers stop, including
runtime errors and watchdog exits. Queued custom-console output is drained before
final shutdown, bounded to one queue of descriptors; guest-controlled device reset
does not read invalidated rings. Callers must provide nonblocking output callbacks;
a callback that itself blocks can prevent teardown.

The connection rule and forwarding switch survive guest driver unbind/rebind and
transport reset. The repeatable real-host regression is
`python3 src/nitro_sandbox/examples/reset-policy.py IMAGE` (CID 210), after building
the `nitro_reset` example and packaging `guest-reset.c` as `/init`.
`port_map` requires `internet_socket_forwarding(true)` on both backends. The
`nitro-sandbox` feature cannot be combined with `tee` or legacy `aws-nitro`.
Kernel-console diagnostics are discarded by default. Set
`nitro_console_sink(writer, total_bytes, bytes_per_second)` to receive bounded
output in a nonblocking embedder writer. Excess bytes are dropped, never queued;
the lifetime cap does not reset and the rate cap replenishes once per second.
This is independent of custom virtio-console port sinks. Sink callbacks must not
block; byte limits cannot bound an arbitrary callback's execution time.
Sink bytes are raw guest output, including escape sequences, invalid UTF-8 and
forged line prefixes. The embedder must escape terminal controls and encode bytes
safely before writing terminals or structured logs. No sink is installed by
default: output is drained and discarded, never implicitly sent to stdout.

### Host-owned lifecycle lock

On hosts requiring serialized teardown, call
`nitro_lifecycle_lock(path, NitroSerialization::Teardown)` before build.
`StartAndTeardown` holds one lock continuously from VM creation through a
successful start, including loading and device setup. The daemon
creates a regular lock file with service-owned permissions before launching VMMs;
the library opens it without following symlinks and retains that inode. Do not
unlink, replace, or rotate the file while any VMM exists. No lock is used by default;
that default is not safe on hosts requiring serialized teardown, including m9g.

**The host daemon retains a VM descriptor for every guest.** Give the builder
the VMM's end of a connected Unix socket to the daemon with
`nitro_descriptor_handoff(socket)`: `build` then sends a duplicate of the VM
descriptor (one `SCM_RIGHTS` message, payload `vm_descriptor\n`;
`nitro_sandbox::lifecycle::receive_vm_descriptor` reads it) immediately after
the create ioctl returns, before any other VM ioctl, and fails if the send
fails. Once the message is sent the descriptor in flight is a reference of its
own, so the only interval in which a death of the VMM can make the final close
is the return of that one ioctl. After `build`, wait (with a time limit) for the
daemon's acknowledgment before `run`; the daemon must acknowledge from a thread
that never performs locked closes, and the lock must not be held across that
round trip (use `Teardown` serialization). `duplicate_nitro_vm_descriptor()`
after `build` remains for tests; its interval spans the whole build. A VM lives
until the last duplicate is closed: SIGKILL, a crash, OOM, or a panicking
destructor in the VMM cannot perform the final close while the daemon retains
its duplicate, and the guest's vCPUs keep running until the daemon closes it.
This handoff is required on hosts with serialized teardown.

The daemon opens the same inode using `NitroLifecycleLock::open`, reaps the VMM,
then calls `NitroOpenLifecycleLock::close_vm_descriptor(&mut descriptor)` off its
event loop. The helper acquires exclusive flock before consuming the descriptor;
on lock failure ownership stays with the caller, which must retry. Close all
retained duplicates serially. Reaping alone is not guest destruction while a
duplicate remains. Never drop a retained descriptor outside the lock. Prefer
handle stop for orderly worker shutdown. See `nitro_supervised` and
`supervised-lifecycles.py` for bounded handoff/kill/reap/close fixtures.

Call `builder.install_nitro_sigterm_handler()` before `build` and retain the
returned guard through build and run. SIGTERM received during build is retained
as a stop request; during run it returns `VmExit::Stopped`. Registration is
explicit because it changes process-wide signal policy. The post-build handle
method remains available, but cannot protect the creation interval. On teardown
backend refusal, `run` returns an error after locked close and retains mappings
that a worker may still access until the one-guest process exits. Exit observer
panics are contained so cleanup reaches the locked close.
All cooperating VMMs and the daemon must resolve the path to the same inode,
including across mount namespaces. Starts/stops in different processes are
serialized by the kernel's flock, not a process-local mutex.

The device watchdog bounds readiness/reset recovery and returns
`VmExit::DeviceWatchdog`. For workload liveness, configure
`nitro_heartbeat(port, socket_path, deadline)`: the embedder supplies a parent
directory owned by the VMM uid with owner-only permissions (normally 0700).
The VMM binds a new Unix socket before guest creation/start, refuses any existing
path, and reserves that mapped vsock port. Normal teardown removes the socket;
after SIGKILL/crash the daemon removes its stale socket inside the private
directory after reaping the VMM. Call `VmHandle::suspend_liveness_deadline()`
before an intentional freeze and `resume_liveness_deadline()` afterward; resume
starts a fresh full deadline, excluding suspended time. The guest agent connects to host CID 2 at the port
and sends ASCII `H` periodically (recommend at most one third of the deadline).
The deadline starts after the guest starts and refreshes only on `H`; failure to
connect or silence returns `VmExit::LivenessExpired`. Reconnection is supported.
The mapping remains subject to the embedder's Unix-connect policy; allow its exact
socket path. Existing paths and port collisions are errors, not overwritten.

The following console-marker classification is legacy compatibility only and is
not the platform's privileged terminal-reason contract. The maintained kernel's
`HOLODECK GUEST STOP panic` diagnostic marker returns
`VmExit::GuestPanic` promptly, even when console output is discarded or its budget
is exhausted. Marker matching handles split reads and kernel prefixes. This text
is not an authenticated tenant status channel. Missing markers are not evidence
of clean shutdown; liveness still bounds a hung guest. Use the maintained kernel
stop convention and `panic=-1` to avoid an indefinite panic halt.
`backend_must_enforce_configured_rule` is a public-API KVM conformance test using
a live denied host listener; it fails if the configured rule is ignored. Backend
integration must run that same semantic test or assert explicit unsupported
build failure; merely retaining a setter field is not compliance.

Internet forwarding is **off by default on both backends**, enabled only by
`internet_socket_forwarding(true)`. This switch is enforced for explicit proxy
requests too, not only guest hijack flags. Forwarding tests and embedders requiring
TSI TCP/UDP/IPv6 must opt in; mapped `vsock-guest-connect` does not require internet
forwarding and remains subject to its Unix connection rule.

### Privileged terminal-reason channel (API alignment pending)

`nitro_stop_reason(true)` adds a dedicated experimental virtio type 63 device,
registered after ordinary devices so console indices stay stable. The matching
kernel patch is 0400 on holodeck `nitro-backend-kernel`, based on kernel lane
`b94b54c`. It maps private shared region ID 0, stores a one-byte terminal reason,
orders the store and directly notifies one queue. Panic reporting allocates no
descriptor, opens no socket, and waits for no interrupt. Panic is sticky across
its following restart. The VMM wakes on the kick eventfd and reads the byte.
Console text does not classify exits when this channel is enabled; poweroff
without a stated byte is an error, not clean shutdown.

Current provisional codes are 1 poweroff, 2 reboot, 3 halt, 4 panic. The KVM
exit-kind worker must publish the shared codes and public exit variants before
this is a stable embedding API. Until alignment, `run` deliberately returns an
`Error` containing the raw stated reason and host observation timestamp; do not
consume that diagnostic string as a permanent protocol. `nitro_terminal` retains
the real-host acceptance probe.

Observed m8g panic notification upper bounds: ten trials 0.784--0.845 ms from
host sending a go byte to VMM observing the privileged reason, before teardown.
The bound includes vsock delivery and the guest's panic path. Poweroff/reboot/halt
were 0.360--0.383 ms. Both timestamps use host CLOCK_MONOTONIC. Guest/host virtual
counter offsets differ and must not be subtracted. VM teardown adds substantial
time after detection; a prior go-to-run-return sample was 109.33 ms, not the
notification latency. Silent hangs remain bounded by the running heartbeat.

## Nitro public method support and limits

| Public method(s) | Nitro behavior |
|---|---|
| `Builder::new`, `build` | Configure/build one guest; errors are returned before running. |
| `vm_config` | CPU count and RAM honored; CPU zero rejected by setter. RAM minimum 64 MiB, 2 MiB multiple. Default 1 CPU/1024 MiB. |
| `nitro_backend` | Select packaged EIF or external-kernel packaging; CID and nonzero readiness watchdog honored. |
| `external_kernel` | arm64 Raw Image plus optional initramfs, internally packaged; other formats rejected. |
| `set_kernel_cmdline`, `add_kernel_cmdline_args` | Honored with external kernel; rejected for prepackaged EIF. No implicit command-line rewriting. |
| `nitro_image_cache` | Optional daemon-owned persistent content-addressed cache across VMM processes; otherwise bounded process-local cache. |
| `outbound_policy`, `internet_socket_forwarding` | Exact configured policy/switch honored across driver resets. Forwarding defaults off. |
| `add_vsock_port` | Guest-originated mapped Unix connections/listens, subject to policy. |
| `port_map` | Honored only with internet forwarding enabled; otherwise build rejects nonempty map. |
| `add_block_cfg`, `add_block_backend`, `root_block_cfg`, `data_block_cfg` | Shared block backend/read-only semantics and exit callbacks; new list takes precedence over legacy setters. |
| `add_virtiofs`, `add_virtiofs_path`, `set_root` | In-process FileSystem or passthrough served over virtio-fs; missing host paths are build errors. Tags 1..36 bytes. Nonzero DAX windows are rejected before guest allocation. |
| `disable_implicit_console` | Required; cannot omit an already-configured implicit console silently. |
| `add_virtio_console`, `add_virtio_console_with_ports`, `add_port`, `add_port_fd`, `add_port_console_fd`, `console_port_path` | Custom ports honored; select backend/configure preceding block/vsock/fs devices first. Incompatible late ordering rejected. |
| `nitro_console_sink` | Bounded kernel diagnostics, discard by default; independent of custom console ports. |
| `nitro_heartbeat` | Reserved guest-originated heartbeat mapping and running-guest deadline. |
| `nitro_stop_reason` | Opt-in privileged byte channel; current raw diagnostic mapping awaits shared KVM exit API alignment. |
| `nitro_lifecycle_lock` | Daemon-owned flock for teardown or creation/start plus teardown; no fixed-path fallback. |
| `console_output` | KVM console convenience path rejected; use bounded Nitro sink/custom port. |
| `workdir`, `exec_path`, `block_root`, `env`, `args`, `rlimits`, `vmm_uid`, `vmm_gid` | Rejected at build; guest agent owns execution and service owns VMM credentials. |
| `add_net_device`, `net_mac`, `set_rng_backend`, `enable_balloon`, `enable_nested_virt`, `gpu_virgl_flags`, `gpu_shm_size` | Rejected at build. GPU/display/input/sound configurations exposed by feature-specific extensions also rejected. |
| `add_virtiofs_vhost_user`, `add_vsock_vhost_user`, `add_vsock_vhost_user_fd`, other external vhost-user configuration | Rejected at build; use in-process servers. |
| `nitro_image`, `nitro_start_flags`, `tee_config_file` | APIs of incompatible legacy/confidential features; combining `aws-nitro` or `tee` with `nitro-sandbox` is a compile-time error. |
| `Context::device_info`, `vm_handle`, `register_exit_observer`, `run` | Supported without KVM state; metadata, stop handle, exactly-once exit dispatch, typed result. |
| `VmHandle::trigger_shutdown_event`, `install_sigterm_handler` | Orderly stop and explicit scoped SIGTERM registration; returns `Stopped`. |
| `VmHandle::pause`, `resume`, `notify_vm_generation_change` | Clear Unsupported error. No running-guest copy support in this backend. |
| `VmHandle::balloon` | None; therefore BalloonHandle operations are unavailable on Nitro. |

Nitro virtio-fs has no DAX window. Set `shm_size` to `None` (or zero);
`build` rejects every nonzero value before creating a guest. The runtime also
rejects nonzero windows, and never installs the copying mapper. Reads, writes,
and guest page-cache mappings use ordinary FUSE requests. A copied file window
cannot provide coherent DAX semantics when a file changes. KVM DAX is unchanged.
Filesystem mounts incur no device-region allocation. Every other device region
is still zeroed before registration, and workers stop before VM close.

Observed m8g no-DAX coverage includes mount by tag, correct contents,
create/write/rename/delete, 1,000 files and complete 64 MiB reads. The real treefs
read-only/writable adapters also pass 3,000 writable operations and live tree
replacement. Historical copying-window experiments are not supported behavior.

Set `NITRO_PROFILE=1` to emit ASCII timings for each hypervisor ioctl, mapping,
image load and VM close to stderr. This is opt-in diagnostic output; route it to
a file. `nitro_agent_timing` measures build, first guest protocol byte and total
run separately. Image-load timing includes its registration ioctls and must not
be added to them twice.

Limits: memory is bounded by the host's configured pool, CPU admission and guest
slots by the driver; requested CPU count fits `u8` (1..255) but host capacity can
reject it. RAM above 1 GiB uses a second GPA range at 0x400000000; both sides of
the old 1 GiB boundary were tested, not the full pool maximum. Device config is
at most 64 bytes, 64 queues per device, and low-level registration at most four
shared ranges of 256 MiB each; public filesystem support uses one range per mount.
No fixed public total device maximum is claimed beyond driver/info-page capacity.

Measured sustained full-device launch rate on shared m8g: 9,338/9,338 in
1,200.843 seconds with eight workers, **7.776 launches/s**, effectively unchanged
from 7.777 before event-driven waiting. Removing millisecond polling does not
remove the observed launch bottleneck. Firmware, guest work, memory/teardown and
harness disk-copy costs remain; their exact shares were not profiled. This is
below the approximate 10/s design target, not a claim of meeting it.
