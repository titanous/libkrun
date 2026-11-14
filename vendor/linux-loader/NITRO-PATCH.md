# Local compatibility patch

Source: https://github.com/theoparis/linux-loader at
af90928dcfa1666d47a5269061ef77857eff46d3 (the fork's prior Cargo patch revision).

`src/configurator/fdt.rs` uses `GuestMemory::check_range` with Write permissions
instead of the removed `checked_offset` trait method. This allows the existing
arm64 FDT configurator to compile against vm-memory 0.18. The out-of-RAM error
remains FDTPastRamEnd. No Nitro-specific boot behavior is added to this dependency.
The vendored build-only copy includes source/manifests/licenses rather than upstream
CI and benchmark assets; absent README and benchmark declarations are removed.
