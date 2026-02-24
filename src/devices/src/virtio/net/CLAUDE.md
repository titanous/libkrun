# Virtio Net

Last verified: 2026-02-24

## Purpose
Implements virtio-net device with multiple backend strategies: synchronous (tap, unix), async (tokio), and proxy (smoltcp userspace TCP/IP stack).

## Contracts
- **Exposes**: `Net` virtio device, `VirtioNetBackend` enum (Tap, UnixGram, UnixStream, CustomAsyncFactory, Proxy), `AsyncNetBackend` trait, `ProxyNetWorker`
- **Guarantees**:
  - `VirtioNetBackend::Proxy { listeners }` activates a `ProxyNetWorker` that bridges VM virtio queues to host via smoltcp
  - `ProxyNetWorker::get_ephemeral_port()` returns `Result<u16, ProxyError>` (errors on exhaustion instead of infinite loop)
  - `InterruptTransport::status_arc()` provides `Arc<AtomicUsize>` for workers needing shared interrupt status
  - Sync backends use `NetWorker`; async backends use `AsyncNetWorker`; proxy uses `ProxyNetWorker` directly
- **Expects**: Valid virtio queues and guest memory from MMIO activation

## Dependencies
- **Uses**: `smoltcp` (proxy stack), `mio` (proxy I/O), `pnet` (packet parsing), `tokio` (async worker), `vm-memory`
- **Used by**: `libkrun` (configures backend via `VirtioNetBackend` enum)
- **Boundary**: All smoltcp/pnet deps gated behind `net` feature flag

## Key Decisions
- `proxy` module is `pub` (needed by device.rs activation path and integration tests)
- `VirtualDevice` uses `Vec<u8>` buffers instead of stack arrays (prevents stack overflow in tests)
- `handle_unix_listener_event` borrows listener from map instead of removing it (fixes listener drop bug)

## Invariants
- `NetWorker::new` panics if given `Proxy` or `CustomAsyncFactory` variant (wrong worker type)
- Proxy VM IP is `192.168.100.2`, proxy IP is `192.168.100.1` (hardcoded constants)
- `intercept_new_session` only intercepts TCP SYN packets (checks SYN flag)

## Key Files
- `device.rs` - `Net` virtio device, `VirtioNetBackend` enum, activation logic
- `proxy.rs` - `ProxyNetWorker`, smoltcp-based userspace networking
- `async_worker.rs` - Tokio-based async net worker
- `worker.rs` - Synchronous net worker (tap/unix backends)
- `mod.rs` - Module declarations (proxy is pub)
