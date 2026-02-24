# Snapshot Completeness Implementation Plan — Phase 1

**Goal:** Extract shared 16550 UART implementation so Snapshottable can be added once in Phase 2.

**Architecture:** Move the identical Serial struct, BusDevice impl, Subscriber impl, and tests from x86_64/serial.rs into a new shared `serial_16550.rs` module at the legacy level. The x86_64 and riscv64 serial modules become thin re-exports.

**Tech Stack:** Rust (devices crate, no new dependencies)

**Scope:** 7 phases from original design (this is phase 1 of 7)

**Codebase verified:** 2026-02-24

---

## Acceptance Criteria Coverage

This phase is infrastructure (code deduplication). It introduces no new functionality and verifies operationally.

**Verifies:** None — this phase restructures existing code without changing behavior.

---

<!-- START_TASK_1 -->
### Task 1: Create shared serial_16550.rs module

**Files:**
- Create: `src/devices/src/legacy/serial_16550.rs`

**Implementation:**

Create the new shared module containing the full 16550 UART implementation. This is the complete contents of `src/devices/src/legacy/x86_64/serial.rs` (lines 1-563), verbatim. The file already uses `crate::bus::BusDevice` and `crate::legacy::ReadableFd` imports, which resolve correctly from the `legacy/` directory level.

The file contains:
- All register constants (DATA through DEFAULT_BAUD_DIVISOR)
- `pub struct Serial` with all fields
- `impl Serial` (constructors + register logic)
- `impl BusDevice for Serial` (read/write handlers)
- `impl Subscriber for Serial` (epoll event handling)
- `#[cfg(test)] mod tests` with SharedBuffer mock + 9 test functions

Copy the entire file from `src/devices/src/legacy/x86_64/serial.rs` without modification.

**Verification:**

This file won't compile yet on its own — it needs module declaration in Task 2.

**Commit:** Do not commit yet — continue to Task 2.
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Wire serial_16550 module into legacy/mod.rs

**Files:**
- Modify: `src/devices/src/legacy/mod.rs:27-45`

**Implementation:**

Add the `serial_16550` module declaration. It must NOT be cfg-gated — this module is shared across x86_64 and riscv64. Add it near the other module declarations (e.g., after the `rtc_pl031` line).

Add this line after line 27 (`mod rtc_pl031;`):

```rust
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
mod serial_16550;
```

Note: We gate on x86_64 OR riscv64 (not aarch64) because aarch64 uses PL011, a completely different serial architecture. The serial_16550 module has no purpose on aarch64.

**Verification:**

Not verifiable yet on its own — continue to Task 3.

**Commit:** Do not commit yet — continue to Task 3.
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Replace x86_64/serial.rs with re-export

**Files:**
- Modify: `src/devices/src/legacy/x86_64/serial.rs` (replace entire contents)

**Implementation:**

Replace the entire file contents with a single re-export line:

```rust
pub use crate::legacy::serial_16550::Serial;
```

This preserves the module path `legacy::x86_64::serial::Serial` that `legacy/mod.rs` imports via `use x86_64::serial;` and re-exports as `pub use self::serial::Serial;`.

**Verification:**

Not verifiable yet on its own — continue to Task 4.

**Commit:** Do not commit yet — continue to Task 4.
<!-- END_TASK_3 -->

<!-- START_TASK_4 -->
### Task 4: Replace riscv64/serial.rs with re-export

**Files:**
- Modify: `src/devices/src/legacy/riscv64/serial.rs` (replace entire contents)

**Implementation:**

Replace the entire file contents with a single re-export line:

```rust
pub use crate::legacy::serial_16550::Serial;
```

Same pattern as x86_64. The module path `legacy::riscv64::serial::Serial` is preserved.

**Verification:**

Run: `cargo build -p devices`

Expected: Build succeeds. The Serial type is resolved through the re-export chain: `legacy::Serial` → `legacy::{x86_64,riscv64}::serial::Serial` → `legacy::serial_16550::Serial`.

Run: `cargo test -p devices --features net`

Expected: All existing serial tests pass. The 9 test functions (test_event_handling_no_in, test_event_handling_with_in, test_serial_output, test_serial_raw_input, test_serial_input, test_serial_thr, test_serial_dlab, test_serial_modem, test_serial_scratch) now run from `serial_16550.rs` instead of `x86_64/serial.rs`.

**Commit:** `refactor: extract shared 16550 serial to serial_16550.rs`
<!-- END_TASK_4 -->
