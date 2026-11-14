# Integration tests

Separate host/guest Cargo workspace with its own lockfile. Use `nix develop` and `just integration [name] [jobs]` from the fork root. Tests boot real KVM guests; `/dev/kvm`, static guest agent/init and libkrunfw are required. The Nix shell supplies firmware links under test-prefix.

The registry in test_cases/src/lib.rs contains runtime VM configuration, vsock/TSI, multiport console, Rust API lifecycle, exit observers, custom/error/slow block backends, async network loopback, external vhost-user vsock echo/fd, generic/minimal filesystem, three vhost-user filesystem DAX modes and in-process DAX read/write/read-back, generation-change kernel reseed notifications, balloon inflation/stats, boot timing and nested virtualization cases.

Host/guest features are mutually exclusive. Macros split the two halves; helpers in krun_rust.rs configure Rust Builder guests. Each case name is unique. Preserve bounded guest waits and assertions rather than suppressing failures. Nested virtualization requires host support and may report its own skip.

Runtime daemons use patched vhost-user-backend/virtio-queue with vm-memory 0.18. They have separate processes and must be reaped by their owning test. mem_block_backend.rs, failing_block_backend.rs, slow_block_backend.rs, minimal_filesystem.rs and loopback_net.rs provide deterministic runtime backends.
