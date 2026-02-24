# Test Requirements: async-net-loopback

Maps each acceptance criterion to either an automated test with expected file path and type, or to a documented human verification with justification.

Rationalized against the design document (`docs/design-plans/2026-02-24-async-net-loopback.md`) and implementation plans (`phase_01.md`, `phase_02.md`).

---

## AC1: Proxy code and dependencies removed

### AC1.1: proxy.rs deleted, no pub mod proxy in mod.rs

**Verification type:** Human (build-time structural check)

**Justification:** This criterion asserts that a file does not exist and that a module declaration has been removed. These are static properties of the source tree, not runtime behaviors. The compiler already enforces that any dangling reference to `proxy` would be a build failure.

**Verification approach:**
1. Confirm `src/devices/src/virtio/net/proxy.rs` does not exist.
2. Confirm `src/devices/src/virtio/net/mod.rs` contains no line matching `pub mod proxy`.
3. `cargo build -p devices --features net` succeeds (covered by AC1.4 automated check).

---

### AC1.2: VirtioNetBackend::Proxy variant removed from enum

**Verification type:** Human (build-time structural check)

**Justification:** This is a static property of the type system. If any code references `VirtioNetBackend::Proxy` after removal, the build would fail. A `grep` for `Proxy` in `device.rs` is sufficient.

**Verification approach:**
1. Confirm `src/devices/src/virtio/net/device.rs` contains no `Proxy` variant in the `VirtioNetBackend` enum.
2. Confirm the `Clone` impl no longer has a `Self::Proxy { .. }` arm.
3. Confirm the `activate` method no longer has a `VirtioNetBackend::Proxy` match arm.
4. Confirm `src/devices/src/virtio/net/worker.rs` no longer has a `VirtioNetBackend::Proxy` panic arm.
5. `cargo build -p devices --features net` succeeds (covered by AC1.4).

---

### AC1.3: net feature flag contains only tokio and bytes

**Verification type:** Human (manifest inspection)

**Justification:** Feature flag contents are declarative metadata in `Cargo.toml`. There is no runtime API to query which crates a feature pulls in. Cargo's dependency resolver enforces correctness — if a removed dependency is still imported in code, the build fails.

**Verification approach:**
1. Inspect `src/devices/Cargo.toml` and confirm the `net` feature line is exactly: `net = ["tokio", "bytes"]`
2. Confirm no entries for `mio`, `pnet`, `pnet_base`, `smoltcp`, `socket2`, or `tracing` remain in `[dependencies]`.
3. `cargo build -p devices --features net` succeeds (covered by AC1.4).

---

### AC1.4: cargo build --features net succeeds

| Field | Value |
|---|---|
| **Test type** | Automated (build verification) |
| **Test file** | N/A (build command, not a test file) |
| **Test command** | `cargo build -p devices --features net` |

---

## AC2: Loopback backend responds to ARP and ICMP

### AC2.1: ARP request for 192.168.100.1 receives ARP reply with correct MAC

| Field | Value |
|---|---|
| **Test type** | Automated (integration / e2e) |
| **Test file** | `tests/test_cases/src/test_net_async_loopback.rs` |
| **Test name** | `net-async-loopback` |
| **Run command** | `make test FEATURE_FLAGS="--features embedded_init"` |

**Rationale:** Verified implicitly. The guest kernel must resolve the destination MAC via ARP before sending the ICMP packet. If the loopback backend fails to respond to the ARP request with the correct MAC, the guest kernel never sends the ICMP packet and the test times out.

---

### AC2.2: ICMP echo request to 192.168.100.1 receives echo reply with matching ID/sequence

| Field | Value |
|---|---|
| **Test type** | Automated (integration / e2e) |
| **Test file** | `tests/test_cases/src/test_net_async_loopback.rs` |
| **Test name** | `net-async-loopback` |
| **Run command** | `make test FEATURE_FLAGS="--features embedded_init"` |

**Rationale:** The guest constructs an ICMP echo request, sends it via `sendto()`, and asserts the reply has type=0 (echo reply), code=0, and matching sequence number.

---

### AC2.3: Non-ARP, non-ICMP packets are silently dropped (no crash, no reply)

| Field | Value |
|---|---|
| **Test type** | Automated (implicit) + Human |
| **Test file** | `tests/test_cases/src/test_net_async_loopback.rs` |
| **Test name** | `net-async-loopback` |
| **Run command** | `make test FEATURE_FLAGS="--features embedded_init"` |

**Rationale:** During guest network configuration and ARP resolution, the guest kernel emits various broadcast/multicast packets. The backend must silently drop non-matching packets. Test passing proves the backend survived.

**Supplemental human verification:** Review `tests/test_cases/src/loopback_net.rs` and confirm `handle_guest_tx()` has a catch-all match arm and no `unwrap()`/`expect()` on packet parsing.

