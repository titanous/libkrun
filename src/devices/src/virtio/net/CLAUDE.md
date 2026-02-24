# Virtio Net

Last verified: 2026-02-24

## Purpose
Implements virtio-net device with multiple backend strategies: synchronous (tap, unix) and async (tokio).

## Contracts
- **Exposes**: `Net` virtio device, `VirtioNetBackend` enum (Tap, UnixGram, UnixStream, CustomAsyncFactory), `AsyncNetBackend` trait
- **Guarantees**:
  - `VirtioNetBackend::CustomAsyncFactory(factory)` activates an `AsyncNetWorker` that bridges virtio queues to a user-supplied `AsyncNetBackend`
  - `InterruptTransport::status_arc()` provides `Arc<AtomicUsize>` for workers needing shared interrupt status
  - Sync backends use `NetWorker`; async backends use `AsyncNetWorker`
- **Expects**: Valid virtio queues and guest memory from MMIO activation

## Dependencies
- **Uses**: `tokio` (async worker), `bytes` (packet buffers), `vm-memory`
- **Used by**: `libkrun` (configures backend via `VirtioNetBackend` enum)
- **Boundary**: `tokio` and `bytes` deps gated behind `net` feature flag

## Key Decisions
- `NetWorker::new` panics if given `CustomAsyncFactory` variant (wrong worker type)

## Invariants
- `NetWorker::new` panics on `CustomAsyncFactory` (must use `AsyncNetWorker` instead)

## Key Files
- `device.rs` - `Net` virtio device, `VirtioNetBackend` enum, activation logic
- `async_backend.rs` - `AsyncNetBackend` trait, `AsyncNetBackendFactory` trait, `NetBackendHandle`
- `async_worker.rs` - Tokio-based async net worker
- `worker.rs` - Synchronous net worker (tap/unix backends)
- `mod.rs` - Module declarations and re-exports
