# Testing Upgrade Design

## Summary

libkrun is a library that boots lightweight Linux virtual machines as a callable API. It exposes both a C API (24 `extern "C"` functions) and a Rust Builder API, but the C API creates a boundary that blocks modern Rust testing tools: Miri cannot cross it, property-based testing frameworks cannot generate values through it, and concurrency model checkers require direct access to Rust primitives. This upgrade removes the C API entirely and then applies a layered set of testing tools against the exposed Rust code.

The approach is refactor-first, then instrument. Phase 1 removes the C API and replaces the Makefile with a `justfile` as the single build and test entry point. Phase 2 extracts pure Rust logic — bitmap tracking, request parsing, FUSE dispatch — out of modules that call into the Linux kernel (via KVM ioctls, UFFD, and FUSE), since those syscall boundaries are the second barrier that tools like Miri and loom cannot cross. With clean pure-logic modules available, Phases 3–6 apply tools in order of what they can see: Miri and loom on the extracted pure logic, cargo-fuzz and ASan on the unsafe syscall boundaries where Miri cannot reach, Kani for bounded formal proofs on critical invariants, and Shuttle for randomized concurrency testing of complex multi-threaded coordination. Phase 7 fills functional gaps in the integration test suite — particularly combined feature interactions (balloon + snapshot + UFFD, virtiofs + DAX + snapshot) and custom trait contracts. Phase 8 runs mutation testing to measure how well the new tests catch real bugs.

## Definition of Done

1. **C API removed** — the 24 `pub extern "C"` functions and any C-only support code are deleted from the libkrun crate, leaving a clean Rust-only API surface.

2. **Testing infrastructure integrated** — all 7 tools (ASan, cargo-fuzz, cargo-mutants, loom, Miri, proptest, Kani) are set up with justfile targets, fuzz harnesses, loom wrappers, Kani proofs, and proptest strategies written for the target feature set (`embedded_init + snapshot + uffd + blk + vhost-user`, Linux x86_64).

3. **Coverage gaps filled** — new functional tests added for identified gaps: combined feature interaction tests, deeper custom trait contract testing (AsyncBlockBackend, FileSystem, SnapshotStore), concurrency stress tests, and advanced error path coverage.

4. **Refactoring done where needed** — pure logic extracted from syscall-dependent code to enable Miri and loom testing (e.g., descriptor parsing, dirty bitmap logic, queue state machines).

5. **Justfile as test runner** — all test workflows accessible via `just` targets (unit tests, integration tests, fuzz, mutants, miri, loom, kani, asan).

## Acceptance Criteria

### testing-upgrade.AC1: C API removed
- **testing-upgrade.AC1.1 Success:** No `pub extern "C"` functions exist in `src/libkrun/src/lib.rs`
- **testing-upgrade.AC1.2 Success:** `Builder`, `Context`, `VmHandle`, and all trait re-exports compile and are publicly accessible from Rust
- **testing-upgrade.AC1.3 Success:** `just check` passes with no C API symbols in the compiled library
- **testing-upgrade.AC1.4 Edge:** Removing C API does not break integration tests (they use Rust API)

### testing-upgrade.AC2: Testing infrastructure integrated
- **testing-upgrade.AC2.1 Success:** `just miri` runs Miri on all pure-logic modules and passes
- **testing-upgrade.AC2.2 Success:** `just fuzz-list` shows 5 fuzz targets; `just fuzz <target>` builds and runs each
- **testing-upgrade.AC2.3 Success:** `just loom` runs exhaustive concurrency tests on DirtyBitmap, ReclaimedBitmap, PageTracker and passes
- **testing-upgrade.AC2.4 Success:** `just shuttle` runs randomized concurrency tests on block quiesce, balloon resize, device activation and passes
- **testing-upgrade.AC2.5 Success:** `just proptest` runs property tests for snapshot round-trips, bitmap invariants, GDT, address translation, builder validation and passes
- **testing-upgrade.AC2.6 Success:** `just kani` runs all bounded proofs and they verify
- **testing-upgrade.AC2.7 Success:** `just asan` runs unit tests with AddressSanitizer and passes
- **testing-upgrade.AC2.8 Success:** `just integration-asan` runs integration tests with AddressSanitizer and passes
- **testing-upgrade.AC2.9 Success:** `just mutants` produces a mutation testing report with scored results
- **testing-upgrade.AC2.10 Failure:** `just fuzz-all` with a 60-second duration produces no crashes on any target
- **testing-upgrade.AC2.11 Edge:** All justfile targets use the same feature set variable (`embedded_init,snapshot,uffd,blk,vhost-user`)

