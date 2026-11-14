# libkrun platform fork

This fork provides a Rust library for configuring and running lightweight Linux virtual machines with KVM. The public crate is `krun`, built from `src/libkrun`; its API consists of `Builder`, `Context`, `VmHandle`, and `VmExit`. It is not a C shared library.

The platform targets Linux KVM and a separate Nitro backend being developed alongside it. Virtio devices are shared by the backends. Guest cloning uses guest-kernel cooperation and fresh device backends, not serialized VMM state.

## Build and test

Use the pinned Nix development environment on x86-64 Linux:

```sh
export CARGO_BUILD_JOBS=12
nix develop
just build-init
just check
just test
just build
just integration all 4
```

`just check` runs format checks and clippy with warnings denied. `just test` runs device and VMM unit tests. `just integration` builds the host runner and static guest agent, then boots real guests; it requires accessible `/dev/kvm`. The development shell provides the guest firmware and creates the `test-prefix/lib64` firmware links. Build init before checking a fresh checkout because `embedded_init` embeds `init/init`.

## Rust API

Configure a `Builder` with a vCPU count and memory size, guest filesystems or disks, command line, and I/O devices. `Builder::build()` returns a `Context`. Obtain a `VmHandle` before `Context::run()` to control pause/resume and the optional memory balloon. `run()` returns a `VmExit` without terminating the host process. Each guest is normally run in its own supervised VMM process.

Firmware is dynamically loaded by default. `static-firmware` supports static launchers, and `embedded_init` embeds the static guest init binary. `net`, `blk`, and `vhost-user` enable their respective device backends; `efi`, `amd-sev`, and `tdx` retain their Linux boot/confidential-guest paths.

## Devices and guest I/O

- Virtio console with named ports and host file descriptors.
- Virtio block with synchronous or asynchronous backends.
- Virtio network with asynchronous backends, or socket proxying over virtio-vsock with Transparent Socket Impersonation.
- Virtio-fs over a generic `FileSystem` implementation, including passthrough and DAX mapping; external vhost-user filesystem and vsock daemons are supported.
- Virtio balloon inflation, deflation, statistics and free-page reporting, plus virtio RNG and RTC.
- VM generation ID on x86-64 and aarch64. After starting a copied guest, call `VmHandle::notify_vm_generation_change()` to write a fresh identifier and raise its interrupt; the guest driver reseeds its random pool.

Cross-build and lint Linux arm64 with `CARGO_BUILD_JOBS=12 nix develop --max-jobs 12 -c just check-aarch64`; both stages deny warnings.

The fork carries guest-kernel patches under `libkrunfw-patches`. `src/aws_nitro` is a separate existing crate and is retained independently of KVM.

## Security boundary

The host is trusted. The VMM proxies guest I/O using its own host credentials and filesystem/network access. Use host process isolation, filesystem visibility controls, and egress policy appropriate to the workload. Device backends are not yet hardened against hostile tenants.

See `CLAUDE.md` and component instruction files for code ownership, build features, and runtime invariants.
