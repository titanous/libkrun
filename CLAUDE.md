# libkrun platform fork

## Tooling
- Rust workspace built through the justfile inside `nix develop`.
- Set `CARGO_BUILD_JOBS=12` (maximum per lane).
- Fresh checkout: `just build-init` before `just check`.
- `just check`: init and workspace format checks, clippy with `-D warnings`.
- `just test`: devices and VMM unit tests, including block/net/vhost-user.
- `just build`: static init and release Rust library.
- `just integration [name] [jobs]`: real KVM guest tests; requires `/dev/kvm` and firmware provided by the Nix shell.
- Additional pure-logic/property/concurrency/fuzz/formal/mutation recipes live in the justfile.

## Structure and contracts
- `src/libkrun`: public Rust `Builder`, `Context`, `VmHandle`, `VmExit` and balloon API; crate type `lib` only.
- `src/vmm`: Linux KVM construction, memory registration, vCPU threads, device managers and exit dispatch.
- `src/devices`: shared virtio and legacy emulation. Device activation transfers `DeviceQueue` ownership; reset recreates queues through the transport.
- `src/arch`, `src/kernel`, `src/cpuid`: boot layout, kernel loading and CPU configuration.
- `src/aws_nitro`: independent existing Nitro crate; do not remove as part of KVM changes.
- `patches`: carried guest-kernel changes.
- `init`: separate Rust workspace producing static musl guest init.
- `tests`: separate host/guest integration workspace, with its own Cargo.lock.
- `fuzz`: separate workspace with FUSE parsing, block request, descriptor chain and vhost-user message harnesses.

Normal startup creates paused vCPU threads and resumes them. Public pause/resume remain lifecycle operations. `Context::run()` returns a `VmExit`; it must not exit the host process. vCPU handle teardown signals and joins threads. Keep VM generation ID and GUID-update support for guest copy notification.

Balloon reclaimed-page tracking remains part of normal device processing. Kernel-bundle configuration queries the host page size for alignment. DAX regions use additional KVM slots; vhost-user guest RAM is memfd-backed for daemon sharing.

## Features
- `embedded_init`: embed `init/init` in devices.
- `net`, `blk`: network/block backends; VMM features forward to the devices crate.
- `vhost-user`: external filesystem/vsock daemons.
- `efi`: firmware boot, implies block and network.
- `amd-sev`, `tdx`: Linux confidential-guest support.
- `dynamic-firmware` (default) / `static-firmware`: firmware loading/linking modes.
- `gpu`, `snd`, `input`: optional device paths.

## Boundaries
The platform uses Linux backends and guest-cooperative cloning with fresh device backends. Device state is not serialized by the VMM. Preserve ordinary reset/shutdown, async backend cleanup, queue notification, and guest copy-notification behavior when removing unused code.

`vendor/vhost`, `vendor/vhost-user-backend`, and `vendor/virtio-queue` contain protocol/compatibility patches used by external device tests; avoid unrelated version updates. Root memory types use vm-memory 0.18. Component instruction files describe finer-grained contracts.
