# Cooperative cloning on Linux KVM (x86-64)

A template is a guest that froze itself, not a VMM snapshot. The source never
runs again. New VMM processes construct fresh device backends in the same order
and enter the kernel's saved continuation over private file-backed RAM.

## API and ordering

1. Boot the maintained guest kernel with hibernation and CPU hotplug enabled;
   platform kernels disable virtio balloon and page reporting. Call
   `Builder::omit_balloon_device()` to omit the transport (default is unchanged).
2. Configure `capture_cooperative_template(0, new_path)` before `build()`.
   Zero selects control-page discovery from BSP RSI at the trapped frozen exit;
   a nonzero GPA is an optional exact-match expectation. Console text is never
   used for discovery or memory bounds.
3. The embedder explicitly arms the test trigger with
   `VmHandle::arm_template_capture(timeout)`, or requests a host-driven freeze
   with `VmHandle::freeze_template(timeout)` (see embedding.md). Without arming,
   capture is refused and the source recovers. For the test trigger, root
   writes `1` to `/sys/power/nitro_freeze` (0600/CAP_SYS_ADMIN; an active
   transition is refused). The generic core checks hibernation availability,
   runs PM prepare notifiers, syncs and freezes configured filesystems, locks
   device hotplug and restricts GFP allocations during device freeze. It freezes tasks,
   suspends consoles, invokes device FREEZE callbacks, offlines secondary CPUs,
   disables interrupts and suspends syscore devices. It saves processor context,
   publishes the page, then writes byte 1 to I/O port `0xe5`.
4. Only BSP may emit that exit. The VMM parks it pending an acceptance decision,
   checks virtio transports have no active backend or DRIVER_OK queues,
   validates the page, saves KVM clock nanoseconds and copies RAM extents into a
   sealed memfd. It exports a create-new disk image before accepting capture.
   On refusal the VMM returns a negative errno in RAX; the x86 hook restores
   processor state and the generic core recovers devices and thaws tasks through
   PMSG_RECOVER. On acceptance it stops/joins all vCPU threads permanently and
   returns `VmExit::Frozen { template_path }`. An accepted source never runs again.
5. Configure the same vCPUs, RAM size and devices, using independent writable
   disk children and fresh vsock/console backends. Call
   `start_from_template_fd(sealed_file)` or `start_from_template_path(path)`.
   Path import retains a read-only descriptor and maps the original file
   privately without copying RAM. It refuses symlinks, foreign owners and
   group/other permissions. The caller must prevent writes and truncation for
   the lifetime of every template and child mapping; unlink is safe. Passed
   descriptors must already have write/grow/shrink seals.
6. Build maps every RAM extent `MAP_PRIVATE`, registers KVM slots and creates
   fresh in-kernel irqchip/PIT and vCPUs. It does not load a payload or rewrite
   boot page tables, command line or boot parameters. It restores KVM clock,
   fills realtime/tag/random seed, updates VMGENID and signals its interrupt.
7. BSP enters the published continuation with IF clear. The kernel restores
   processor context and syscore state, enables interrupts, corrects wall time,
   reseeds VM-fork entropy and explicitly wipes the seed before any task thaws.
   APs start through normal hotplug startup, devices through RESTORE, then consoles
   and tasks resume. A pending x86 getrandom syscall result frozen at signal return
   is regenerated after a fork-generation change before returning to userspace.
   The sysfs trigger retains `path= msg= err= seq=` status; agents can poll it for
   POLLPRI, then seek/read the changed sequence independently of VMGENID.
   Old vsock connections are dead; userspace reconnects guest-to-host CID 2.
   On x86 the final post-RESTORE hook refreshes realtime through 32-bit I/O reads:
   port `0xe6` returns low 32 bits of host realtime nanoseconds since Unix epoch,
   `0xe7` returns high 32 bits. A high-low-high read repeats on rollover.
   Reading current time avoids treating restoration latency as a fixed wall-clock
   offset. The first read also re-injects the already-updated VMGENID notification
   once: the pre-entry interrupt edge can be cleared by IOAPIC syscore restoration.
   The control-page realtime word remains the resume-time validity flag. Kernels with
   x86 patch 0101 require these ports; older VMMs are incompatible with that patch.

The caller supplies the same device order and features. RAM/vCPU count,
control-page version, long-mode prerequisites, and a byte-exact host topology
manifest are checked. The manifest records console structure, block/net/vsock
features and queues, and ordered filesystem tags, queues and DAX GPA ranges.
Backend paths, connection policy and server objects are deliberately excluded:
new guests receive their own configuration and fresh backends. This format is
same-host/same-kernel only, not a portable or hostile-input migration format.
Full CPUID/kernel-build compatibility fencing is required before accepting
untrusted templates. GPU and vhost-user backends are unsupported. Network
devices are refused for both capture and template starts: virtio-net restore
does not re-read the MAC address, so a fresh backend cannot safely supply a new
identity. Use guest-initiated vsock with each new guest's own connection policy.

In-process virtio-fs supports plain and DAX mounts with the kernel's cooperative
FUSE patches (0200/0201). FREEZE revokes DAX mappings; capture excludes those
host-known SHM extents, and each clone allocates empty mappings at exactly the
same GPAs before attaching fresh servers. No DAX content or host file descriptor
is serialized. Servers must preserve the filesystem's stable inode identity and
handle semantics across reconstruction; this is a caller contract, not something
a tag/queue manifest can verify.

## Control page

4096 bytes, 512 little-endian u64 words. Preserve the arm64 meanings where
applicable; x86-specific words form a documented superset. The discovery address
is physical. Entry/table/stack/percpu addresses below are virtual under CR3.