### testing-upgrade.AC3: Coverage gaps filled
- **testing-upgrade.AC3.1 Success:** Combined balloon+snapshot+UFFD test passes: inflate → snapshot → restore with UFFD → reclaimed pages zero-filled → deflate → guest reuses memory
- **testing-upgrade.AC3.2 Success:** Combined block+snapshot+UFFD test passes: write via custom backend → snapshot → restore with UFFD → read back matches
- **testing-upgrade.AC3.3 Success:** Combined virtiofs(DAX)+snapshot test passes: mount custom FileSystem → write → snapshot → restore → read back matches
- **testing-upgrade.AC3.4 Success:** Full stack test passes: TSI connection + balloon + snapshot → restore → TSI resumes, balloon preserved
- **testing-upgrade.AC3.5 Success:** Failing block backend test: guest receives I/O errors, device doesn't crash, other requests unaffected
- **testing-upgrade.AC3.6 Success:** Slow block backend test: queue doesn't stall, metrics track correctly
- **testing-upgrade.AC3.7 Success:** Minimal FileSystem test: guest gets ENOSYS for unsupported ops, device doesn't crash, DAX enabled
- **testing-upgrade.AC3.8 Success:** Concurrent balloon resize + snapshot stress test: no race in excluded pages collection
- **testing-upgrade.AC3.9 Success:** Parallel UFFD faults with balloon-reclaimed pages: zero-fill path doesn't race with demand-paging

### testing-upgrade.AC4: Refactoring complete
- **testing-upgrade.AC4.1 Success:** `PageTracker` exists in `src/vmm/src/uffd/page_tracker.rs` with no syscall dependencies
- **testing-upgrade.AC4.2 Success:** Block request types exist in `src/devices/src/virtio/block/request.rs` with no async runtime dependency
- **testing-upgrade.AC4.3 Success:** FUSE dispatch exists in `src/devices/src/virtio/fs/fuse_dispatch.rs` with no FileSystem backend dependency
- **testing-upgrade.AC4.4 Success:** `#[cfg(loom)]` atomic import shims present in `dirty_bitmap.rs`, `reclaimed_bitmap.rs`, `page_tracker.rs`
- **testing-upgrade.AC4.5 Success:** `just test` passes after all extractions (behavior-preserving refactoring)
- **testing-upgrade.AC4.6 Edge:** Miri spike result documented: GuestMemoryMmap either works under Miri (descriptor_utils gets Miri coverage) or doesn't (descriptor_utils tested via fuzzing+ASan only)

### testing-upgrade.AC5: Justfile as test runner
- **testing-upgrade.AC5.1 Success:** `Makefile` does not exist at project root
- **testing-upgrade.AC5.2 Success:** `justfile` exists at project root with all targets documented in Section 9
- **testing-upgrade.AC5.3 Success:** `just build` produces the release library (replaces `make`)
- **testing-upgrade.AC5.4 Success:** `just integration <name>` runs a single named integration test
- **testing-upgrade.AC5.5 Success:** `just all` runs test + miri + proptest + loom + shuttle as a compound target
- **testing-upgrade.AC5.6 Success:** `just safety` runs asan + miri + fuzz-all + kani as a compound target

## Glossary

