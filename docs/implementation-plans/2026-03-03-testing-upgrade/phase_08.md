# Phase 8: Mutation Testing Baseline

**Goal:** Establish a test quality baseline by running cargo-mutants across the codebase, triaging surviving mutants, annotating intentionally untested functions, and recording the baseline score.

**Design reference:** `docs/design-plans/2026-03-03-testing-upgrade.md` — `<!-- START_PHASE_8 -->`

**Scope:** Phase 8 of 8 (final phase)

**Codebase verified:** 2026-03-03

---

## Acceptance Criteria Coverage

This phase implements and tests:

### testing-upgrade.AC2.9: Mutation testing baseline
- **testing-upgrade.AC2.9 Success:** `just mutants` runs cargo-mutants with the full feature set and produces `mutants.out/outcomes.json`
- **testing-upgrade.AC2.9 Success:** `just mutants-diff` runs cargo-mutants scoped to changed files vs `origin/main`
- **testing-upgrade.AC2.9 Success:** Baseline mutation score is recorded in `docs/mutation-baseline.md`
- **testing-upgrade.AC2.9 Success:** Surviving mutants are triaged with documented justification for each category
- **testing-upgrade.AC2.9 Edge:** `#[mutants::skip]` is applied only where code is genuinely unreachable or intentionally untested; not used to hide coverage gaps

**Done when:** `just mutants` produces a report, baseline mutation score is recorded in `docs/mutation-baseline.md`, and surviving mutants are triaged with documented categories.

**Dependencies:** All previous phases complete:
- Phase 1: C API removed; justfile established
- Phase 2: Pure-logic modules extracted
- Phase 3: Miri, proptest, loom tests added
- Phase 4: (shuttle / additional concurrency tests)
- Phase 5: (fuzzing targets)
- Phase 6: (Kani proofs)
- Phase 7: (coverage reporting)

---

## Investigation Findings

### Expected surviving mutant categories

Mutation testing will almost certainly leave some mutants surviving. The following categories are expected and acceptable — they do not represent test gaps that need to be closed:

**1. `fmt::Display` / `fmt::Debug` error message text**
Any mutant that changes the string content of an error message (e.g., changing `"invalid magic"` to `""` or `"xnvalid magic"`) will survive unless tests assert on exact error message text. This is intentional: tests should assert on error *types*, not formatted strings.

**2. Unreachable platform branches (`#[cfg(target_os = ...)]`)**
Code gated behind `#[cfg(target_os = "macos")]` is unreachable on Linux CI. cargo-mutants may generate mutants from these blocks that no test can catch because the mutants simply don't compile on Linux. These appear as `unviable` in the report (compile failure) rather than `missed`, so they are noise but not a real coverage concern.

**3. Logging and tracing calls (`log::debug!`, `log::trace!`, `log::info!`)**
Mutants that delete or alter log macro arguments survive because tests do not assert on log output. This is acceptable.

**4. Metrics and statistics counters (non-behavioral)**
Incrementing a statistics counter by 1 vs 2 in `PageTracker::stats()` or similar may produce a surviving mutant if no test asserts the exact counter value in a boundary case. These are low-priority gaps.

**5. Excluded subsystems**
The following subsystems are excluded from the cargo-mutants run and will have no mutants generated:
- `src/rutabaga_gfx` — GPU virtualization, no unit tests exist
- `src/hvf` — macOS HVF backend, not runnable on Linux CI
- `src/devices/src/virtio/gpu` — GPU device, no unit tests
- `src/devices/src/virtio/snd` — Sound device, no unit tests
- `src/devices/src/virtio/input` — Input device, no unit tests

**6. FFI boundary functions**
Functions that directly call into KVM ioctls or libc cannot be unit-tested without a real KVM device. Mutants in these functions survive by design. Annotate with `#[mutants::skip]` only if the ioctl wrapper has no testable logic beyond the call itself.

### cargo-mutants outcome taxonomy

| Outcome | Meaning | Action |
|---------|---------|--------|
| `caught` | At least one test failed on this mutant — good | None needed |
| `missed` | All tests passed on this mutant — coverage gap | Triage: add test or skip with justification |
| `unviable` | Mutant did not compile | Noise; ignore |
| `timeout` | Test suite hung | Investigate; likely infinite-loop mutant in hot path |

### Expected total mutant count

