# Nitro Sandbox host backend

This crate implements host-side Nitro Sandbox protocol support, not the unrelated
`aws_nitro` enclave workload support. Default parser/lifecycle tests require no
Nitro or KVM device. The optional `devices` feature attaches the fork's block,
vsock and console implementations through `Transport`.

Info-page parsing operates on copied bytes. Live reads use volatile copies and
acquire fences with generation/reset retries. Lifecycle Stop must synchronously
stop queue access before Start or VM-fd close. Never reuse ring indices after reset.
Vsock and console also require their existing Polly subscribers to be dispatched;
activation alone does not service all queues. Drive lifecycle refresh before dispatch.

Sandbox closes its VM fd under the embedder-supplied, daemon-created lifecycle
lock before releasing mappings; there is no fixed-path fallback. The daemon keeps
a duplicate VM descriptor after acknowledged SCM_RIGHTS handoff and closes it
under the same lock after reaping the VMM. StartAndTeardown holds one guard from
creation through start. A reset refusal returns an error and retains mappings
still reachable by a worker until the one-guest process exits. Memory exposed by unsafe `guest_memory()` does not own
the driver mapping: drop all devices and memory clones before Sandbox. The first
GiB starts at GPA 0x40000000; backing above it starts at GPA 0x400000000, leaving
the intervening PCI/MMIO hole unmapped.

The crate is a path member of the root workspace. Run `cargo test -p nitro_sandbox`
from the fork root. Captured m8g device entries are in tests/fixtures with provenance.
The boot/reset watchdog disarms only while all registered devices are active.
Shared-region registration is unsafe: callers retain mappings until VM close and
must not concurrently assign them elsewhere; registration zeroes all bytes.
Build device example with `--features devices --example devices`. Guest-side
`examples/guest-smoke.c` must be statically compiled for the guest architecture;
`build-smoke.py` makes a test EIF using a reference EIF kernel. Test success requires
all guest markers plus host echo verification; poweroff alone is not test success.
