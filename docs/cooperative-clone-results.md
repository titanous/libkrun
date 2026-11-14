# Observed x86-64 KVM cooperative clone results

Hardware: shared Linux x86-64 AMD Threadripper host, real `/dev/kvm`, 256 MiB
per guest. Guest source is Amazon Linux 6.18.62 with shared patches 0001–0007
and this lane's generic 0100, x86 0101 and pending-getrandom 0102. Balloon/page
reporting disabled. VMM based on `nitro` 2e9af53.

## Acceptance

`clone-kvm-combined-acceptance` completed 20 consecutive freeze/new-process
cycles at each of 1, 4 and 8 CPUs, then 16 concurrent new 8-CPU VMM processes
from one stopped template. All 76 full-workload children passed; all 76 disk
images passed read-only e2fsck after orderly unmount/poweroff. The source VMM
exited before any path-mode child started. There were no automatic retries.

Checks: continuous ext4 writes/readback across freeze, dead old vsock connection,
fresh guest-to-host echo, console, entropy, XMM15 preservation, AP count,
monotonic/realtime, distinct host generation IDs, distinct fresh guest random
bytes and distinct results from an independent getrandom task held at syscall
exit across freeze. Root-only trigger and VMGENID driver binding were checked
in the source; the independent reader also observed the sysfs resume event.
Every accepted child must show a delivered VMGENID interrupt, not just binding.
An earlier reviewed matrix's worst realtime error was245874 ns (0.246 ms).

`clone-kvm-fs-just` passed20 plain and20 DAX real-copy children using the combined
kernel (0100–0102 and filesystem0200/0201). Retained open descriptors, mmap,
background reads and an inotify watch passed against fresh servers; each child
required both INIT handshakes, and incompatible DAX topology was rejected.
VMM implementation commit: b1163d35. This tests a stable-handle fixture, not the
production content server. `clone-kvm-combined-fd` also passed1/4/8 CPU cycles
plus16 concurrent children sharing one sealed memfd with the same final build.

Additional `clone-kvm-forwarding-fd` run passed one cycle at each CPU count plus
16 concurrent same-template children, using one sealed memfd shared across
processes. Default-closed new guests were refused; permitted children reached
their configured echo destination after RESTORE. Internet forwarding disabled
was checked against a live host TCP listener while the connection rule allowed
TCP. No connection reached that listener.

## Measurements (20 samples per CPU count)

| CPUs | Freeze ms median / worst | Start-to-userspace ms median / worst | Private KiB at 10 s median / worst |
|---|---:|---:|---:|
| 1 | 63.973 / 87.228 | 1776.198 / 2112.543 | 27296 / 27712 |
| 4 | 91.204 / 123.162 | 1479.040 / 2156.605 | 30316 / 31116 |
| 8 | 97.573 / 137.962 | 1506.952 / 2302.450 | 34160 / 35036 |

Freeze is host receipt of the flushed trigger marker to the synchronous frozen
exit (not RAM export time). Start-to-userspace includes process startup and
path import into a sealed memfd, and was sampled at 5-ms host polling intervals.
Private memory is whole-VMM `smaps_rollup` Private_Clean + Private_Dirty after
10 seconds idle, including VMM overhead, not a guest-RAM-only PSS measurement.
The source template is never run again. Passed-fd children map the same immutable
file privately without a full RAM copy; path imports intentionally stabilize a
persistent file into a sealed memfd before mapping it.

## Failure investigations and limits

- Initial HIBERNATION omission (SWAP disabled) detected by missing symbols; config
  corrected before accepting a kernel artifact.
- Kernel console FREEZE deadlock fixed with standard console suspend ordering.
- Unbound virtio-rtc pre-DRIVER_OK status required checking actual backend/queue
  quiescence rather than status equal to zero.
- Monotonic underflow fixed by preserving KVM clock nanoseconds.
- An earlier full matrix stopped at 8-CPU cycle 8 on stale realtime (>100 ms).
  Host time is now synchronously sampled by the kernel after syscore resume,
  before user thaw; the complete reviewed matrix passed the unchanged bound.
- A first concurrent-reader test compared already-returned userspace cached
  bytes and failed. The deterministic syscall-exit test exposes the actual
  pending-result case; a VM-fork generation check regenerates that result before
  returning to userspace. Userspace DRBG/cached buffers still require agents to
  invalidate/restart application state using the resume event.
- VMGENID pre-entry edge was lost during IOAPIC restore despite driver binding.
  One-shot re-notification on the first post-syscore time-port read fixes delivery;
  explicit guest interrupt-count checks passed the final full matrix.
- Initial DAX capture faulted after FREEZE revoked host mappings. DAX windows
  are now excluded from RAM capture and recreated empty at identical GPAs.
  Both plain/DAX40-child acceptance and topology negatives passed afterwards.
- A combined-matrix invocation lacked mkfs.ext4 before any guest boot; the Nix
  devShell now includes e2fsprogs/Python. Another combined run hit the shell's
  default600-second budget (not a guest assertion); partial logs were preserved.
- Same-host trusted-template contract only; caller supplies independent disks
  and filesystem identity/handle semantics. Device manifest validation exists;
  full CPUID/build-ID fencing and cross-host/TSC migration are not claimed.
- FPU proof is XMM15 plus standard kernel processor-state restore, not exhaustive
  AVX/AVX512 sentinels. Per-CPU pv MSRs and interrupt/timer handling are exercised
  by AP hotplug, I/O, timer-driven writes, idle and shutdown; no exhaustive MSR
  value equivalence test is claimed.

Raw logs and measurements are under
`/work/.nitro-sandboxes-ref/build/clone-kvm-combined-acceptance/`,
`clone-kvm-fs-just/` and `clone-kvm-forwarding-fd/` under that same build root.
Final combined kernel store hash: dvbrcmmn4f1p4xb938xbml5bzaz089v6;
device initramfs:7y564g7gwj7wdrg8ghn65ik6y1hvjdrg; filesystem initramfs:
bhk7ap8ljs8g4jf05lsi05pcjsdf2y0g. Filesystem patches are consumed read-only
from the kernel lane via `tests/cooperative/fs-kernel.nix`.
Reproduction and ABI are in [cooperative-clone.md](cooperative-clone.md).
