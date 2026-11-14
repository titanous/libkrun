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
3. Root writes `1` to `/sys/power/nitro_freeze` (0600/CAP_SYS_ADMIN; an active
   transition is refused). The generic core checks hibernation availability,
   runs PM prepare notifiers, syncs and freezes configured filesystems, locks
   device hotplug and restricts GFP allocations during device freeze. It freezes tasks,
   suspends consoles, invokes device FREEZE callbacks, offlines secondary CPUs,
   disables interrupts and suspends syscore devices. It saves processor context,
   publishes the page, then writes byte 1 to I/O port `0xe5`.
4. Only BSP may emit that exit. The VMM parks it terminally, stops/joins all vCPU
   threads, checks virtio transports have no active backend or DRIVER_OK queues,
   validates the page, saves KVM clock nanoseconds and copies RAM extents into a
   sealed memfd. It exports a create-new disk image and returns
   `VmExit::Frozen { template_path }`. Failed validation never publishes a template.
5. Configure the same vCPUs, RAM size and devices, using independent writable
   disk children and fresh vsock/console backends. Call
   `start_from_template_fd(sealed_file)` or `start_from_template_path(path)`.
   Path import copies into a sealed memfd to exclude later file mutations;
   passed descriptors must already have write/grow/shrink seals.
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
   `0xe7` returns high 32 bits. A high-low-high read repeats on rollover. This
   avoids treating AP/device restoration time as a fixed wall-clock offset; the
   control-page realtime word remains the resume-time validity flag. Kernels with
   patch 0102 require these ports; older VMMs are incompatible with that patch.

The current caller supplies the same device order and features. RAM/vCPU count,
control-page version and long-mode prerequisites are checked. This format is
same-host/same-kernel only, not a portable or hostile-input migration format.
Full CPUID/kernel-build/device-manifest compatibility fencing is required before
accepting templates from outside the supervising daemon's trusted artifact set.
Virtio-fs/DAX and GPU mappings are not supported by this path.

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

Page 0 is a little-endian header: words 0..6 are magic bytes `KRUNFRZ1`,
version 1, control GPA, vCPU count, configured RAM MiB, extent count, and KVM
clock nanoseconds. Words 8 onward are `(GPA, byte length, file offset)` triples,
up to 128 extents. Extents are page-aligned, ordered, non-overlapping and bounded
by file length. RAM starts at file offset 4096. No active virtqueues or backend
state are serialized. Child RAM writes cannot modify the source or other clones.

## Real-KVM tests

Build the maintained shared kernel series plus patches 0100/0101 in holodeck.
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
fsck. Any failed assertion stops the matrix; no automatic retries.
Logs and measurements.json remain in the result directory. Freeze timing is
host receipt of the flushed guest trigger marker to the synchronous frozen exit;
resume timing is process invocation to the userspace marker, including file import.
Private RSS is Private_Clean + Private_Dirty in smaps_rollup at ten seconds idle.