- **ASan (AddressSanitizer)**: Compile-time instrumentation that detects memory errors at runtime — buffer overflows, use-after-free, heap corruption. Activated with `RUSTFLAGS="-Zsanitizer=address"`. Requires nightly Rust.
- **AsyncBlockBackend**: Trait in the `devices` crate defining an interface for block storage backends. Custom implementations fulfill this contract to give the virtual block device different behaviors in tests.
- **balloon (virtio-balloon)**: Virtual device that allows the hypervisor to reclaim guest memory at runtime. The guest inflates the balloon by lending pages back to the host; deflating returns them.
- **bincode**: Rust serialization library encoding data in compact binary format. Used for snapshot serialization.
- **ByteValued**: Trait from `vm-memory` marking a type as safe to interpret from raw bytes. Used to parse hardware-format request headers from guest memory.
- **cargo-fuzz**: Rust fuzzing tool integrating with libFuzzer for coverage-guided fuzz testing. Requires nightly.
- **cargo-mutants**: Mutation testing tool that modifies source code and checks whether tests detect each change. Mutation score measures fraction of mutations caught.
- **Condvar**: Rust condition variable, used with a `Mutex` for one thread to block until another signals a state change. Appears in block worker quiesce and balloon resize coordination.
- **DAX (Direct Access)**: Virtio-fs mode mapping guest filesystem directly into guest physical address space, bypassing the virtio queue for data. Eliminates per-request copying.
- **DescriptorChain**: Linked list of memory descriptors in a virtio queue ring. Guest drivers scatter request data across memory regions and chain them; host walks the chain via `Reader`/`Writer`.
- **DirtyBitmap / ReclaimedBitmap**: Bitmap structures tracking which guest pages are dirty (written since last snapshot) or reclaimed (lent to balloon). Use atomic operations for concurrent access.
- **FUSE (Filesystem in Userspace)**: Protocol for implementing filesystems in userspace. Virtio-fs speaks FUSE between guest kernel and host `FileSystem` trait implementation.
- **FileSystem**: Trait abstracting backing storage for virtiofs. `PassthroughFs` is built-in; custom implementations can be plugged in.
- **GDT (Global Descriptor Table)**: x86 structure describing memory segments. `gdt.rs` contains pure arithmetic for building/reading entries.
- **GuestMemoryMmap**: Concrete type from `vm-memory` backing guest physical memory with mmap regions. Whether it works under Miri determines descriptor chain coverage path.
- **Kani**: Bounded model checker for Rust. Exhaustively verifies correctness up to configurable bounds on loops and data size. Ships own toolchain.
- **KVM (Kernel-based Virtual Machine)**: Linux kernel subsystem exposing hardware virtualization as `ioctl` calls. libkrun uses KVM to create/run vCPUs and manage guest memory.
- **loom**: Exhaustive concurrency testing library. Replaces standard atomics with instrumented versions and explores every thread interleaving. Practical only for small units.
- **Miri**: Rust MIR interpreter checking for undefined behavior. Cannot run code that calls into the OS kernel.
- **PageTracker**: Module tracking which guest pages are loaded during UFFD restore, using bitmaps and atomic counters for deduplication.
- **proptest**: Property-based testing library. Developer writes strategies generating arbitrary values; proptest runs tests with many inputs and shrinks failures.
- **Shuttle**: Randomized concurrency testing library. Samples thread interleavings, scaling to more complex scenarios than loom's exhaustive approach.
- **SnapshotStore**: Trait abstracting snapshot storage backend. `FsSnapshotStore` writes to directories; `MockSnapshotStore` is a test double.
- **TSI (Transparent Socket Impersonation)**: Networking mechanism giving guests outbound TCP/UDP via vhost-user vsock proxy without TAP devices.
- **UFFD (userfaultfd)**: Linux mechanism for userspace page fault handling. Used during snapshot restore for demand-paging — pages load on first access.
- **vhost-user**: Protocol for moving virtio backends into separate daemons communicating over Unix sockets. Used for virtiofs (with DAX) and vsock.
- **virtio**: Standardized paravirtualized I/O interface. Guest drivers communicate with host implementations through shared memory rings (virtqueues).
- **VM exit**: Transition from guest execution to hypervisor when guest triggers an event the host must handle.

## Architecture

Refactor-first approach: extract pure logic from syscall-dependent modules, then apply testing tools optimally to clean code. Eight implementation phases, sequenced so each builds on the previous.

**Target feature set:** `embedded_init + snapshot + uffd + blk + vhost-user` on Linux x86_64. All justfile targets compile with this feature combination. Code behind other feature gates (GPU, sound, input, TEE, EFI, HVF, multi-arch) is untouched — existing `#[cfg]` gates make it invisible to all tools.