---

## AC3: Integration test proves async path

### AC3.1: Test uses VirtioNetBackend::CustomAsyncFactory with LoopbackFactory

| Field | Value |
|---|---|
| **Test type** | Automated (structural, type-system enforced) |
| **Test file** | `tests/test_cases/src/test_net_async_loopback.rs` |
| **Test name** | `net-async-loopback` |
| **Run command** | `make test FEATURE_FLAGS="--features embedded_init"` |

**Rationale:** If `LoopbackFactory` does not implement `AsyncNetBackendFactory`, or if `CustomAsyncFactory` is not used, the test will not compile.

---

### AC3.2: Guest configures eth0 with 192.168.100.2/24 and pings 192.168.100.1

| Field | Value |
|---|---|
| **Test type** | Automated (integration / e2e) |
| **Test file** | `tests/test_cases/src/test_net_async_loopback.rs` |
| **Test name** | `net-async-loopback` |
| **Run command** | `make test FEATURE_FLAGS="--features embedded_init"` |

**Rationale:** Guest calls `configure_eth0()` (IP 192.168.100.2, netmask /24, interface up) then sends ICMP to 192.168.100.1.

---

### AC3.3: Guest receives ICMP echo reply within timeout

| Field | Value |
|---|---|
| **Test type** | Automated (integration / e2e) |
| **Test file** | `tests/test_cases/src/test_net_async_loopback.rs` |
| **Test name** | `net-async-loopback` |
| **Run command** | `make test FEATURE_FLAGS="--features embedded_init"` |

**Rationale:** Guest sets `SO_RCVTIMEO` to 5 seconds and asserts `recvfrom()` returns > 0 bytes.

---

## AC4: All tests pass

### AC4.1: make test FEATURE_FLAGS="--features embedded_init" passes

| Field | Value |
|---|---|
| **Test type** | Automated (full integration suite) |
| **Test file** | All test files in `tests/test_cases/src/` |
| **Run command** | `make test FEATURE_FLAGS="--features embedded_init"` |

**Known flakiness:** Tests are inherently flaky (VM + network timing). 5-6/6 passing is normal.

---

### AC4.2: cargo build --features net succeeds (no compilation errors from removal)

| Field | Value |
|---|---|
| **Test type** | Automated (build verification) |
| **Test command** | `cargo build -p devices --features net` |

---

## Summary Matrix

| AC | Description | Verification | Method |
|----|-------------|-------------|--------|
| AC1.1 | proxy.rs deleted, no pub mod proxy | Human | File/grep inspection + build gate |
| AC1.2 | VirtioNetBackend::Proxy removed | Human | Code inspection + build gate |
| AC1.3 | net feature = tokio + bytes only | Human | Cargo.toml inspection + build gate |
| AC1.4 | cargo build --features net succeeds | **Automated** | `cargo build -p devices --features net` |
| AC2.1 | ARP reply with correct MAC | **Automated** | `net-async-loopback` integration test (implicit via kernel ARP) |
| AC2.2 | ICMP echo reply with matching ID/seq | **Automated** | `net-async-loopback` integration test (explicit assertions) |
| AC2.3 | Non-ARP/ICMP silently dropped | **Automated** + Human | Integration test survival + code review of match arms |
| AC3.1 | Uses CustomAsyncFactory + LoopbackFactory | **Automated** | Compilation of test (type system enforces trait bounds) |
| AC3.2 | Guest configures eth0 and pings | **Automated** | `net-async-loopback` guest-side assertions |
| AC3.3 | Echo reply within timeout | **Automated** | `recvfrom()` with 5s SO_RCVTIMEO + assert received > 0 |
| AC4.1 | Full test suite passes | **Automated** | `make test FEATURE_FLAGS="--features embedded_init"` |
| AC4.2 | Build succeeds post-removal | **Automated** | `cargo build -p devices --features net` |

**Automated test files:**
- `tests/test_cases/src/test_net_async_loopback.rs` — integration test (covers AC2.1, AC2.2, AC2.3, AC3.1, AC3.2, AC3.3)
- `tests/test_cases/src/loopback_net.rs` — LoopbackFactory + LoopbackBackend (test helper, not a test itself)

**Human verification files:**
- `src/devices/src/virtio/net/mod.rs` — confirm no `pub mod proxy` (AC1.1)
- `src/devices/src/virtio/net/device.rs` — confirm no `Proxy` variant (AC1.2)
- `src/devices/src/virtio/net/worker.rs` — confirm no `Proxy` panic arm (AC1.2)
- `src/devices/Cargo.toml` — confirm `net` feature contents (AC1.3)
- `tests/test_cases/src/loopback_net.rs` — confirm catch-all drop arm in `handle_guest_tx` (AC2.3)