| Word | Meaning |
|---|---|
| 0 | frozen publication marker `0x46524f5a454e3031`, written last |
| 1 | arm64 release word; zero in an x86 template |
| 2 | x86 saved post-call stack pointer |
| 3 | CR3 physical page-table root |
| 4 | CR4 |
| 5 | virtual `nitro_x86_resume` entry |
| 6–7 | arm64 interrupt-controller fields; unused on x86 |
| 8 | host realtime nanoseconds since Unix epoch, filled for each new guest |
| 9 | new random clone tag |
| 10–13 | 32-byte fresh VM-fork entropy seed |
| 14 | seed-valid flag, consumed by guest |
| 15–17 | reserved/arm64 counter fields |
| 18 | resume path status, set to 3 by x86 continuation |
| 19 | x86 entry ABI version, currently 1 |
| 20 | CR0 |
| 21 | EFER |
| 22 | kernel GS base |
| 23–24 | GDT virtual base and limit |
| 25–26 | IDT virtual base and limit |

Before BSP entry the VMM sets RIP/RSP, RFLAGS=2, CR0/3/4 and EFER; CS is
64-bit ring-0 selector 0x10, data/stack selectors 0x18, GS base is word 22,
GDT/IDT come from the page. Ordinary syscall MSRs and initial FPU are configured;
the kernel's saved callee-saved registers/return RIP and hibernation processor
restore complete the continuation. APs start at KVM reset state awaiting INIT/SIPI.
The guest uses `save_processor_state`/`restore_processor_state` and FPU bracketing,
not a host XSAVE/device-state snapshot.

## Template file

A template contains the whole guest's memory, including tenant data, credentials,
kernel secrets and random-generator state. Treat it as sensitive tenant data,
not as a boot image. Export creates a new file with owner-only permissions
(0600, further restricted by the caller's umask) at the caller-supplied path.
The caller must name a directory it owns and controls, prevent untrusted writes
to that directory, and apply the tenant's retention and deletion policy. Never
import a template from an untrusted writer. Templates must remain within one
trust domain: siblings retain the source kernel address layout and secrets.

Page 0 is a little-endian header: words 0..6 are magic bytes `KRUNFRZ1`,
version 2, control GPA, vCPU count, configured RAM MiB, extent count, and KVM
clock nanoseconds. Word 7 is topology-manifest length (at most4096 bytes);
words 8 onward are `(GPA, byte length, file offset)` triples, up to128 extents.
Page1 contains the topology bytes. Extents are page-aligned, ordered,
non-overlapping and bounded by file length. RAM starts at file offset8192.
No active virtqueues, DAX content or backend state are serialized. Child RAM
writes cannot modify the source or other clones.

## Real-KVM tests

Build the maintained shared kernel series plus patches 0100–0102 in holodeck.
0100 contains the generic PM core, 0101 the x86 context/port hooks, and 0102
protects completed getrandom results still pending in syscall-exit work.
Build a standalone initramfs containing `tests/cooperative/devices.c` as `/init`:

```
nix build --impure --file tests/cooperative/image.nix --arg holodeck /path/to/holodeck -o cooperative-initramfs
```

Run in the fork's Nix environment with Python and e2fsprogs on PATH. The launcher
uses the external ELF kernel/initramfs API; no bundled firmware is needed:

```
just cooperative-clone /path/to/vmlinux cooperative-initramfs/initramfs.cpio CONTROL_GPA_HEX NEW_RESULT_DIR
```

Append `20 16 --passed-fd` to exercise one sealed memfd shared by all child VMMs
instead of path imports. Path mode explicitly starts independent processes after
the capturing VMM has exited, covering persistent templates across daemon restart.
The fd test imports each source file once into a sealed memfd and retains that
same descriptor through all concurrent children; each maps it privately without
copying its RAM.

Defaults are 20 consecutive source-freeze/new-process-start cycles at each of
1/4/8 CPUs, then 16 new VMMs from one stopped template. Each child has an
independent ext4 disk, continuous writes across freeze, record readback,
old-vsock failure/new echo, XMM15 preservation, monotonic/realtime checks,
11 seconds idle for private-RSS sampling, orderly unmount/poweroff and read-only
fsck. An independent ptraced process is held at a completed getrandom syscall
exit through freeze: each child must return different bytes and observe the
pollable resume event. Fresh getrandom output and host generation identifiers
must also differ across all children. A non-root trigger attempt must fail and
the VMGENID platform device must have a driver bound. Closed-policy clones must
be refused while children configured for their own mapped echo destination must
reach it; Internet forwarding remains disabled against a live host TCP listener.
Cached userspace entropy/DRBG state is not rewritten by the kernel: agents must
use the event to invalidate/restart dependent application state.
Any failed assertion stops the matrix; no automatic retries.
Logs and measurements.json remain in the result directory. Freeze timing is
host receipt of the flushed guest trigger marker to the synchronous frozen exit;
resume timing is process invocation to the userspace marker, including file import.
Private RSS is Private_Clean + Private_Dirty in smaps_rollup at ten seconds idle.

For the separate filesystem matrix, build a combined kernel with 0100–0102 plus
0200/0201 using `tests/cooperative/fs-kernel.nix`, and build the initramfs with
`--argstr workload fs`. Then run:

```
nix develop --command just cooperative-fs VMLINUX FS_INITRAMFS NEW_RESULT_DIR
```

Defaults are20 new VMM processes from each of a plain and a DAX template. The
fixture creates fresh read-only `/workspace` and writable `/scratch` servers for
each process with stable handles. It checks open descriptors, mmap, a background
reader and an inotify watch across the actual freeze/capture/new-process restore;
both fresh server INIT handshakes are required and mismatched DAX layouts must
be rejected. This is a protocol fixture, not the production content server.
Observed results and limitations are in [cooperative-clone-results.md](cooperative-clone-results.md).