**Tool allocation by code category:**

| Code Category | Tools Applied | Why |
|---------------|---------------|-----|
| Pure logic (bitmaps, validation, parsing) | Miri, proptest, loom, Kani | No syscall barriers; full tool coverage |
| Unsafe syscall wrappers (UFFD, KVM, FUSE) | cargo-fuzz, ASan | Can't run under Miri; fuzzing + ASan catch real UB at runtime |
| Concurrent state (atomics, Mutex+Condvar) | Loom (exhaustive, small primitives), Shuttle (randomized, larger coordination) | Complementary: loom proves small things, shuttle finds bugs in complex scenarios |
| Integration paths (full VM lifecycle) | ASan overlay on existing tests, new combined-feature tests | Tests real execution with memory error detection |
| Test quality measurement | cargo-mutants | Run last; measures effectiveness of everything above |

**Justfile replaces Makefile** as the single entry point for all build and test workflows. Feature set defined once as a justfile variable, referenced by all targets.

## Existing Patterns

Investigation of the libkrun codebase revealed these patterns that the design follows:

**Unit test structure:** `#[cfg(test)] mod tests` with helper fixture functions (e.g., `make_memory()`, `valid_header()` in `src/vmm/src/snapshot.rs`). Individual test functions can be feature-gated with `#[cfg(feature = "snapshot")]`. This design follows the same pattern — new proptest strategies and loom tests live inside existing `#[cfg(test)]` modules.

**Mock object pattern:** Traits implemented on test structs with `Arc<Mutex<Vec<...>>>` for operation tracking and `AtomicU64` for metrics. Examples: `TrackingBackend` in `src/devices/src/virtio/block/async_worker.rs`, `MockSnapshotStore` in `tests/test_cases/src/mock_snapshot_store.rs`. New test backends (FailingBlockBackend, SlowBlockBackend, MinimalFileSystem) follow this pattern.

**test_utils feature:** `#[cfg(any(test, feature = "test_utils"))]` makes mock helpers available to downstream crates (e.g., `DummyIrqChip` in `src/devices/src/legacy/irqchip.rs`). Extracted modules can use this pattern if needed.

**Integration test host/guest split:** `#[host]` and `#[guest]` proc macros in `tests/macros/` with mutually exclusive features. New integration tests follow this pattern. Test daemons (`tests/test_daemon/`, `tests/test_vsock_proxy/`) provide external processes for vhost-user testing.

**No existing use of:** proptest, cargo-fuzz, loom, shuttle, Miri, Kani, or cargo-mutants. This design introduces all seven as new patterns.

## Implementation Phases

<!-- START_PHASE_1 -->
### Phase 1: C API Removal + Justfile

**Goal:** Remove the C API (sole source of testing friction) and replace Makefile with justfile.

**Components:**
- `src/libkrun/src/lib.rs` — delete all 24 `pub extern "C" fn krun_*` functions, the global `CTX_MAP`/`CTX_IDS` context dictionary, `#[no_mangle]` attributes, and libc error-code return patterns. Keep `Builder`, `Context`, `VmHandle`, all device handles, all trait re-exports, `ContextConfig` (internal).
- `Makefile` — delete entirely
- `justfile` (new) — create at project root with `check`, `build`, `test`, `integration` targets. Define `features` variable once. Migrate `make test` logic and `tests/run.sh` invocation.

**Dependencies:** None (first phase)

**Done when:** `just check` passes, `just test` passes, `just integration` passes. No `pub extern "C"` functions remain in libkrun crate. Makefile is deleted.
<!-- END_PHASE_1 -->

<!-- START_PHASE_2 -->
### Phase 2: Pure Logic Extraction

**Goal:** Separate pure Rust logic from syscall-dependent code to enable Miri and loom testing.

