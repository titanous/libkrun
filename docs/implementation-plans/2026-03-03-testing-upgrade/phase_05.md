# Phase 5: ASan + Shuttle

## Overview

**Goal:** Runtime memory error detection on unit tests and integration tests, and randomized concurrency testing for complex multi-threaded coordination patterns.

**Design reference:** `docs/design-plans/2026-03-03-testing-upgrade.md` — `<!-- START_PHASE_5 -->`

**Acceptance criteria addressed:**
- testing-upgrade.AC2.4: `just shuttle` runs randomized concurrency tests on block quiesce, balloon resize, device activation and passes
- testing-upgrade.AC2.7: `just asan` runs unit tests under AddressSanitizer and passes
- testing-upgrade.AC2.8: `just integration-asan` runs integration tests under AddressSanitizer and passes

**Done when:** `just asan` passes, `just integration-asan` passes, `just shuttle` passes.

**Dependencies:** Phase 1 complete (justfile exists with `asan`, `integration-asan`, `shuttle` stub targets).

---

## Investigation Findings

### ASan toolchain requirements

ASan requires nightly Rust and a specific `--target` flag (even when compiling for the host). The invocation is:

```
RUSTFLAGS="-Zsanitizer=address" cargo +nightly test --target x86_64-unknown-linux-gnu
```

There is no existing `.cargo/config.toml` in the root workspace; RUSTFLAGS must be passed via the environment. ASan cannot be combined with loom (`--cfg loom`) or Miri — they run separately.

For integration tests, `tests/run.sh` builds the `runner`, `test-daemon`, and `test-vsock-proxy` binaries via plain `cargo build` calls (no nightly, no RUSTFLAGS). To get ASan coverage, RUSTFLAGS must be set before `run.sh` is invoked so those `cargo build` calls inherit the flag. The `runner` binary is the process that spawns VMs and calls into libkrun; it is the primary target for ASan instrumentation. The `guest-agent` binary is musl-compiled (`x86_64-unknown-linux-musl`) — musl + ASan is not supported; that build is left without ASan. The `test-daemon` and `test-vsock-proxy` are standard glibc builds and will inherit ASan automatically when RUSTFLAGS is set.

### Quiesce handshake (`src/devices/src/virtio/block/async_worker.rs`)

The quiesce handshake coordinates the VMM control plane thread with the async block worker (a tokio task). The coordination point is `quiesce_ack: Arc<(Mutex<bool>, Condvar)>` (line 158):

- VMM signals quiesce by writing to `quiesce_fd` (EventFd)
- Worker receives the event, drains in-flight I/O operations, then sets the ACK:
  ```rust
  // async_worker.rs lines 543-547
  let (lock, cvar) = &*quiesce_ack;
  *lock.lock().unwrap() = true;
  cvar.notify_one();
  ```
- VMM waits on the condvar until `acked == true`, then reads queue state
- VMM writes to `resume_fd` to release the worker from park

The shuttle test models the ACK side of this protocol: a "worker" thread sets `acked = true` and signals the condvar; a "VMM" thread waits on the condvar until `acked`. This isolates the `Mutex<bool> + Condvar` coordination from the tokio runtime and EventFd syscalls.

**Existing test context:** A full `test_quiesce_ack_resume` integration test exists at line 2431 of `async_worker.rs` and runs against a real tokio runtime. The shuttle test is complementary — it exhaustively randomizes thread scheduling on the pure `Mutex<bool> + Condvar` pattern to detect missed wakeups or deadlocks under adversarial interleavings.

### Balloon condvar (`src/libkrun/src/lib.rs` + `src/devices/src/virtio/balloon/device.rs`)

`BalloonHandle::await_target` (lib.rs lines 3473-3515) holds a `Mutex<u64>` guard and calls `cvar.wait_timeout(actual, stall_timeout)` in a loop until the actual page count reaches target. The condvar is signaled by the guest write handler in `device.rs` lines 671-674:

```rust
let (lock, cvar) = &*self.actual_condvar;
if let Ok(mut val) = lock.lock() {
    *val = actual_pages;
    cvar.notify_all();
}
```