For a codebase of this size (~15k lines of non-excluded Rust), expect:
- 1,000–3,000 total mutants generated
- 60–80% caught (optimistic baseline after all earlier phases)
- 10–20% unviable (won't compile)
- 5–15% missed (the interesting ones to triage)
- 1–3% timeout (set `--timeout 60` to cap these)

A first-pass baseline score of 65–75% caught is realistic. The goal of this phase is to *measure and document* the score, not to reach a specific threshold.

---

## Task 1: Install and verify cargo-mutants

### Step 1.1 — Install cargo-mutants

```bash
cargo install cargo-mutants
```

Verify the installed version:

```bash
cargo mutants --version
```

As of 2026, the stable release is `cargo-mutants 24.x` or later. The `--in-diff` flag and `--timeout` flag are available in all versions ≥ 23.x.

### Step 1.2 — Verify feature flag compatibility

Run a dry-list to confirm cargo-mutants can parse the workspace with the required features:

```bash
cargo mutants --list \
  --features embedded_init,snapshot,uffd,blk,vhost-user \
  -e 'src/rutabaga_gfx' \
  -e 'src/hvf' \
  -e 'src/devices/src/virtio/gpu' \
  -e 'src/devices/src/virtio/snd' \
  -e 'src/devices/src/virtio/input'
```

This lists mutants without running tests — fast (~10 seconds). Confirm output is non-empty and no compilation errors appear. If the list looks wrong (too few mutants, or unexpected errors), check that `--features` matches the feature set actually present in `Cargo.toml`.

### Step 1.3 — Add `mutants.out/` to `.gitignore`

cargo-mutants writes its output directory to `mutants.out/` by default. Add it to the project root `.gitignore`:

```
# cargo-mutants output
mutants.out/
mutants.out.old/
```

Verify the current `.gitignore` at the repo root and append these lines if not already present. The `mutants.out.old/` entry covers the backup directory cargo-mutants creates when it renames a previous run.

---

## Task 2: Run the baseline mutation report

### Step 2.1 — Preview mutant list (fast, no tests run)

Before the full run, preview which files and functions will be mutated:

```bash
cargo mutants --list \
  --features embedded_init,snapshot,uffd,blk,vhost-user \
  -e 'src/rutabaga_gfx' \
  -e 'src/hvf' \
  -e 'src/devices/src/virtio/gpu' \
  -e 'src/devices/src/virtio/snd' \
  -e 'src/devices/src/virtio/input' \
  --json > /tmp/mutants-list.json
```

Inspect the JSON to count mutants per file:

```bash
jq 'group_by(.file) | map({file: .[0].file, count: length}) | sort_by(.count) | reverse' \
  /tmp/mutants-list.json
```

This identifies the hottest files. If any unexpected files appear (e.g., test files themselves), add them to the exclusion list.

### Step 2.2 — Run the full baseline (takes hours)

The full run should be done once to establish the baseline. Use `--timeout 60` to prevent hung test suites from blocking the run:

```bash
cargo mutants \
  --features embedded_init,snapshot,uffd,blk,vhost-user \
  -e 'src/rutabaga_gfx' \
  -e 'src/hvf' \
  -e 'src/devices/src/virtio/gpu' \
  -e 'src/devices/src/virtio/snd' \
  -e 'src/devices/src/virtio/input' \
  --timeout 60 \
  --jobs 4
```

The `--jobs 4` flag runs 4 mutants in parallel. Adjust based on available CPU cores. The `--timeout 60` flag marks any mutant whose test suite takes more than 60 seconds as `timeout`.

Output is written to `mutants.out/`:
- `mutants.out/outcomes.json` — machine-readable results
- `mutants.out/caught.txt` — list of caught mutants
- `mutants.out/missed.txt` — list of missed mutants (the important one)
- `mutants.out/timeout.txt` — list of timed-out mutants
- `mutants.out/unviable.txt` — list of mutants that didn't compile

### Step 2.3 — Extract summary statistics

After the run completes:

```bash
jq '{
  total: length,
  caught: [.[] | select(.outcome == "caught")] | length,
  missed: [.[] | select(.outcome == "missed")] | length,
  unviable: [.[] | select(.outcome == "unviable")] | length,
  timeout: [.[] | select(.outcome == "timeout")] | length
}' mutants.out/outcomes.json
```

Calculate the score:

```
mutation_score = caught / (caught + missed) * 100
```

Note: `unviable` and `timeout` are excluded from the denominator by convention.

### Step 2.4 — Inspect missed mutants

```bash
jq '.[] | select(.outcome == "missed") | {file: .file, line: .line, function: .function, mutation: .mutation}' \
  mutants.out/outcomes.json | head -100
```

Group missed mutants by file to identify coverage hotspots:

```bash
jq '[.[] | select(.outcome == "missed")] | group_by(.file) | map({file: .[0].file, count: length}) | sort_by(.count) | reverse' \
  mutants.out/outcomes.json
```

---

## Task 3: Add `#[mutants::skip]` annotations

`#[mutants::skip]` tells cargo-mutants to skip generating mutants for a specific function. Use it sparingly — only on functions that are:
1. Pure FFI wrappers with no logic beyond a single syscall
2. Genuinely unreachable code paths (verified by coverage data from Phase 7)
3. Intentionally untested stubs (marked as such in code comments)

Do NOT use `#[mutants::skip]` to silence missed mutants in functions that *should* be tested but currently are not.

### Step 3.1 — Add `mutants` dev-dependency where needed

cargo-mutants provides the `#[mutants::skip]` attribute via the `mutants` crate. Add it as a dev-dependency to any crate where annotations are needed:

In `src/vmm/Cargo.toml`:
```toml
[dev-dependencies]
# ... existing entries ...
mutants = "0.0.3"
```

In `src/devices/Cargo.toml`:
```toml
[dev-dependencies]
# ... existing entries ...
mutants = "0.0.3"
```

In `src/libkrun/Cargo.toml`:
```toml
[dev-dependencies]
# ... existing entries ...
mutants = "0.0.3"
```

Note: `mutants = "0.0.3"` is the canonical crate providing the `#[mutants::skip]` proc-macro attribute. It is a no-op in normal builds; cargo-mutants recognizes it only during mutation runs.

### Step 3.2 — Annotate FFI ioctl wrappers

Functions that are thin wrappers around KVM ioctls cannot be meaningfully unit-tested without a real `/dev/kvm`. Annotate these:

```rust
/// Direct KVM ioctl wrapper — no testable logic beyond the syscall.
#[cfg_attr(test, mutants::skip)]
fn kvm_set_user_memory_region(
    vm_fd: &VmFd,
    slot: u32,
    region: &kvm_userspace_memory_region,
) -> Result<(), kvm_ioctls::Error> {
    vm_fd.set_user_memory_region(*region)
}
```

The `#[cfg_attr(test, mutants::skip)]` form is preferred over `#[mutants::skip]` directly because it keeps the attribute conditional on test builds.

**Candidate functions to annotate** (verify each against missed mutants list from Task 2):
- KVM ioctl wrappers in `src/vmm/src/builder.rs`
- HVF wrappers in `src/vmm/src/hvf/` (already excluded from mutants run, but annotate for clarity)
- Direct `libc::` syscall call sites that have no surrounding logic

### Step 3.3 — Annotate `fmt::Display` / `fmt::Debug` impls

Error Display implementations that only format strings are expected to have surviving mutants. Annotate the entire `fmt` method:

```rust
impl fmt::Display for StartError {
    #[mutants::skip]  // String formatting; tests assert on error type, not message text
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::ZeroVcpus => write!(f, "vCPU count must be at least 1"),
            StartError::TagTooLong(n) => write!(f, "virtiofs tag too long: {} bytes (max 36)", n),
            // ...
        }
    }
}
```

Apply this annotation to `fmt::Display` impls for all error types where tests do not assert on formatted output.

### Step 3.4 — Annotate logging-only functions

If any function's entire body is `log::debug!(...)` or similar, annotate it:

```rust
#[mutants::skip]  // Logging only; no observable behavior in tests
fn log_vm_config(config: &VmConfig) {
    log::debug!("VM config: vcpus={}, ram_mib={}", config.vcpu_count, config.ram_mib);
}
```

### Step 3.5 — Re-run `--list` to verify skip annotations take effect

After adding annotations, re-run the list command and confirm that annotated functions no longer appear:

```bash
cargo mutants --list \
  --features embedded_init,snapshot,uffd,blk,vhost-user \
  -e 'src/rutabaga_gfx' \
  -e 'src/hvf' \
  -e 'src/devices/src/virtio/gpu' \
  -e 'src/devices/src/virtio/snd' \
  -e 'src/devices/src/virtio/input'
```

The total mutant count should decrease by the number of skipped functions.

---

## Task 4: Add justfile targets

Add the following targets to the root `justfile`. These targets are the primary interface for running mutation testing.

```just
# Mutation testing feature set (matches full build features)
mutants_features := "embedded_init,snapshot,uffd,blk,vhost-user"

# Excluded subsystems (no tests exist for these)
mutants_excludes := "-e 'src/rutabaga_gfx' -e 'src/hvf' -e 'src/devices/src/virtio/gpu' -e 'src/devices/src/virtio/snd' -e 'src/devices/src/virtio/input'"

# Run full mutation test suite. Produces mutants.out/outcomes.json.
# timeout: seconds per mutant test run (default 3600 for full run, use 60 for quick checks)
# jobs: parallel workers (default 4)
mutants timeout="3600" jobs="4":
    cargo mutants \
      --features {{mutants_features}} \
      {{mutants_excludes}} \
      --timeout {{timeout}} \
      --jobs {{jobs}}

# Run mutation tests scoped to files changed vs origin/main.
# Much faster than full run; suitable for CI on PRs.
mutants-diff timeout="60" jobs="4":
    cargo mutants \
      --features {{mutants_features}} \
      {{mutants_excludes}} \
      --in-diff origin/main..HEAD \
      --timeout {{timeout}} \
      --jobs {{jobs}}

# Preview mutants that will be generated (no tests run). Fast (~10s).
mutants-list:
    cargo mutants --list \
      --features {{mutants_features}} \
      {{mutants_excludes}} \
      --json

# Print summary of last mutation run from mutants.out/outcomes.json.
mutants-summary:
    jq '{total: length, caught: [.[] | select(.outcome == "caught")] | length, missed: [.[] | select(.outcome == "missed")] | length, unviable: [.[] | select(.outcome == "unviable")] | length, timeout: [.[] | select(.outcome == "timeout")] | length}' mutants.out/outcomes.json
```

Notes on the justfile design:
- `just mutants` uses `timeout=3600` (1 hour per mutant) as the default. This is intentionally generous for the baseline run — most tests finish in seconds, so the timeout only triggers on genuinely hung mutants.
- `just mutants timeout=60` is the recommended invocation for quick checks or CI pre-flights.
- `just mutants-diff` is the recommended CI target: scopes to changed files only, with a 60-second per-mutant timeout. This keeps CI fast (minutes, not hours).
- The `{{mutants_excludes}}` variable uses shell word splitting; verify that `just` passes these correctly. If `just` interpolates the string as a single argument, split the flags into separate lines in the recipe instead.

### Step 4.1 — Verify justfile interpolation of exclusion flags

Test that the exclusion flags are passed correctly by running `mutants-list` after adding the target:

```bash
just mutants-list | wc -l
```

If the count is unexpectedly high (includes GPU/sound/input files), the `-e` flags are not being parsed correctly. In that case, inline the flags in the recipe rather than using a variable:

```just
mutants timeout="3600" jobs="4":
    cargo mutants \
      --features embedded_init,snapshot,uffd,blk,vhost-user \
      -e 'src/rutabaga_gfx' \
      -e 'src/hvf' \
      -e 'src/devices/src/virtio/gpu' \
      -e 'src/devices/src/virtio/snd' \
      -e 'src/devices/src/virtio/input' \
      --timeout {{timeout}} \
      --jobs {{jobs}}
```

---

## Task 5: Document baseline score in `docs/mutation-baseline.md`

After the full baseline run completes (Task 2), create the baseline documentation file.

Create `docs/mutation-baseline.md` with the following structure:

```markdown
# Mutation Testing Baseline

**Date:** 2026-03-03
**cargo-mutants version:** (output of `cargo mutants --version`)
**Features:** `embedded_init,snapshot,uffd,blk,vhost-user`
**Excluded subsystems:** rutabaga_gfx, hvf, virtio/gpu, virtio/snd, virtio/input

## Summary

| Metric | Count |
|--------|-------|
| Total mutants | (fill in) |
| Caught | (fill in) |
| Missed | (fill in) |
| Unviable | (fill in) |
| Timeout | (fill in) |
| **Mutation score** | **(fill in)%** |

Mutation score = caught / (caught + missed) * 100.

## Missed Mutant Triage

### Accepted gaps (annotated with `#[mutants::skip]`)

| File | Function | Reason |
|------|----------|--------|
| `src/vmm/src/builder.rs` | `kvm_set_user_memory_region` | Direct ioctl wrapper, no testable logic |
| `src/libkrun/src/lib.rs` | `StartError::fmt` | Display impl; tests assert on error type, not message text |
| (add more as found) | | |

### Accepted gaps (no annotation — low priority)

| File | Function | Mutation | Reason |
|------|----------|----------|--------|
| (fill in from missed.txt) | | | |

### Gaps to address in follow-up

| File | Function | Mutation | Proposed fix |
|------|----------|----------|--------------|
| (fill in from missed.txt) | | | |

## How to reproduce

```bash
just mutants timeout=3600 jobs=4
just mutants-summary
```

## CI usage

For PR checks, run only on changed files:

```bash
just mutants-diff
```

This scopes mutation testing to files changed vs `origin/main` with a 60-second per-mutant timeout.
```

Fill in the actual numbers after running Task 2. The triage tables should be populated from inspection of `mutants.out/missed.txt` after the baseline run.

---

## Verification

After completing all tasks, verify the following:

### 1. justfile targets exist and are syntactically valid

```bash
just --list | grep mutants
```

Expected output includes: `mutants`, `mutants-diff`, `mutants-list`, `mutants-summary`.

### 2. Preview run completes without errors

```bash
just mutants-list | head -20
```

Should print a JSON array of mutant descriptors. No compilation errors.

### 3. `.gitignore` excludes mutants output

```bash
git check-ignore -v mutants.out
```

Should print a line indicating `mutants.out` is ignored. If not, the `.gitignore` entry is missing or in the wrong file.

### 4. `#[mutants::skip]` annotations compile

```bash
cargo check --features embedded_init,snapshot,uffd,blk,vhost-user
```

Compilation must succeed. The `mutants` dev-dependency crate provides the attribute as a no-op in normal builds.

### 5. `docs/mutation-baseline.md` exists and is populated

```bash
ls -la docs/mutation-baseline.md
```

File must exist and contain the filled-in baseline score table and triage sections (not the template placeholders).

### 6. Full mutants run produces outcomes.json

```bash
just mutants timeout=60 jobs=2
ls -la mutants.out/outcomes.json
just mutants-summary
```

Using `timeout=60` for the verification run keeps it fast. The summary should show non-zero counts for `caught` and `unviable` at minimum.

---

## Notes

### Full run timing expectations

On a modern workstation (8-core), expect:
- `--list` preview: ~10 seconds
- Full run with `--jobs 4`: 2–6 hours depending on test suite speed
- `mutants-diff` on a small PR (5–10 changed files): 5–30 minutes

The baseline run should be done once manually. After that, `just mutants-diff` in CI is sufficient for ongoing quality checking.

### Parallelism and resource limits

`cargo mutants --jobs N` spawns N parallel test suite invocations. Each invocation compiles and runs the full test suite for one mutant. Memory usage scales with N. On a machine with 16 GB RAM:
- `--jobs 2` is safe
- `--jobs 4` is typical
- `--jobs 8` may cause OOM during compilation

### Relationship to coverage (Phase 7)

Phase 7 establishes line/branch coverage. Mutation testing is complementary: coverage tells you which lines were *executed*, mutation testing tells you whether executing them *detected the change*. A function can have 100% line coverage and still have surviving mutants if tests only assert on unrelated outputs.

The triage in `docs/mutation-baseline.md` should cross-reference Phase 7 coverage data: if a function has low coverage AND surviving mutants, it is a real gap. If it has high coverage but surviving mutants (e.g., a Display impl), it is an accepted gap.

### Keeping `#[mutants::skip]` minimal

The policy is: prefer adding tests over adding skip annotations. Skip annotations are acceptable only when:
- The code genuinely cannot be tested without hardware (KVM, HVF)
- The mutant would require asserting on non-observable behavior (log output, debug formatting)
- The code is in a `#[cfg]` block that is unreachable on the CI platform

Before adding a `#[mutants::skip]` annotation, document the reason in a code comment immediately above the attribute.

### cargo-mutants and integration tests

cargo-mutants runs the test suite as-is, including integration tests if they are in the workspace. Integration tests (in `tests/`) that require a running VM may be slow or unreliable as mutant detectors. If integration tests dominate the run time, add `--test-workspace=false` or scope to unit tests with `--test unit_tests` to run only `#[cfg(test)]` modules. Check the cargo-mutants documentation for the exact flag name in the installed version.