**Components:**
- `src/vmm/src/uffd.rs` → extract `PageTracker` (bitmap ops, statistics, `LoadSource` tracking) and address translation functions (`guest_to_host`, `guest_addr_to_page_index`, `is_eexist`) into `src/vmm/src/uffd/page_tracker.rs`. `UffdHandler` stays in `src/vmm/src/uffd/handler.rs`.
- `src/devices/src/virtio/block/async_worker.rs` → extract `RequestHeader`, `Request` enum, `DiscardWriteData`, `ParsedRequest`, `QueuedWrite`, `BatchWriteResult`, `AsyncWorkerMetrics` into `src/devices/src/virtio/block/request.rs`.
- `src/devices/src/virtio/fs/server.rs` → extract FUSE opcode dispatch and message header parsing into `src/devices/src/virtio/fs/fuse_dispatch.rs`.
- `src/vmm/src/dirty_bitmap.rs`, `src/devices/src/virtio/balloon/reclaimed_bitmap.rs`, extracted `page_tracker.rs` — add `#[cfg(loom)]` / `#[cfg(not(loom))]` atomic import shims.
- **Spike task:** write one unit test using `GuestMemoryMmap::from_ranges` and run under `cargo +nightly miri test` to determine if vm-memory works under Miri. Result determines whether descriptor_utils gets Miri coverage or is tested via fuzzing only.

**Dependencies:** Phase 1 (justfile exists for running checks)

**Done when:** `just test` passes (refactoring is behavior-preserving). Extracted modules compile independently of their syscall-dependent siblings.
<!-- END_PHASE_2 -->

<!-- START_PHASE_3 -->
### Phase 3: Miri + proptest + Loom

**Goal:** Apply pure-logic testing tools to extracted modules.

**Components:**

*Miri targets:*
- `src/vmm/src/dirty_bitmap.rs` — all existing unit tests
- `src/devices/src/virtio/balloon/reclaimed_bitmap.rs` — all existing unit tests
- `src/arch/src/x86_64/gdt.rs` — all existing unit tests
- `src/vmm/src/snapshot.rs` — validation and bincode round-trip tests (non-I/O)
- `src/vmm/src/uffd/page_tracker.rs` — address translation, bitmap logic
- `src/devices/src/virtio/block/request.rs` — RequestHeader parsing, batch aggregation
- Conditionally: `src/devices/src/virtio/descriptor_utils.rs` (if Miri spike succeeds)

*proptest strategies (added as dev-dependency to vmm, devices, libkrun):*
- Snapshot round-trips: arbitrary `SnapshotHeader`, `VmSnapshot`, `IncrementalSnapshot`, `DirtyPage` through bincode serialize/deserialize
- Bitmap invariants: mark N random pages → drain returns exactly N; mark_range → count matches; mark_loaded deduplication
- GDT properties: `get_base(gdt_entry(flags, base, limit)) == base` for all valid inputs
- Address translation: guest_to_host returns Some for in-range, None for out-of-range
- Builder validation: zero vCPUs → ZeroVcpus error, tag >36 bytes → TagTooLong

*Loom tests (added as dev-dependency to vmm, devices):*
- `DirtyBitmap`: concurrent `mark_dirty` (vCPU thread) + `drain_dirty_pages` (snapshot thread). Property: no page lost across drain boundary. Verifies `swap(0, AcqRel)` ordering.
- `ReclaimedBitmap`: concurrent `mark` + `clear` + `iter_set_pages`/`count`. Property: count never inconsistent with set bits.
- `PageTracker`: concurrent `mark_loaded(Preload)` + `mark_loaded(Fault)` on same page. Property: exactly one counter increment per unique page (deduplication via `fetch_or`).

*Justfile targets:* `just miri`, `just proptest`, `just proptest-long`, `just loom`

**Dependencies:** Phase 2 (extracted modules with loom shims)

**Done when:** `just miri` passes, `just proptest` passes, `just loom` passes.
<!-- END_PHASE_3 -->

<!-- START_PHASE_4 -->
### Phase 4: Fuzzing

**Goal:** Set up cargo-fuzz with harnesses targeting unsafe boundaries.

**Components:**

*Infrastructure:*
- `fuzz/` directory at project root with `cargo-fuzz` configuration
- `fuzz/Cargo.toml` depending on devices and vmm crates with target features