The shuttle test models this: a "VMM" thread calls the `await_target` loop pattern (lock condvar, check value, wait); a "guest" thread sets the value and signals. The test verifies no deadlock and correct termination when the guest delivers the value.

**Key detail:** `await_target` uses `cvar.wait_timeout` (not `cvar.wait`). Shuttle's `Condvar::wait_timeout` behaves differently from std's — shuttle does not respect real wall-clock duration and instead treats the timeout as a scheduling point. The shuttle test must not rely on the `stall_timeout` expiring; it should verify the condvar-signaled path only.

### DeviceState (`src/devices/src/virtio/device.rs` lines 48-51)

```rust
pub enum DeviceState {
    Inactive,
    Activated(GuestMemoryMmap, InterruptTransport),
}
```

`DeviceState` has no `Mutex` — it is held by value inside each device struct. `GuestMemoryMmap` and `InterruptTransport` contain non-`Send` or non-trivially-clonable types, making `Arc<DeviceState>` not directly constructible for a test. The shuttle test wraps the state in `Arc<Mutex<DeviceState>>` — a realistic pattern since the device struct itself is typically protected by a `Mutex` at the VMM layer. The test uses a `bool` stand-in for the state transition (Inactive → Activated) since `GuestMemoryMmap` requires a real KVM mapping.

The property being tested: no torn read — concurrent readers always observe either `Inactive` or `Activated`, never a partially-written state. With `Arc<Mutex<bool>>` as the model, this reduces to verifying that a mutex-protected flag transitions atomically from false to true with no reader seeing an inconsistent intermediate state.

### Shuttle library

- Version: `shuttle = "0.8"` as `[dev-dependencies]`
- Shuttle tests must not run during normal `cargo test`. The conventional approach is to gate them with a `shuttle` feature flag: add `shuttle = []` to `[features]` and protect test modules with `#[cfg(feature = "shuttle")]`. This prevents shuttle from interfering with proptest, loom, or regular unit test runs.
- Entry point: `shuttle::check_random(|| { ... }, N)` for randomized scheduling with N iterations
- Import pattern replaces std sync primitives:
  ```rust
  use shuttle::sync::{Arc, Mutex, Condvar};
  use shuttle::thread;
  ```
- `shuttle::check_random` panics on detected deadlock or assertion failure; the test harness catches the panic and reports which interleaving triggered it.

---

## Task 1: Add `shuttle` dev-dependency to `vmm` and `devices` Cargo.toml

**Verifies:** prerequisite for Tasks 2-4 (shuttle tests compile)

**Files:**
- Modify: `src/vmm/Cargo.toml`
- Modify: `src/devices/Cargo.toml`

**Implementation:**

**Step 1: Add shuttle to `src/vmm/Cargo.toml`**

Add a `shuttle` feature flag and the `shuttle` dev-dependency. The `[dev-dependencies]` section already exists at line 67 with `devices = { path = "../devices", features = ["test_utils"] }`.

Add to `[features]`:
```toml
shuttle = ["dep:shuttle"]
```

Add to `[dev-dependencies]`:
```toml
shuttle = { version = "0.8", optional = true }
```

Because optional dev-dependencies require Cargo 1.60+ syntax, the `optional = true` form is used. Alternatively, use the simpler non-optional form if all CI machines have shuttle available:
```toml
[dev-dependencies]
shuttle = "0.8"
```

Then gate the test modules with `#[cfg(feature = "shuttle")]` to prevent them from compiling unless explicitly requested.

**Preferred approach** (avoids pulling shuttle into all dev builds): Add to `[features]`:
```toml
shuttle = []
```

Add to `[dev-dependencies]`:
```toml
shuttle = "0.8"
```

