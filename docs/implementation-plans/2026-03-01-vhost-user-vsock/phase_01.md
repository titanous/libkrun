# Vhost-User-VSock Implementation Plan - Phase 1

**Goal:** Enable VhostUserDevice to accept a pre-connected UnixStream, supporting fd-provisioned connections.

**Architecture:** Extract the feature negotiation and construction logic from `VhostUserDevice::new()` into a shared helper method, then add `from_stream()` as a second constructor that accepts an already-connected `UnixStream`. Both `new()` and `from_stream()` converge on the same negotiation path.

**Tech Stack:** Rust, vendored vhost crate (0.15 with patches), `std::os::unix::net::UnixStream`

**Scope:** 1 of 6 phases from original design

**Codebase verified:** 2026-03-01

---

## Acceptance Criteria Coverage

This phase is infrastructure — it enables AC1.2 but does not directly test it at the VhostUserVsock level.

**Verifies: None** (infrastructure phase; operational verification only)

---

<!-- START_TASK_1 -->
### Task 1: Extract shared negotiation helper and add from_stream constructor

**Files:**
- Modify: `src/devices/src/virtio/vhost_user/device.rs` (lines 97-220)

**Implementation:**

Refactor `VhostUserDevice::new()` to extract the post-connection logic into a private helper, then add `from_stream()`.

The helper method `negotiate_and_build` takes a `UnixStream` plus the same device parameters as `new()` and performs all feature negotiation and struct construction. Both `new()` and `from_stream()` call this helper.

Add the following to `impl VhostUserDevice` (the first impl block, before `activate_vhost_user`):

```rust
/// Create a new vhost-user device from a pre-connected UnixStream.
///
/// This supports the fd-provisioned connection model where the orchestrator
/// establishes the Unix socket connection before passing the fd to libkrun.
///
/// # Arguments
///
/// * `stream` - A pre-connected UnixStream to the vhost-user backend
/// * `device_type` - Virtio device type ID
/// * `device_name` - Human-readable device name for logging
/// * `num_queues` - Number of queues (0 = query backend via MQ protocol)
/// * `queue_sizes` - Size for each queue (empty = use default 256)
pub fn from_stream(
    stream: UnixStream,
    device_type: u32,
    device_name: String,
    num_queues: u16,
    queue_sizes: &[u16],
) -> IoResult<Self> {
    debug!(
        "Creating vhost-user device from pre-connected stream for {}",
        device_name
    );
    Self::negotiate_and_build(stream, device_type, device_name, num_queues, queue_sizes)
}
```

Refactor `new()` to delegate to the helper:

```rust
pub fn new(
    socket_path: &str,
    device_type: u32,
    device_name: String,
    num_queues: u16,
    queue_sizes: &[u16],
) -> IoResult<Self> {
    debug!("Connecting to vhost-user backend at {}", socket_path);
    let stream = UnixStream::connect(socket_path)?;
    Self::negotiate_and_build(stream, device_type, device_name, num_queues, queue_sizes)
}
```

Add the private helper (contains all the existing negotiation logic from `new()`, starting from `let mut frontend = Frontend::from_stream(stream, 1);` through to `Ok(VhostUserDevice { ... })`):

```rust
/// Shared construction logic: negotiate features with backend and build device.
fn negotiate_and_build(
    stream: UnixStream,
    device_type: u32,
    device_name: String,
    num_queues: u16,
    queue_sizes: &[u16],
) -> IoResult<Self> {
    let mut frontend = Frontend::from_stream(stream, 1);

    // Get available features from backend
    let avail_features = frontend.get_features().map_err(io::Error::other)?;

    debug!("{}: backend features: 0x{:x}", device_name, avail_features);

    // VHOST_USER_F_PROTOCOL_FEATURES (bit 30) is a backend-only feature
    // that enables vhost-user protocol extensions. It's not a virtio feature,
    // so we don't expose it to the guest, but we always use it with the backend.
    const VHOST_USER_F_PROTOCOL_FEATURES: u64 = 1 << 30;

    // Separate backend-only features from virtio features
    let backend_features = avail_features & VHOST_USER_F_PROTOCOL_FEATURES;
    let our_avail_features = avail_features & !VHOST_USER_F_PROTOCOL_FEATURES;

    // Determine actual queue count - may require protocol feature negotiation
    let acked_protocol_features = if backend_features & VHOST_USER_F_PROTOCOL_FEATURES != 0 {
        frontend
            .set_features(backend_features)
            .map_err(io::Error::other)?;

        let protocol_features = frontend.get_protocol_features().map_err(io::Error::other)?;

        let mut our_protocol_features = VhostUserProtocolFeatures::empty();
        if protocol_features.contains(VhostUserProtocolFeatures::CONFIG) {
            our_protocol_features |= VhostUserProtocolFeatures::CONFIG;
        }
        if protocol_features.contains(VhostUserProtocolFeatures::MQ) {
            our_protocol_features |= VhostUserProtocolFeatures::MQ;
        }
        if protocol_features.contains(VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS) {
            our_protocol_features |= VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS;
        }
        if protocol_features.contains(VhostUserProtocolFeatures::DEVICE_STATE) {
            our_protocol_features |= VhostUserProtocolFeatures::DEVICE_STATE;
        }

        frontend
            .set_protocol_features(our_protocol_features)
            .map_err(io::Error::other)?;

        our_protocol_features
    } else {
        VhostUserProtocolFeatures::empty()
    };

    let actual_num_queues = if num_queues == 0 {
        if backend_features & VHOST_USER_F_PROTOCOL_FEATURES != 0 {
            let backend_queue_num = frontend.get_queue_num().map_err(io::Error::other)?;

            debug!(
                "{}: backend reports {} queues available",
                device_name, backend_queue_num
            );

            backend_queue_num as usize
        } else {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "Backend doesn't support protocol features, must specify queue count",
            ));
        }
    } else {
        num_queues as usize
    };

    debug!(
        "{}: using {} queues (requested: {}, sizes provided: {})",
        device_name,
        actual_num_queues,
        num_queues,
        queue_sizes.len()
    );

    let default_size = queue_sizes.last().copied().unwrap_or(256);
    let queue_configs: Vec<_> = (0..actual_num_queues)
        .map(|i| {
            let size = queue_sizes.get(i).copied().unwrap_or(default_size);
            QueueConfig::new(size)
        })
        .collect();

    let device_state_supported =
        acked_protocol_features.contains(VhostUserProtocolFeatures::DEVICE_STATE);

    Ok(VhostUserDevice {
        frontend: Arc::new(Mutex::new(frontend)),
        device_type,
        device_name,
        queue_configs,
        avail_features: our_avail_features,
        backend_features,
        acked_features: 0,
        acked_protocol_features,
        device_state_supported,
        device_state: DeviceState::Inactive,
    })
}
```

**Verification:**

Build with vhost-user feature to confirm compilation:
```bash
cd /home/titanous/vm-platform/libkrun/.worktrees/vhost-user-vsock && cargo check -p devices --features vhost-user
```
Expected: compiles without errors.

Run existing unit tests to confirm no regressions:
```bash
cargo test -p devices --features vhost-user
```
Expected: all existing tests pass.

**Commit:** `feat(vhost-user): add VhostUserDevice::from_stream constructor`
<!-- END_TASK_1 -->
