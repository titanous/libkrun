# Test Coverage Implementation Plan — Phase 8

**Goal:** End-to-end validation of `ProxyNetWorker` (smoltcp-based) as a VM network backend via a new `VirtioNetBackend::Proxy` variant.

**Architecture:** `ProxyNetWorker` does NOT implement `AsyncNetBackend` or `AsyncNetBackendFactory` — it has its own mio-based event loop. To expose it via the Rust Builder API, this phase adds `VirtioNetBackend::Proxy { listeners: Vec<(u16, String)> }` to the `VirtioNetBackend` enum and handles it in `Net::activate()` by spawning `ProxyNetWorker` on a dedicated thread. The integration test `TestNetProxy` registers a host TCP listener, starts a VM with the Proxy backend, and verifies the guest can make a TCP connection through the smoltcp proxy and exchange `"PING"` / `"PONG"` messages.

**Tech Stack:** Rust, `krun` crate, smoltcp (via `ProxyNetWorker`), `std::net::TcpListener/TcpStream`, `macros::{host, guest}`.

**Scope:** Phase 8 of 8 phases

**Dependencies:** Phase 5 (infrastructure), Phase 3 (ProxyNetWorker bug fix). Also requires Phase 7 Task 1 (`Builder::vm_config()` → `Result`) before or alongside this phase, because Phase 8 code uses `builder.vm_config(1, 512)?`.

**Codebase verified:** 2026-02-23

---

## Acceptance Criteria Coverage

This phase implements and tests:

### test-coverage.AC8: Net proxy integration
- **test-coverage.AC8.1 Success:** Guest makes a TCP connection to a host listener through the smoltcp `ProxyNetWorker` backend, exchanges a `"PING"` / `"PONG"` message pair successfully

---

## Codebase Findings (Phase 8 Investigation)

### ProxyNetWorker architecture

`ProxyNetWorker` (`src/devices/src/virtio/net/proxy.rs`) is a **standalone struct with its own mio event loop**, distinct from `AsyncNetWorker`. It does NOT implement `AsyncNetBackend` or `AsyncNetBackendFactory`.

`ProxyNetWorker::new()` requires:
```rust
pub fn new(
    queues: Vec<Queue>,
    queue_evts: Vec<EventFd>,
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: EventFd,
    intc: Option<IrqChip>,
    irq_line: Option<u32>,
    mem: GuestMemoryMmap,
    listeners: Vec<(u16, String)>,  // (vm_port, unix_socket_path) for vsock-like ingress
) -> io::Result<Self>
```

`ProxyNetWorker::run(self)` — blocking, runs the mio event loop in the calling thread.

### Current VirtioNetBackend enum

```rust
pub enum VirtioNetBackend {
    UnixstreamFd(RawFd),
    UnixstreamPath(PathBuf),
    UnixgramFd(RawFd),
    UnixgramPath(PathBuf, bool),
    Tap(String),  // Linux only
    CustomAsyncFactory(Box<dyn AsyncNetBackendFactory>),
}
```

`Net::activate()` handles `CustomAsyncFactory` → spawns `AsyncNetWorker`. All other variants → spawns `NetWorker` (sync path). `ProxyNetWorker` is neither path.

### Net::activate() structure (src/devices/src/virtio/net/device.rs lines 257–323)

```rust
fn activate(&mut self, mem: GuestMemoryMmap, interrupt: InterruptTransport) -> ActivateResult {
    let queue_evts: Vec<EventFd> = self.queue_evts.iter().map(|e| e.try_clone().unwrap()).collect();
    let backend = self.cfg_backend.take().ok_or(ActivateError::BadActivate)?;
    match backend {
        VirtioNetBackend::CustomAsyncFactory(factory) => { /* AsyncNetWorker */ }
        sync_backend => { /* NetWorker (sync) */ }
    }
}
```

### InterruptTransport contents

`InterruptTransport` wraps `IrqChip` and related interrupt infrastructure. The task-implementor must check how `Net::activate()` can extract `interrupt_status`, `interrupt_evt`, `intc`, and `irq_line` from `InterruptTransport` to pass them to `ProxyNetWorker::new()`.

Look at how `NetWorker::new()` (sync path) uses the `interrupt` parameter to understand the extraction pattern.

### ProxyNetWorker routing

`ProxyNetWorker` routes TCP connections from the guest (via virtio-net queues) to real host TCP endpoints. The guest uses a virtual IP `192.168.100.2` and the proxy at `192.168.100.1`. TCP SYN packets trigger `intercept_new_session()` which connects to the real destination on the host.

For the integration test, the host starts a TCP listener on `127.0.0.1:0` (ephemeral port). The guest connects to `127.0.0.1:<port>` — this goes through the smoltcp proxy which intercepts the SYN and creates a real host TcpStream to `127.0.0.1:<port>`.

The guest's default gateway and DNS resolution are handled by the proxy (smoltcp's built-in routing). The guest must be configured to use the proxy's IP as its gateway.