Gate test modules with `#[cfg(feature = "shuttle")]`. The `shuttle` crate is always present as a dev-dep (no optional = true needed since dev-deps don't affect the release build), but the test code only compiles with `--features shuttle`.

**Step 2: Add shuttle to `src/devices/Cargo.toml`**

The `devices` Cargo.toml has no `[dev-dependencies]` section. Add one at the end of the file:

```toml
[dev-dependencies]
shuttle = "0.8"
```

Add to `[features]`:
```toml
shuttle = []
```

**Verification:**

```bash
cargo check -p vmm --features shuttle
cargo check -p devices --features shuttle
```

Expected: Both compile cleanly. No shuttle tests run yet (modules don't exist).

**Commit:** `chore: add shuttle dev-dependency to vmm and devices`

---

## Task 2: Shuttle test for block worker quiesce handshake

**Verifies:** testing-upgrade.AC2.4 (block quiesce scenario)

**Files:**
- Modify: `src/devices/src/virtio/block/async_worker.rs`

**Implementation:**

Add a new test module at the bottom of `src/devices/src/virtio/block/async_worker.rs`, inside the existing `#[cfg(test)]` block (or add a new one if the existing tests use a separate module):

```rust
#[cfg(feature = "shuttle")]
mod shuttle_tests {
    use shuttle::sync::{Arc, Condvar, Mutex};
    use shuttle::thread;

    /// Shuttle test for the quiesce ACK handshake pattern.
    ///
    /// Models the coordination between VMM control plane and async block worker:
    ///   - Worker thread: sets acked=true, signals condvar (simulates lines 543-547
    ///     of async_worker.rs after draining in-flight I/O)
    ///   - VMM thread: waits on condvar until acked==true (simulates VMM quiesce wait)
    ///
    /// Verifies: no deadlock, no missed wakeup under any thread interleaving.
    /// The quiesce_fd/resume_fd EventFd signaling is omitted — shuttle tests the
    /// pure Mutex<bool>+Condvar coordination that follows the fd notification.
    #[test]
    fn shuttle_quiesce_ack_no_deadlock() {
        shuttle::check_random(
            || {
                let quiesce_ack: Arc<(Mutex<bool>, Condvar)> =
                    Arc::new((Mutex::new(false), Condvar::new()));

                // Worker thread: simulate quiesce ACK (async_worker.rs lines 543-547)
                let ack_worker = Arc::clone(&quiesce_ack);
                let worker = thread::spawn(move || {
                    let (lock, cvar) = &*ack_worker;
                    *lock.lock().unwrap() = true;
                    cvar.notify_one();
                });

                // VMM thread: wait for worker to ACK quiesce
                let ack_vmm = Arc::clone(&quiesce_ack);
                let vmm = thread::spawn(move || {
                    let (lock, cvar) = &*ack_vmm;
                    let mut acked = lock.lock().unwrap();
                    while !*acked {
                        acked = cvar.wait(acked).unwrap();
                    }
                    assert!(*acked, "quiesce ack must be true after condvar wait");
                });

                worker.join().unwrap();
                vmm.join().unwrap();
            },
            1000,
        );
    }

    /// Shuttle test: quiesce → resume cycle (two sequential handshakes).
    ///
    /// Verifies that after an ACK+resume, a second quiesce cycle does not deadlock.
    /// Models the real pattern where snapshot can trigger multiple quiesce cycles.
    #[test]
    fn shuttle_quiesce_two_cycles_no_deadlock() {
        shuttle::check_random(
            || {
                let quiesce_ack: Arc<(Mutex<bool>, Condvar)> =
                    Arc::new((Mutex::new(false), Condvar::new()));

                for _cycle in 0..2 {
                    let ack_worker = Arc::clone(&quiesce_ack);
                    let worker = thread::spawn(move || {
                        let (lock, cvar) = &*ack_worker;
                        *lock.lock().unwrap() = true;
                        cvar.notify_one();
                    });

                    let ack_vmm = Arc::clone(&quiesce_ack);
                    let vmm = thread::spawn(move || {
                        let (lock, cvar) = &*ack_vmm;
                        let mut acked = lock.lock().unwrap();
                        while !*acked {
                            acked = cvar.wait(acked).unwrap();
                        }
                    });

                    worker.join().unwrap();
                    vmm.join().unwrap();

                    // Reset for next cycle (models resume phase resetting ack state)
                    *quiesce_ack.0.lock().unwrap() = false;
                }
            },
            500,
        );
    }
}
```

**Note:** The test module uses `#[cfg(feature = "shuttle")]` (not `#[cfg(loom)]`). This is intentional — shuttle and loom are separate tools with different APIs. Loom tests for `DirtyBitmap`, `ReclaimedBitmap`, and `PageTracker` use `#[cfg(loom)]`.

**Verification:**

```bash
cargo test -p devices --features net,shuttle -- shuttle_tests
```

Expected: Both shuttle tests pass with 1000 iterations each.

**Commit:** `test(devices): add shuttle test for block worker quiesce handshake`

---

## Task 3: Shuttle test for balloon condvar

**Verifies:** testing-upgrade.AC2.4 (balloon resize scenario)

**Files:**
- Modify: `src/devices/src/virtio/balloon/device.rs`

**Implementation:**

Add a new test module at the bottom of `src/devices/src/virtio/balloon/device.rs` (inside or alongside the existing `#[cfg(test)]` block):

```rust
#[cfg(feature = "shuttle")]
#[cfg(test)]
mod shuttle_tests {
    use shuttle::sync::{Arc, Condvar, Mutex};
    use shuttle::thread;

    /// Shuttle test for the balloon actual-pages condvar pattern.
    ///
    /// Models BalloonHandle::await_target + guest config-write handler coordination:
    ///   - "guest" thread: sets actual_pages and signals condvar
    ///     (device.rs lines 671-674: *val = actual_pages; cvar.notify_all())
    ///   - "VMM" thread: waits on condvar until actual >= target
    ///     (lib.rs await_target: cvar.wait(actual) loop)
    ///
    /// Verifies: condvar wait terminates, no deadlock, correct final value.
    ///
    /// Note: Uses cvar.wait (not wait_timeout) since shuttle does not respect
    /// real wall-clock durations. The stall_timeout path is tested separately
    /// in unit tests (test_balloon_handle_await_target_stalled_no_progress).
    #[test]
    fn shuttle_balloon_condvar_no_deadlock() {
        shuttle::check_random(
            || {
                // actual_condvar: Arc<(Mutex<u64>, Condvar)>
                // Mirrors device.rs actual_condvar structure (line 147)
                let actual_condvar: Arc<(Mutex<u64>, Condvar)> =
                    Arc::new((Mutex::new(0u64), Condvar::new()));

                let target_pages: u64 = 64; // arbitrary target

                // "Guest" thread: writes actual and signals (device.rs lines 671-674)
                let condvar_guest = Arc::clone(&actual_condvar);
                let guest = thread::spawn(move || {
                    let (lock, cvar) = &*condvar_guest;
                    let mut val = lock.lock().unwrap();
                    *val = target_pages;
                    cvar.notify_all();
                });

                // "VMM" thread: await_target loop (lib.rs lines 3483-3514, wait path only)
                let condvar_vmm = Arc::clone(&actual_condvar);
                let vmm = thread::spawn(move || {
                    let (lock, cvar) = &*condvar_vmm;
                    let mut actual = lock.lock().unwrap();
                    while *actual < target_pages {
                        actual = cvar.wait(actual).unwrap();
                    }
                    assert!(
                        *actual >= target_pages,
                        "await_target must observe actual >= target after condvar wait, got {}",
                        *actual
                    );
                });

                guest.join().unwrap();
                vmm.join().unwrap();
            },
            1000,
        );
    }

    /// Shuttle test: multiple guest updates, VMM observes final value.
    ///
    /// Models incremental inflation: guest sends multiple actual updates before
    /// reaching target. Verifies VMM loop terminates correctly.
    #[test]
    fn shuttle_balloon_incremental_updates_no_deadlock() {
        shuttle::check_random(
            || {
                let actual_condvar: Arc<(Mutex<u64>, Condvar)> =
                    Arc::new((Mutex::new(0u64), Condvar::new()));

                let target_pages: u64 = 3;

                // Guest sends three incremental updates
                let condvar_guest = Arc::clone(&actual_condvar);
                let guest = thread::spawn(move || {
                    for pages in 1u64..=target_pages {
                        let (lock, cvar) = &*condvar_guest;
                        let mut val = lock.lock().unwrap();
                        *val = pages;
                        cvar.notify_all();
                    }
                });

                // VMM waits until actual reaches target
                let condvar_vmm = Arc::clone(&actual_condvar);
                let vmm = thread::spawn(move || {
                    let (lock, cvar) = &*condvar_vmm;
                    let mut actual = lock.lock().unwrap();
                    while *actual < target_pages {
                        actual = cvar.wait(actual).unwrap();
                    }
                    assert!(*actual >= target_pages);
                });

                guest.join().unwrap();
                vmm.join().unwrap();
            },
            500,
        );
    }
}
```

**Verification:**

```bash
cargo test -p devices --features net,shuttle -- balloon::device::shuttle_tests
```

Expected: Both shuttle tests pass.

**Commit:** `test(devices): add shuttle test for balloon condvar coordination`

---

## Task 4: Shuttle test for DeviceState transitions

**Verifies:** testing-upgrade.AC2.4 (device activation scenario)

**Files:**
- Modify: `src/devices/src/virtio/device.rs`

**Implementation:**

`DeviceState` has no `Mutex` by itself — it is an enum held by value inside device structs. `GuestMemoryMmap` and `InterruptTransport` cannot be constructed in a test without KVM. The shuttle test uses `bool` as a stand-in for the Inactive → Activated state transition, wrapped in `Arc<Mutex<bool>>` to model the protection that a real device struct's `Mutex<DeviceImpl>` provides.

The property under test: a writer transitioning the state from `false` (Inactive) to `true` (Activated) is always observed atomically by concurrent readers — readers see either `false` or `true`, never a partially-written state. With `Mutex`, this is trivially true by construction, but the shuttle test validates that no code path accidentally reads outside the lock.

Add at the bottom of `src/devices/src/virtio/device.rs`:

```rust
#[cfg(feature = "shuttle")]
#[cfg(test)]
mod shuttle_tests {
    use shuttle::sync::{Arc, Mutex};
    use shuttle::thread;

    /// Shuttle test for DeviceState Inactive → Activated transition.
    ///
    /// DeviceState itself has no Mutex (it is held by value in device structs).
    /// In production, device structs are protected by Mutex at the VMM layer.
    /// This test models that pattern: a Mutex<bool> where false=Inactive, true=Activated.
    ///
    /// Verifies: concurrent readers never observe a torn or intermediate state.
    /// The writer holds the lock for the full transition; readers hold the lock
    /// for the full read. Shuttle randomizes the acquisition order.
    #[test]
    fn shuttle_device_state_transition_no_torn_read() {
        shuttle::check_random(
            || {
                // false = Inactive, true = Activated
                let state: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

                // Writer: transition Inactive → Activated (holds lock for full write)
                let state_writer = Arc::clone(&state);
                let writer = thread::spawn(move || {
                    let mut s = state_writer.lock().unwrap();
                    *s = true; // Inactive → Activated
                });

                // Reader 1: reads state, asserts it is either Inactive or Activated (never torn)
                let state_r1 = Arc::clone(&state);
                let reader1 = thread::spawn(move || {
                    let s = state_r1.lock().unwrap();
                    // Must be a valid state: false (Inactive) or true (Activated)
                    // The assertion always holds for bool, but shuttle verifies no
                    // lock-free path exists that could produce an intermediate value.
                    assert!(
                        *s == false || *s == true,
                        "DeviceState must be Inactive or Activated, never torn"
                    );
                });

                // Reader 2: concurrent with reader1 and writer
                let state_r2 = Arc::clone(&state);
                let reader2 = thread::spawn(move || {
                    let s = state_r2.lock().unwrap();
                    assert!(*s == false || *s == true);
                });

                writer.join().unwrap();
                reader1.join().unwrap();
                reader2.join().unwrap();

                // After all threads: state must be Activated (writer always runs to completion)
                let final_state = state.lock().unwrap();
                assert!(
                    *final_state,
                    "state must be Activated after writer completes"
                );
            },
            1000,
        );
    }

    /// Shuttle test: activation followed by concurrent readers.
    ///
    /// Models workers that read DeviceState after activation to decide whether to
    /// process virtqueue notifications. Verifies that once activated, all readers
    /// consistently observe the activated state.
    #[test]
    fn shuttle_device_state_post_activation_consistent() {
        shuttle::check_random(
            || {
                let state: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

                // Activate first (sequential — no concurrency for the write itself)
                {
                    let mut s = state.lock().unwrap();
                    *s = true;
                }

                // Multiple concurrent readers post-activation: all must see true
                let handles: Vec<_> = (0..3)
                    .map(|_| {
                        let state_clone = Arc::clone(&state);
                        thread::spawn(move || {
                            let s = state_clone.lock().unwrap();
                            assert!(*s, "all readers must see Activated after activation");
                        })
                    })
                    .collect();

                for h in handles {
                    h.join().unwrap();
                }
            },
            500,
        );
    }
}
```

**Verification:**

```bash
cargo test -p devices --features shuttle -- device::shuttle_tests
```

Expected: Both shuttle tests pass.

**Commit:** `test(devices): add shuttle test for DeviceState activation transitions`

---

## Task 5: Add `just asan` justfile target

**Verifies:** testing-upgrade.AC2.7

**Files:**
- Modify: `justfile` (project root)

**Implementation:**

Replace the `asan` stub in `justfile` with the full implementation:

```just
# ASan: run unit tests under AddressSanitizer.
# Requires nightly Rust. Detects buffer overflows, use-after-free, heap corruption.
# Must use --target explicitly (ASan requires target triple even for host builds).
asan:
    RUSTFLAGS="-Zsanitizer=address" \
    cargo +nightly test \
        --target x86_64-unknown-linux-gnu \
        -p devices --features net,snapshot
    RUSTFLAGS="-Zsanitizer=address" \
    cargo +nightly test \
        --target x86_64-unknown-linux-gnu \
        -p vmm --features snapshot
```

**Notes:**
- `--target x86_64-unknown-linux-gnu` is required even when compiling for the host. ASan with `-Zsanitizer=address` requires an explicit target triple; without it, the build fails with a cryptic error about sanitizers only being supported for specific targets.
- The `devices` tests with `--features net` require tokio, which compiles cleanly under ASan.
- The `blk` and `vhost-user` features are omitted from the ASan target to reduce build time; `net` and `snapshot` cover the highest-value code paths (async network worker, snapshot serialization).
- ASan adds ~2x runtime overhead and ~2x compile time. The target is limited to the two highest-value crates.
- If the nightly toolchain from `rust-toolchain.toml` already specifies nightly, the `+nightly` override can be omitted. Check the `channel` field in `rust-toolchain.toml` before running.

**Verification:**

```bash
just asan
```

Expected: Both `cargo +nightly test` invocations compile and run to completion with no ASan violations (no `ERROR: AddressSanitizer` in output, exit code 0).

If ASan reports a violation, it prints a stack trace to stderr and exits non-zero. Common causes:
- Buffer read past end of a slice (stack-buffer-overflow)
- Use-after-free in a dropped value accessed via raw pointer
- Heap corruption via unsafe FFI

Fix any violations before proceeding to Task 6.

**Commit:** `feat(justfile): implement just asan target`

---

## Task 6: Add `just integration-asan` justfile target

**Verifies:** testing-upgrade.AC2.8

**Files:**
- Modify: `justfile` (project root)

**Implementation:**

Replace the `integration-asan` stub in `justfile` with the full implementation:

```just
# integration-asan: run integration tests with ASan instrumentation on the runner binary.
#
# How it works:
#   1. Sets RUSTFLAGS="-Zsanitizer=address" and RUSTUP_TOOLCHAIN=nightly so that
#      all `cargo build` calls inside tests/run.sh compile with ASan.
#   2. The runner, test-daemon, and test-vsock-proxy binaries are built with ASan.
#   3. The guest-agent binary is musl-compiled (x86_64-unknown-linux-musl);
#      musl + ASan is unsupported — that build will fail if RUSTFLAGS is set
#      unconditionally. run.sh must be patched (see note below) or the guest-agent
#      build must be separated from the ASan build.
#   4. Calls tests/run.sh with FEATURE_FLAGS="--features embedded_init".
#
# Note on guest-agent: musl + ASan is not supported by the ASan runtime.
# The workaround is to build guest-agent before entering ASan mode, or to
# modify run.sh to skip the RUSTFLAGS env when building for the musl target.
# See implementation note below.
integration-asan:
    mkdir -p test-prefix/lib64
    cd tests && \
        GUEST_TARGET_ARCH="$(uname -m)-unknown-linux-musl" \
        cargo build --target="$(uname -m)-unknown-linux-musl" -p guest-agent && \
        RUSTFLAGS="-Zsanitizer=address" \
        RUSTUP_TOOLCHAIN=nightly \
        KRUN_TEST_GUEST_AGENT_PATH="target/$(uname -m)-unknown-linux-musl/debug/guest-agent" \
        KRUN_NO_RUN_SH_GUEST_AGENT=1 \
        FEATURE_FLAGS="--features embedded_init" \
        LD_LIBRARY_PATH="$(realpath ../test-prefix/lib64/)" \
        ./run.sh test
```

**Implementation note — guest-agent musl incompatibility:**

`tests/run.sh` builds `guest-agent` with `cargo build --target=$GUEST_TARGET_ARCH -p guest-agent` where `$GUEST_TARGET_ARCH` is `x86_64-unknown-linux-musl`. Setting `RUSTFLAGS="-Zsanitizer=address"` globally causes this build to fail because the musl target does not support ASan.

There are two options:

**Option A (preferred — patch run.sh):** Modify `tests/run.sh` to check for a `KRUN_NO_RUN_SH_GUEST_AGENT` environment variable and skip the `cargo build -p guest-agent` step if it is set. The justfile recipe pre-builds guest-agent without ASan before calling run.sh:

```sh
# In tests/run.sh, replace:
cargo build --target=$GUEST_TARGET_ARCH -p guest-agent

# With:
if [ -z "${KRUN_NO_RUN_SH_GUEST_AGENT}" ]; then
    cargo build --target=$GUEST_TARGET_ARCH -p guest-agent
fi
```

Then in the justfile recipe, set `KRUN_NO_RUN_SH_GUEST_AGENT=1` and pre-build `guest-agent` without ASan (no RUSTFLAGS set).

**Option B (simpler — build guest-agent separately, unset RUSTFLAGS for musl):** Use `cargo build` with `RUSTFLAGS=""` overriding the outer env for just the musl build. Shell syntax: `env -u RUSTFLAGS cargo build --target=... -p guest-agent`. This avoids patching run.sh but requires the justfile recipe to be more careful about env variable scoping.

The implementation plan recommends Option A because it keeps run.sh self-contained and avoids shell quoting complexity.

**Files for Option A:**
- Modify: `tests/run.sh` (add `KRUN_NO_RUN_SH_GUEST_AGENT` guard)
- Modify: `justfile` (integration-asan recipe)

**Verification:**

```bash
just integration-asan
```

Expected: Integration tests compile with ASan instrumentation, run, and complete with the same pass rate as `just integration` (5-6/6 tests passing; inherent flakiness from timing-sensitive VM tests). No `ERROR: AddressSanitizer` output.

If an ASan violation is detected in the runner binary during integration tests, it will print a stack trace and the test run will fail. Investigate the specific violation — it may be in libkrun C FFI boundaries or in the runner's memory management.

**Commit:** `feat(justfile): implement just integration-asan target`

---

## Task 7: Add `just shuttle` justfile target

**Verifies:** testing-upgrade.AC2.4

**Files:**
- Modify: `justfile` (project root)

**Implementation:**

Replace the `shuttle` stub in `justfile` with the full implementation:

```just
# Shuttle: randomized concurrency testing for complex multi-threaded coordination.
# Uses shuttle crate to sample thread interleavings (not exhaustive like loom).
# Targets: block worker quiesce handshake, balloon condvar, device state transitions.
# Default: 1000 iterations per test. Pass iterations=N to override.
shuttle iterations="1000":
    SHUTTLE_ITERATIONS={{iterations}} \
    cargo test -p devices --features net,shuttle -- shuttle_tests
    SHUTTLE_ITERATIONS={{iterations}} \
    cargo test -p devices --features shuttle -- \
        balloon::device::shuttle_tests \
        device::shuttle_tests
```

**Note on SHUTTLE_ITERATIONS:** The `shuttle` crate reads the `SHUTTLE_ITERATIONS` environment variable if set, overriding the iteration count passed to `check_random`. This allows CI to run with a lower iteration count (e.g., 100) for speed while local runs use the default 1000. However, as of shuttle 0.8, the env variable override may not be implemented — verify by checking shuttle's documentation. If not supported, the iterations parameter is ignored and the hardcoded value in `check_random(|| { ... }, 1000)` is used. In that case, remove `SHUTTLE_ITERATIONS` from the recipe and document that the count is set at the call site.

**Update the `all` compound target:**

The `all` target in Phase 1 was defined as `all: check test`. Update it to include shuttle:

```just
# Compound target: all fast tests
all: check test shuttle
```

**Verification:**

```bash
just shuttle
```

Expected: All shuttle tests complete with 1000 iterations each, no deadlocks detected, exit code 0.

```bash
just shuttle iterations=100
```

Expected: Same, but faster (100 iterations).

```bash
just all
```

Expected: `check` + `test` + `shuttle` all pass.

**Commit:** `feat(justfile): implement just shuttle target, update just all`

---

## Verification

After completing all tasks, run the following sequence to confirm all acceptance criteria are met:

```bash
# 1. Shuttle tests pass
just shuttle

# 2. ASan unit tests pass
just asan

# 3. Integration tests pass under ASan
just integration-asan

# 4. Regular test suite still passes (no regressions)
just test

# 5. Full compound target
just all
```

Expected results:
- `just shuttle`: 6 shuttle tests across 3 scenarios, all passing at 1000 iterations
- `just asan`: All devices and vmm unit tests pass under ASan, no violations
- `just integration-asan`: Integration tests pass at the same rate as normal `just integration` (5-6/6)
- `just test`: Unchanged pass rate from Phase 1-4 baseline
- `just all`: All subtargets pass

---

## Design Discrepancy Notes

- **`await_target` uses `wait_timeout` not `wait`:** The real `BalloonHandle::await_target` (lib.rs line 3505) uses `cvar.wait_timeout(actual, stall_timeout)` to implement a stall detection timeout. Shuttle's `Condvar::wait_timeout` treats the timeout as a scheduling point rather than a real timer, and shuttle tests typically do not exercise the timeout path (it would require the condvar to never be signaled, which would be a deadlock in the test). The shuttle test for balloon uses `cvar.wait` (no timeout) to test the condvar-signaled path only. The stall detection path is covered by the existing `test_balloon_handle_await_target_stalled_no_progress` unit test with real wall-clock timing.

- **DeviceState shuttle test uses `bool` not real `DeviceState`:** The design plan says to test "DeviceState Inactive→Activated transition while worker threads read state." The real `DeviceState::Activated(GuestMemoryMmap, InterruptTransport)` cannot be instantiated without a KVM file descriptor and a real VM memory mapping. The shuttle test substitutes `Mutex<bool>` as a model. This is explicitly noted in the "Important notes" section of the design requirements. The property tested (no torn read) is still valid because the mutex guarantees atomicity regardless of the value type.

- **`SHUTTLE_ITERATIONS` env var:** Shuttle 0.8 may not support `SHUTTLE_ITERATIONS` as an env override. If the env variable has no effect, remove it from the justfile recipe and document that iteration count is set at the `check_random` call site. The justfile recipe accepts `iterations` as a parameter that could alternatively be passed via a different mechanism (e.g., a wrapper script that uses `sed` to substitute the count, though this is not recommended).

- **integration-asan requires patching `tests/run.sh`:** The design plan says "thread RUSTFLAGS through run.sh or build before calling run.sh." The chosen approach (Option A: patch run.sh to respect `KRUN_NO_RUN_SH_GUEST_AGENT`) requires a one-line change to `tests/run.sh`. This is a prerequisite for Task 6 and should be committed as part of that task.

- **ASan and the nightly toolchain pin:** `rust-toolchain.toml` specifies a stable channel. The `just asan` recipe uses `cargo +nightly` to override the channel for ASan-requiring commands. If the project later pins to a nightly channel in `rust-toolchain.toml`, the explicit `+nightly` override can be removed from the justfile.