*Fuzz targets in `fuzz/fuzz_targets/`:*
- `fuzz_fuse_parsing.rs` — feed arbitrary bytes as FUSE request through `Server::handle_message()` with a mock `FileSystem` impl returning canned responses. Targets 62 unsafe blocks in `passthrough.rs` via the FUSE dispatch layer.
- `fuzz_descriptor_chain.rs` — write random bytes into descriptor table region of `GuestMemoryMmap`, call `DescriptorChain::checked_new()` and iterate with `Reader`/`Writer`.
- `fuzz_snapshot_deser.rs` — feed arbitrary bytes to `bincode::deserialize::<VmSnapshot>()` and `bincode::deserialize::<IncrementalSnapshot>()`, then run `validate_header_for_vm()`.
- `fuzz_vhost_user_msg.rs` — mock Unix socket with arbitrary bytes, drive vhost-user message parsing.
- `fuzz_block_request.rs` — write random bytes where RequestHeader lives, exercise request type discrimination.

*Justfile targets:* `just fuzz <target>`, `just fuzz-all [duration=60]`, `just fuzz-list`, `just fuzz-corpus <target>`

**Dependencies:** Phase 2 (extracted FUSE dispatch for harness construction)

**Done when:** All 5 fuzz targets build. Initial 60-second runs produce no crashes. Corpora seeded from existing test data.
<!-- END_PHASE_4 -->

<!-- START_PHASE_5 -->
### Phase 5: ASan + Shuttle

**Goal:** Runtime memory error detection on integration tests and randomized concurrency testing for complex coordination.

**Components:**

*ASan:*
- Justfile targets `just asan` and `just integration-asan` wrapping `RUSTFLAGS="-Zsanitizer=address" cargo +nightly test --target x86_64-unknown-linux-gnu`
- Applied to all existing unit tests and all 27+ integration tests (real VMs with ASan instrumentation)
- ASan is also enabled automatically inside cargo-fuzz/libFuzzer

*Shuttle tests (added as dev-dependency to vmm, devices):*
- Block worker quiesce handshake: `Arc<(Mutex<bool>, Condvar)>` pause/resume between VMM control plane and async block worker. Verifies no deadlock, no missed wakeup.
- Balloon resize + stats polling: VMM thread calls `BalloonHandle::resize()` + `await_target()`, guest-simulated thread updates `actual`. Verifies condvar wait doesn't deadlock.
- Device activation/deactivation: `DeviceState` Inactive→Activated transition while worker threads read state. Verifies no torn reads.

*Justfile targets:* `just asan`, `just integration-asan`, `just shuttle [iterations=1000]`

**Dependencies:** Phase 1 (justfile), no dependency on extraction phases (ASan runs on existing code)

**Done when:** `just asan` passes, `just integration-asan` passes, `just shuttle` passes.
<!-- END_PHASE_5 -->

<!-- START_PHASE_6 -->
### Phase 6: Kani Proofs

**Goal:** Bounded formal verification proofs for critical unsafe and correctness-sensitive functions.

**Components:**
- `kani-proofs/` directory at project root with Kani harness files
- 5–7 proofs with `#[kani::proof]` annotation:
  - PageTracker deduplication: `mark_loaded` on same page twice increments counter exactly once. Bound: ≤128 pages.
  - DirtyBitmap bounds: `mark_dirty` with any `u64` never panics or writes out of bounds. Bound: ≤256 pages.
  - ReclaimedBitmap consistency: `mark` → `is_set` returns true; `clear` → `is_set` returns false; `count` equals popcount. Bound: ≤256 pages.
  - Snapshot header validation: `validate_header_for_vm` rejects all invalid magic, version, region count, vcpu count. Uses `kani::any()`.
  - GDT round-trip: `get_base(gdt_entry(flags, base, limit)) == base` and `get_limit` equivalent for all inputs.
  - Address translation: `guest_to_host` returns correct offset for in-range, None for out-of-range. Bound: ≤4 regions.
  - (Stretch) Block RequestHeader: ByteValued deserialization handles all 16-byte inputs without UB.
- Syscall stubs via `kani::stub` for any KVM/libc dependencies pulled in transitively

*Justfile targets:* `just kani`, `just kani-proof <name>`

**Dependencies:** Phase 2 (extracted modules for clean harness construction)

