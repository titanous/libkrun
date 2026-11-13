# Nitro Sandbox host backend

This crate implements host-side Nitro Sandbox protocol support, not the unrelated
`aws_nitro` enclave workload support. It remains dependency-free so info-page and
lifecycle tests run without a Nitro device or KVM.

Info-page parsing operates on copied bytes. A live mapping reader must use volatile
reads and acquire fences, retrying when either generation or reset count changes;
a borrowed Rust slice over concurrently hypervisor-written memory is not sufficient.
Lifecycle actions are instructions to the caller: Stop must synchronously stop all
queue access before a new Start or VM-fd close. Never reuse ring indices after reset.
Watchdog deadlines use monotonic elapsed time, not wall-clock timestamps.

Run `cargo test --manifest-path src/nitro_sandbox/Cargo.toml` from the fork root.
