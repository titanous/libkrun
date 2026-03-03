# Mutation Testing Baseline

**Date:** 2026-03-03
**cargo-mutants version:** 26.2.0
**Features:** `embedded_init,snapshot,uffd,blk,vhost-user`
**Excluded subsystems:** rutabaga_gfx, hvf, virtio/gpu, virtio/snd, virtio/input

## Summary

| Metric | Count |
|--------|-------|
| Total mutants | 10,678 |
| Caught | (to be determined after baseline run) |
| Missed | (to be determined after baseline run) |
| Unviable | (to be determined after baseline run) |
| Timeout | (to be determined after baseline run) |
| **Mutation score** | **(to be determined)%** |

Mutation score = caught / (caught + missed) * 100.

## Expected Surviving Mutant Categories

The following categories of surviving mutants are **expected and acceptable** — they do not represent test gaps that need to be closed:

### 1. Display/Debug error message text
Tests assert on error *types*, not formatted strings. Mutants that change string literals in error messages will survive.

**Example:** Changing `"invalid magic"` to `""` or `"xnvalid magic"` will not be caught.

### 2. Unreachable platform branches
Code gated behind `#[cfg(target_os = "macos")]` is unreachable on Linux CI. These appear as `unviable` (won't compile) rather than `missed`.

### 3. Logging and tracing calls
Tests do not assert on log output. Mutants that delete or alter `log::debug!`, `log::trace!`, `log::info!` arguments will survive.

### 4. Metrics and statistics counters
Incrementing a statistics counter by 1 vs 2 may produce surviving mutants if no test asserts the exact value in a boundary case.

### 5. FFI boundary functions
Functions that directly call into KVM ioctls or libc cannot be unit-tested without real hardware. These are expected to have surviving mutants.

## Missed Mutant Triage

*Note: This section will be populated after running the baseline. Run `just mutants timeout=3600 jobs=4` to generate the baseline.*

### Accepted gaps (annotated with `#[mutants::skip]`)

To be populated after analysis of missed mutants. Functions should only be annotated if they are:
- Pure FFI wrappers with no logic beyond a single syscall
- Genuinely unreachable (verified by coverage data)
- Intentionally untested stubs (marked as such in code comments)

| File | Function | Reason |
|------|----------|--------|
| (to be filled) | | |

### Accepted gaps (no annotation — low priority)

| File | Function | Mutation | Reason |
|------|----------|----------|--------|
| (to be filled) | | | |

### Gaps to address in follow-up

| File | Function | Mutation | Proposed fix |
|------|----------|----------|--------------|
| (to be filled) | | | |

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

## Next Steps

1. Run the full baseline mutation test: `just mutants timeout=3600 jobs=4`
2. Extract summary stats: `just mutants-summary`
3. Inspect missed mutants: `jq '.[] | select(.outcome == "missed")' mutants.out/outcomes.json | head -50`
4. Group by file: `jq '[.[] | select(.outcome == "missed")] | group_by(.file) | map({file: .[0].file, count: length})' mutants.out/outcomes.json`
5. Populate the triage tables above with findings
6. Add `#[mutants::skip]` annotations only where appropriate
7. Re-run to verify: `cargo mutants --list --features embedded_init,snapshot,uffd,blk,vhost-user ...` to confirm skipped functions no longer generate mutants