**Done when:** All proofs verify. `just kani` completes successfully.
<!-- END_PHASE_6 -->

<!-- START_PHASE_7 -->
### Phase 7: New Integration Tests

**Goal:** Fill coverage gaps with combined-feature, trait-contract, and stress tests.

**Components:**

*Combined feature tests in `tests/test_cases/src/`:*
- `test_balloon_snapshot_uffd.rs` — inflate balloon → full snapshot → restore with UFFD → verify reclaimed pages zero-filled → deflate → guest uses reclaimed memory
- `test_block_snapshot_uffd.rs` — write via custom AsyncBlockBackend → snapshot → restore with UFFD → read back → verify consistency
- `test_virtiofs_dax_snapshot.rs` — mount custom FileSystem impl with DAX enabled → write file → snapshot → restore → read file back
- `test_full_stack.rs` — vhost-user vsock + balloon + snapshot: establish TSI connection → inflate balloon → snapshot → restore → verify TSI resumes and balloon state preserved

*Trait contract tests:*
- `test_block_backend_errors.rs` — custom backend returning errors on specific sectors; verify guest sees I/O errors, device doesn't crash
- `test_block_backend_slow.rs` — custom backend with artificial delays; verify timeout handling, queue doesn't stall
- `test_virtiofs_minimal.rs` — FileSystem impl with only lookup+read (ENOSYS otherwise); DAX enabled; verify guest gets proper errors

*Stress tests:*
- `test_balloon_snapshot_race.rs` — rapidly alternate inflate/deflate during snapshot; verify no race in excluded pages collection
- `test_uffd_balloon_parallel.rs` — multiple vCPUs faulting on pages including balloon-reclaimed addresses; verify zero-fill path doesn't race with demand-paging

*New test helpers in `tests/test_cases/src/`:*
- `failing_block_backend.rs` — AsyncBlockBackend impl returning errors on configured sectors
- `slow_block_backend.rs` — AsyncBlockBackend impl with configurable delays
- `minimal_filesystem.rs` — FileSystem impl with minimal operations, ENOSYS for unsupported ops

**Dependencies:** Phases 1–2 (justfile, working test infrastructure)

**Done when:** `just integration` passes with all new tests.
<!-- END_PHASE_7 -->

<!-- START_PHASE_8 -->
### Phase 8: Mutation Testing Baseline

**Goal:** Establish test quality baseline and identify remaining weak spots.

**Components:**
- cargo-mutants configuration: `--features embedded_init,snapshot,uffd,blk,vhost-user` with exclusions `-e 'src/rutabaga_gfx/*'` `-e 'src/hvf/*'` `-e 'src/devices/src/virtio/gpu/*'` `-e 'src/devices/src/virtio/snd/*'` `-e 'src/devices/src/virtio/input/*'`
- `#[mutants::skip]` annotations on intentionally untested functions (debug Display impls, unreachable error formatting)
- Baseline mutation score documented
- Justfile targets: `just mutants`, `just mutants-diff`

**Dependencies:** All previous phases (mutants measures the combined effectiveness of all testing)

**Done when:** `just mutants` produces a report. Baseline mutation score recorded. Surviving mutants triaged (genuine gaps vs. acceptable skips).
<!-- END_PHASE_8 -->

## Additional Considerations

**Miri spike determines descriptor_utils coverage path.** If `GuestMemoryMmap::from_ranges` works under Miri (it uses `MAP_PRIVATE|MAP_ANONYMOUS` which Miri supports), descriptor chain logic gets Miri + proptest coverage for free. If not, that code is covered by fuzzing + ASan instead. No speculative generic refactor over `GuestMemory` trait — the spike decides.

**Test flakiness.** Existing integration tests are inherently flaky (timing-sensitive VM + network tests). cargo-mutants may see spurious failures. The `-e` exclusion flags and `#[mutants::skip]` annotations mitigate noise, but some manual triage of survived mutants will be needed.

**Nightly toolchain requirement.** Miri, ASan, and cargo-fuzz require nightly Rust. Kani ships its own toolchain. The justfile targets that need nightly use `cargo +nightly` explicitly. Standard `just test` uses the stable toolchain from `rust-toolchain.toml`.
