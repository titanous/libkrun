# Cooperative copying on AWS Nitro Sandboxes (arm64)

The shared contract in `cooperative-clone.md` (template format 4, the control
page, `CaptureOutcome`, `TemplateMarkerMissing`) applies unchanged. This page
lists what is specific to the Nitro arm64 backend and every place where it
differs from KVM. On Nitro the guest's memory cannot be mapped copy-on-write
into another VM, so every copy receives a full private copy of the template's
memory, written into a fresh VM that has booted a small parked stub.

## The kernel marker

- A guest may become a template only if it booted with `cooperative_template`
  on its kernel command line. For an `external_kernel` image,
  `Builder::future_template()` inserts the flag (before any `--` separator). A
  prebuilt EIF must already carry it; the builder does not rewrite an EIF.
- The marked arm64 kernel (holodeck patch 0123) publishes word 32 = 1 in its
  control page and prints one console line, `NITRO_TEMPLATE_MARKER active=1`.
  The host accepts that notification once, checks word 32 in guest-owned
  memory, and only then confirms the marker on the capture request.
- `/sys/power/nitro_freeze` exists only in marked guests. An ordinary guest
  has no trigger file at all.

## Who arms and in what order

1. The host daemon quiesces the workload through the guest agent.
2. The daemon calls `VmHandle::arm_template_capture(timeout)`. It is refused
   with `TemplateMarkerMissing` until the boot marker is confirmed.
3. The daemon tells the agent to write the trigger (agent `Freeze` message).
   The kernel freezes tasks, resets devices, and publishes the frozen control
   page.
4. The runtime validates the frozen state, captures guest-owned memory into a
   sealed descriptor, closes the source VM under the lifecycle lock, and only
   then delivers the template: `VmHandle::wait_template_capture()` returns the
   `CaptureOutcome`. The source is gone after an accepted capture.

## What is refused

| Condition | Result |
| --- | --- |
| Arm or capture without a confirmed marker | `TemplateMarkerMissing` (`PermissionDenied`); nothing is frozen |
| Trigger written while not armed, or after the arm deadline | Source resumes in place (resume path 1); no template exists; `wait_template_capture` errors (`TimedOut` after an expired arm) |
| A device not reset at freeze, invalid control page, or memory outside the boot-reported ranges | Capture refused before the irreversible step; source resumes in place |
| Console devices or virtio-fs DAX windows on a template or copy | Refused at build |
| Block device without `template_disk_identity` | Refused at build |
| Template with another architecture tag, or format 3 | Refused when opened |
| CPU count, memory size, or device list differing from the template | Refused before the VM is created (`start_from_template`) or at `TemplateAssignment::assign` |
| Receiver kernel build ID differing from the template's | Refused before any memory is written to the receiver |

## Receiving VMs and the parked receiver

A receiving VM boots the same kernel Image with a parked stub injected
(`src/nitro_sandbox/examples/receiver.S` and `receiver-image.py`). The stub
parks in the reserved range `[0x70000000, 0x70200000)`, publishes READY and
its own kernel build ID, and waits. The host then validates, copies the
template's memory privately (skipping the reservation), writes fresh entropy
and host time, and releases the stub, which replays interrupt-controller state
and enters the frozen kernel's continuation. The kernel refreshes time and
randomness before thawing tasks (holodeck patch 0122). Device backends are new
and idle; the guest re-probes them. Existing vsock connections are reset
(reads fail with `ENOTCONN`); the agent reconnects with a new generation.

Two ways to start a receiving VM:

- `Builder::start_from_template(source, ...)` chooses the template before the
  VM exists. The VM boots, parks, and is released immediately.
- `Builder::park_for_template(wait)` builds the VM with no template. After
  `build()`, `Context::take_template_assignment()` returns a one-shot
  `TemplateAssignment`. `receiver_ready()` reports that the stub has
  published READY. `assign(source)` checks the template against the
  receiver's CPU count, memory and device list, then hands it over; the
  runtime picks it up through a wake descriptor and validates the receiver's
  kernel build ID before copying.