### Guest networking setup

The guest VM needs:
- A network interface with IP `192.168.100.2/24`
- Default gateway `192.168.100.1` (the proxy's IP)

The guest-agent binary needs to bring up the interface and configure routing. Verify if libkrun's guest init (`embedded_init`) already does this when a virtio-net device is present, or if the guest-agent must run `ip addr add` / `ip route add` commands.

---

## Tasks

<!-- START_SUBCOMPONENT_A (tasks 1-3) -->

<!-- START_TASK_1 -->
### Task 1: Add VirtioNetBackend::Proxy variant and handle it in Net::activate()

**Verifies:** test-coverage.AC8.1 (prerequisite)

**Files:**
- Modify: `src/devices/src/virtio/net/device.rs` — `VirtioNetBackend` enum and `Net::activate()`
- Modify: `src/libkrun/src/lib.rs` — re-export the new variant (it's already `pub use devices::virtio::net::device::VirtioNetBackend`)

**Implementation:**

**Step 1: Add variant to VirtioNetBackend**

In `src/devices/src/virtio/net/device.rs`, add to the enum:
```rust
pub enum VirtioNetBackend {
    UnixstreamFd(RawFd),
    UnixstreamPath(PathBuf),
    UnixgramFd(RawFd),
    UnixgramPath(PathBuf, bool),
    #[cfg(target_os = "linux")]
    Tap(String),
    CustomAsyncFactory(Box<dyn AsyncNetBackendFactory>),
    /// Use smoltcp-based ProxyNetWorker as the network backend.
    /// `listeners` maps VM-side ports to host Unix socket paths for ingress connections.
    Proxy { listeners: Vec<(u16, String)> },
}
```

Also update the `Clone` impl for `VirtioNetBackend`:
```rust
Self::Proxy { listeners } => Self::Proxy { listeners: listeners.clone() },
```

**Step 2: Handle Proxy in Net::activate()**

In `Net::activate()`, add a new arm before or after `CustomAsyncFactory`:

```rust
VirtioNetBackend::Proxy { listeners } => {
    use crate::virtio::net::proxy::ProxyNetWorker;
    // Extract what ProxyNetWorker needs from the net device state
    // The interrupt_status, interrupt_evt, intc, irq_line must come from
    // the Net struct's stored fields or the InterruptTransport.
    // The task-implementor must find where interrupt_status, interrupt_evt,
    // intc, and irq_line are stored in Net and how to extract them from
    // InterruptTransport (which is the `interrupt` param to activate()).
    //
    // Pattern: Look at how NetWorker::new() uses the `interrupt` param —
    // extract the same fields for ProxyNetWorker.

    let interrupt_status = /* extract from interrupt */;
    let interrupt_evt = /* extract from interrupt */;
    let intc = /* extract from interrupt or Net fields */;
    let irq_line = /* extract from Net fields */;

    match ProxyNetWorker::new(
        self.queues.clone(),
        queue_evts,
        interrupt_status,
        interrupt_evt,
        intc,
        irq_line,
        mem.clone(),
        listeners,
    ) {
        Ok(worker) => {
            std::thread::spawn(move || worker.run());
            self.device_state = DeviceState::Activated(mem, interrupt);
            Ok(())
        }
        Err(err) => {
            error!("Error activating ProxyNetWorker: {err:?}");
            Err(ActivateError::BadActivate)
        }
    }
}
```

**Critical investigation needed before implementation:** The task-implementor MUST read `NetWorker::new()` (in `src/devices/src/virtio/net/worker.rs` or similar) to understand how `InterruptTransport` is used to extract `interrupt_status`, `interrupt_evt`, `intc`, and `irq_line`. These are the same parameters `ProxyNetWorker::new()` needs.

**Verification:**

Run: `cargo build -p devices`
Expected: Builds without errors. No existing net tests broken.

Run: `cargo test -p devices`
Expected: All existing net tests pass.

**Commit:** `feat(net): add VirtioNetBackend::Proxy variant backed by ProxyNetWorker`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Create test_net_proxy.rs (AC8.1)

**Verifies:** test-coverage.AC8.1

**Files:**
- Create: `tests/test_cases/src/test_net_proxy.rs`

**Implementation:**

The host starts a TCP listener (standard `std::net::TcpListener`), starts the VM with `VirtioNetBackend::Proxy`, and accepts the guest's TCP connection. The guest connects to the host listener's IP:port via the proxy and exchanges PING/PONG.

**Note on host IP from guest perspective:** The ProxyNetWorker intercepts TCP SYN packets from the guest and connects a real `TcpStream` to the destination. So the guest connects to the literal host IP:port (e.g., `127.0.0.1:<port>`), and the proxy connects to the same `127.0.0.1:<port>` on the host. The host TCP listener must be bound before the VM starts.

**Note on guest network configuration:** The task-implementor must verify whether the guest init (`embedded_init`) automatically brings up the virtio-net interface with IP `192.168.100.2` and gateway `192.168.100.1`, or whether the guest-agent binary must do it manually. If manual, add network setup to the guest `in_guest()` function.

```rust
use macros::{guest, host};

pub struct TestNetProxy;

#[host]
mod host {
    use super::*;
    use crate::krun_rust::setup_fs_builder;
    use crate::{Test, TestSetup};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;
    use std::thread;

    fn server(listener: TcpListener) {
        listener.set_nonblocking(false).unwrap();
        let (mut stream, _addr) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

        let mut buf = vec![0u8; 4];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"PING");

        stream.write_all(b"PONG").unwrap();
    }

    impl Test for TestNetProxy {
        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            // Bind TCP listener on an ephemeral port
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let host_port = listener.local_addr().unwrap().port();

            // Spawn server thread to handle the guest's connection
            thread::spawn(move || server(listener));

            // Write port to a file in the guest filesystem so the guest knows where to connect
            // (alternative: use a fixed port known at compile time — simpler but less flexible)
            // The guest will read HOST_TCP_PORT from an env var or a known file.
            // Simplest approach: use a fixed well-known host port by storing it in a temp file
            // that the host writes and the guest reads via virtiofs.
            // OR: use a vsock control channel first to send the port, then connect via TCP.
            //
            // Simplest approach for the test: write the port to a file in the virtiofs root
            // before the guest reads it.
            let root_dir = test_setup.tmp_dir.join("root");
            std::fs::write(root_dir.join("host_port"), host_port.to_string())?;

            let mut builder = krun::Builder::new();
            builder.vm_config(1, 512)?;
            setup_fs_builder(&mut builder, &test_setup)?;
            builder.add_net_device(
                krun::VirtioNetBackend::Proxy { listeners: vec![] },
                [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee], // MAC address
                0, // features (0 = no extra features)
            );

            let context = builder.build()?;
            let vm_thread = thread::spawn(move || context.run());

            vm_thread.join().ok();
            Ok(())
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    impl Test for TestNetProxy {
        fn in_guest(self: Box<Self>) {
            // If the guest init does NOT automatically bring up the network interface,
            // the task-implementor must add network configuration here:
            // std::process::Command::new("ip")
            //     .args(["addr", "add", "192.168.100.2/24", "dev", "eth0"])
            //     .status().unwrap();
            // std::process::Command::new("ip")
            //     .args(["route", "add", "default", "via", "192.168.100.1"])
            //     .status().unwrap();

            // Read host port from the file written by the host
            let port_str = std::fs::read_to_string("/host_port")
                .expect("Failed to read /host_port");
            let port: u16 = port_str.trim().parse().expect("Invalid port number");

            // Connect to host TCP listener through the smoltcp proxy
            // The proxy intercepts this SYN and connects a real TcpStream to 127.0.0.1:port
            let mut stream = TcpStream::connect(("127.0.0.1", port))
                .expect("Failed to connect to host TCP listener");
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();

            // Send PING
            stream.write_all(b"PING").expect("Failed to send PING");

            // Receive PONG
            let mut buf = vec![0u8; 4];
            stream.read_exact(&mut buf).expect("Failed to receive PONG");
            assert_eq!(&buf, b"PONG", "Expected PONG, got {:?}", &buf);

            println!("OK");
        }
    }
}
```

**Alternative approach for port communication:** Instead of a file, the port can be hardcoded as a well-known value (e.g., 8080). The host binds to `127.0.0.1:8080` and the guest connects to `127.0.0.1:8080`. This is simpler but fails if port 8080 is already in use. The file-in-virtiofs approach is more robust.

**Note on MAC features parameter:** The `features` parameter to `add_net_device()` controls which virtio-net features are negotiated. Pass `0` for a minimal feature set; the guest driver will negotiate what it needs. If the proxy doesn't support certain features, errors will surface during the test.

**Verification:**

Run: `cargo build --features host -p test_cases && cargo build --features guest -p test_cases`
Expected: Both compile without errors.

**Commit:** `test(net-proxy): add TCP PING/PONG integration test for AC8.1`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Register test case and run full test suite

**Verifies:** AC8.1 registered

**Files:**
- Modify: `tests/test_cases/src/lib.rs`

**Implementation:**

```rust
mod test_net_proxy;
use test_net_proxy::TestNetProxy;

// In test_cases():
TestCase::new("net-proxy-ping-pong", Box::new(TestNetProxy)),
```

**Step 1: Run the new test**

Run: `make test` (or equivalent)
Expected: `net-proxy-ping-pong` passes (AC8.1).

**Step 2: Verify no regressions**

Expected: All prior tests still pass (now 17 total).

**Commit:** `test(net-proxy): register net proxy integration test in lib.rs`
<!-- END_TASK_3 -->

<!-- END_SUBCOMPONENT_A -->