Parked receiver contract:

- `wait` counts from start and bounds both assignment and release. On expiry
  `run` tears the VM down through the normal path (under the configured
  lifecycle lock) and returns a `TimedOut` error carrying
  `ParkedReceiverError::Expired`.
- Dropping every `TemplateAssignment` while the receiver waits ends it the
  same way with `ConnectionAborted` / `ParkedReceiverError::Abandoned`. A
  stop request ends it with `VmExit` stopped.
- A second `assign`, or an `assign` after expiry or end, fails with
  `ParkedReceiverError::Closed`.
- The boot watchdog is suspended while parked and restarts at release.
- Configure the parked VM with the same device list, in the same order, as
  the template's source. Backends may receive their contents (for example a
  disk snapshot written in place into the already-open file) at any time
  before `assign`, because the guest does not touch them until release.
- Parking cannot be combined with capture or with `start_from_template`.

## Limits

- Memory 1 GiB to 4 GiB per guest; CPU counts 1, 4 and 8 were tested.
- At most 256 interrupt-controller register writes are replayed.
- Copying is serial through a 1 MiB buffer (about 60–70 ms per GiB measured);
  receiver boot to READY is about 213 ms; both are paid per copy unless the
  receiver is parked in advance.
- Every final close of a Nitro VM descriptor, including receivers that
  expire, are dropped, or fail to start, happens under the one host-wide
  lifecycle lock (`nitro_sandbox::lifecycle::LIFECYCLE_LOCK_PATH`).
- The guest kernel must carry holodeck patches 0120–0123. The consolidated
  series without them cannot produce or receive copies on arm64.

## What the host daemon must do on this backend

- Boot templates with `cooperative_template` and pass sealed descriptors only;
  one trust domain per template.
- Quiesce through the agent, arm, then send `Freeze`; keep the agent session
  open until the source closes.
- Start receiving VMs from the same kernel Image with the parked stub
  injected for that Image's build ID, the same CPU count, memory and device
  list; give each copy its own disk snapshot and filesystem tree and a new
  CID. Park them ahead of demand with `park_for_template` to hide receiver
  boot; bound each park with `wait` and reap on expiry.
- Accept a copy as ready only after the agent reconnects with the expected
  boot ID and token and acknowledges the new generation.

Differences from KVM: memory is copied, not mapped; the receiver must boot a
parked stub of the identical kernel; interrupt-controller state is replayed by
the stub; capture always closes the source VM; receivers can be parked before
a template exists; DAX is refused.

## Open items

- Kernel freeze time and time to the first resumed user instruction are not
  measured in isolation. What is measured are upper bounds: freeze request to
  sealed template (includes the freeze, the capture and source teardown) and
  release to the agent's Hello (includes task thaw, agent reconnect and one
  protocol message). This does not affect correctness, and the latency a
  daemon sees (assignment to agent Hello) is measured directly; it matters
  only for deciding where to optimize. Measuring it needs the arm64 kernel to
  record the virtual counter at freeze entry, tasks frozen, devices reset and
  publication, and at resume entry and the first user task scheduled, into
  spare control-page words (a patch in the 0120–0129 range), plus host
  parsing. Only differences within one VM are meaningful, because the counter
  offset between VMs is not known to the host.
- Memory import is serial through one buffer; a parallel import would cut the
  roughly 250 ms spent on a 4 GiB copy.
- The consolidated kernel series does not yet include patches 0120–0123.
- Not tested: four parked 4 GiB receivers at once (host limits), parked
  receivers with 4 or 8 CPUs, more than one block and one filesystem device.
- The image loader accepted a 4 KiB image of zeros; that receiver booted,
  never published READY and was removed when its park wait expired. An early
  image check would fail such receivers sooner.
- The holodeck daemon does not yet hold parked receivers; this page describes
  what it must do.
