# Feature set used by all targets.
features := "embedded_init,snapshot,uffd,blk,vhost-user"

# Default: check
default: check

# Format check + clippy
check:
    cargo fmt --check
    cargo clippy -p libkrun --features {{features}} -- -D warnings

# Build the release library
build:
    cargo build --release -p libkrun --features {{features}}

# Unit tests for all crates
test:
    cargo test -p devices --features net,snapshot,vhost-user
    cargo test -p vmm --features uffd,snapshot

# Requires libkrunfw.so in test-prefix/lib64/ (nix shellHook creates this symlink).
# Usage: just integration [<name>]
# Run integration tests (all by default, or single by name).
integration test="all":
    mkdir -p test-prefix/lib64
    cd tests && RUST_LOG=trace LD_LIBRARY_PATH="$(realpath ../test-prefix/lib64/)" ./run.sh test --test-case "{{test}}"

# Benchmark boot-timing-e2e with release builds; prints min/max/mean/stddev/median.
# Runs one warmup iteration then N timed samples.
# Usage: just bench-boot [n]
bench-boot n="20":
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p test-prefix/lib64
    export LD_LIBRARY_PATH="$(realpath test-prefix/lib64/)"
    cd tests
    GUEST_TARGET_ARCH="$(uname -m)-unknown-linux-musl"
    HOST_TARGET_ARCH="$(uname -m)-unknown-linux-gnu"
    cargo build --release --target="$GUEST_TARGET_ARCH" -p guest-agent
    cargo build --release -p runner
    cargo build -p test-daemon
    cargo build -p test-vsock-proxy
    export KRUN_TEST_GUEST_AGENT_PATH="target/$GUEST_TARGET_ARCH/release/guest-agent"
    export KRUN_TEST_DAEMON_PATH="target/debug/test-daemon"
    export KRUN_TEST_VSOCK_PROXY_PATH="target/debug/test-vsock-proxy"
    RUNNER="target/$HOST_TARGET_ARCH/release/runner"

    run_once() {
        unshare --user --map-root-user --net -- /bin/sh -c \
            "ifconfig lo 127.0.0.1 && exec $RUNNER test --test-case boot-timing-e2e" \
            2>&1 | sed -n 's/.*boot_timing_e2e: \([0-9][0-9]*\)ms.*/\1/p'
    }

    printf 'Warming up...\n'
    warmup=$(run_once)
    printf '  warmup: %sms\n' "$warmup"

    printf 'Collecting %d samples...\n' "{{n}}"
    declare -a samples
    for i in $(seq 1 {{n}}); do
        ms=$(run_once)
        if [ -z "$ms" ]; then
            printf '  run %2d: FAILED (no timing output)\n' "$i"
            exit 1
        fi
        samples+=("$ms")
        printf '  run %2d: %sms\n' "$i" "$ms"
    done

    printf '\nResults (%d samples):\n' {{n}}
    sorted=($(printf '%s\n' "${samples[@]}" | sort -n))
    n={{n}}
    if (( n % 2 == 1 )); then
        median=${sorted[$((n / 2))]}
    else
        median=$(( (${sorted[$((n / 2 - 1))]} + ${sorted[$((n / 2))]}) / 2 ))
    fi
    printf '%s\n' "${samples[@]}" | gawk -v med="$median" '
        { a[NR]=$1; sum+=$1; if(NR==1||$1<min)min=$1; if(NR==1||$1>max)max=$1 }
        END {
            n=NR; mean=sum/n
            for(i=1;i<=n;i++) v+=(a[i]-mean)^2
            sd=sqrt(v/n)
            printf "  n=%d  min=%dms  median=%dms  mean=%.1fms  max=%dms  stddev=%.1fms\n",
                   n, min, med, mean, max, sd
        }
    '

# Full fast suite: check + test + miri + proptest + loom + shuttle
all: check test miri proptest loom shuttle

# Full safety suite: check + fuzz-all + asan + shuttle + kani
safety: check fuzz-all asan shuttle kani

# Miri: run pure-logic unit tests under Miri (requires nightly)
miri:
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p arch -- gdt --skip proptest_tests
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p vmm --features snapshot -- dirty_bitmap --skip proptest_tests
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p vmm --features snapshot -- snapshot::tests::test_header --skip proptest_tests
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p vmm --features uffd,snapshot -- uffd::page_tracker --skip proptest_tests
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p devices --features net,blk -- balloon::reclaimed_bitmap --skip proptest_tests
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p devices --features net,blk -- virtio::block::request --skip proptest_tests

# proptest: property-based tests for bitmap invariants, GDT, address translation, round-trips
proptest:
    cargo test -p vmm --features snapshot -- proptest_tests
    cargo test -p vmm --features uffd,snapshot -- uffd::page_tracker::tests::proptest_tests
    cargo test -p devices --features net -- virtio::balloon::reclaimed_bitmap::tests::proptest_tests
    cargo test -p arch -- x86_64::gdt::tests::proptest_tests
    cargo test -p libkrun --features {{features}} -- tests::proptest_tests

# proptest-long: extended runs (10x cases)
proptest-long:
    PROPTEST_CASES=10000 cargo test -p vmm --features snapshot -- proptest_tests
    PROPTEST_CASES=10000 cargo test -p vmm --features uffd,snapshot -- uffd::page_tracker::tests::proptest_tests
    PROPTEST_CASES=10000 cargo test -p devices --features net -- virtio::balloon::reclaimed_bitmap::tests::proptest_tests

# Requires --release for performance (loom is computationally intensive).
# Exhaustive concurrency testing on all bitmap/tracker types.
loom:
    RUSTFLAGS="--cfg loom" cargo test --release -p vmm -- dirty_bitmap::tests::loom_tests
    RUSTFLAGS="--cfg loom" cargo test --release -p vmm --features uffd -- uffd::page_tracker::tests::loom_tests
    RUSTFLAGS="--cfg loom" cargo test --release -p devices --features net -- virtio::balloon::reclaimed_bitmap::tests::loom_tests

# Usage: just fuzz <target> [duration]
#   just fuzz fuzz_snapshot_deser
#   just fuzz fuzz_fuse_parsing 120
# Run a single fuzz target for the given duration (default 60s).
fuzz target duration="60":
    cargo +nightly fuzz run --fuzz-dir fuzz {{target}} -- -max_total_time={{duration}}

# Usage: just fuzz-all [duration]
#   just fuzz-all 300
# Run all fuzz targets sequentially for the given duration (default 60s each).
fuzz-all duration="60":
    for target in $(just fuzz-list); do \
        echo "--- Fuzzing $target for {{duration}}s ---"; \
        just fuzz $target {{duration}}; \
    done

# List all available fuzz targets.
fuzz-list:
    @cargo +nightly fuzz list --fuzz-dir fuzz 2>/dev/null \
        || grep '^name = ' fuzz/Cargo.toml | grep -v 'libkrun-fuzz' | sed 's/name = "\(.*\)"/\1/'

# Usage: just fuzz-corpus <target>
#   just fuzz-corpus fuzz_snapshot_deser
# Show corpus statistics for a fuzz target.
fuzz-corpus target:
    @if [ -d "fuzz/corpus/{{target}}" ]; then \
        echo "Corpus for {{target}}:"; \
        ls -lh fuzz/corpus/{{target}}/; \
        echo "Total: $(ls fuzz/corpus/{{target}}/ | wc -l) files"; \
    else \
        echo "No corpus directory yet: fuzz/corpus/{{target}}/"; \
        echo "Run 'just fuzz {{target}}' to start generating one."; \
    fi

# Must use --target explicitly (ASan requires target triple even for host builds).
# Unit tests under AddressSanitizer (nightly; detects memory bugs).
asan:
    RUSTFLAGS="-Zsanitizer=address" \
    cargo +nightly test \
        --target x86_64-unknown-linux-gnu \
        -p devices --features net,snapshot,vhost-user
    RUSTFLAGS="-Zsanitizer=address" \
    cargo +nightly test \
        --target x86_64-unknown-linux-gnu \
        -p vmm --features uffd,snapshot

# integration-asan: run integration tests under AddressSanitizer.
#
# How it works:
#   1. Sets RUSTFLAGS="-Zsanitizer=address" and RUSTUP_TOOLCHAIN=nightly so that
#      all `cargo build` calls inside tests/run.sh compile with ASan.
#      RUSTUP_TOOLCHAIN is intercepted by the Nix cargoWrapper (no rustup needed).
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
# Integration tests under AddressSanitizer (host binaries only; musl guest-agent built separately).
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

# Uses shuttle crate to sample thread interleavings (not exhaustive like loom).
# Targets: block worker quiesce, balloon condvar, device state transitions.
# Randomized concurrency testing (default 1000 iterations; pass iterations=N to override).
shuttle iterations="1000":
    SHUTTLE_ITERATIONS={{iterations}} \
    cargo test -p devices --features net,blk,shuttle -- shuttle_tests

# Requires: cargo install --locked kani-verifier && cargo kani setup
# Bounded formal verification proofs (inline #[cfg(kani)] modules in source files).
kani:
    #!/usr/bin/env bash
    set -euo pipefail
    # -Z function-contracts: #[kani::ensures], #[kani::proof_for_contract]
    # -Z stubbing: #[kani::stub], #[kani::stub_verified]
    # -j: run harnesses in parallel within each crate
    pids=()
    cargo kani -j --output-format terse -p vmm --features snapshot,uffd -Z function-contracts &
    pids+=($!)
    cargo kani -j --output-format terse -p devices --features net,snapshot,vhost-user -Z function-contracts -Z stubbing &
    pids+=($!)
    cargo kani -j --output-format terse -p arch -Z function-contracts -Z stubbing &
    pids+=($!)
    cargo kani -j --output-format terse -p utils -Z function-contracts &
    pids+=($!)
    cargo kani -j --output-format terse -p kernel &
    pids+=($!)
    cargo kani -j --output-format terse -p cpuid &
    pids+=($!)
    failed=0
    for pid in "${pids[@]}"; do
        wait "$pid" || ((failed++))
    done
    if ((failed > 0)); then
        echo "$failed kani job(s) failed"
        exit 1
    fi

# Usage: just kani-proof <name>
#   just kani-proof proof_mark_dirty_no_panic
# Run a single named Kani proof across all packages.
kani-proof name:
    cargo kani -p vmm --features snapshot,uffd -Z function-contracts --harness {{name}} 2>/dev/null || \
    cargo kani -p devices --features net,snapshot,vhost-user -Z function-contracts -Z stubbing --harness {{name}} 2>/dev/null || \
    cargo kani -p arch -Z function-contracts -Z stubbing --harness {{name}} 2>/dev/null || \
    cargo kani -p utils -Z function-contracts --harness {{name}} 2>/dev/null || \
    cargo kani -p kernel --harness {{name}} 2>/dev/null || \
    cargo kani -p cpuid --harness {{name}} 2>/dev/null || \
    echo "No harness named '{{name}}' found in any package"

# Run a single Kani proof with concrete playback for debugging failures.
kani-playback name:
    cargo kani -p vmm --features snapshot,uffd -Z function-contracts --harness {{name}} --concrete-playback=print 2>/dev/null || \
    cargo kani -p devices --features net,snapshot,vhost-user -Z function-contracts -Z stubbing --harness {{name}} --concrete-playback=print 2>/dev/null || \
    cargo kani -p arch -Z function-contracts -Z stubbing --harness {{name}} --concrete-playback=print 2>/dev/null || \
    cargo kani -p utils -Z function-contracts --harness {{name}} --concrete-playback=print 2>/dev/null || \
    cargo kani -p kernel --harness {{name}} --concrete-playback=print 2>/dev/null || \
    cargo kani -p cpuid --harness {{name}} --concrete-playback=print 2>/dev/null || \
    echo "No harness named '{{name}}' found in any package"

# Excluded subsystems (no tests exist for these)
# Note: mutants_excludes relies on sh -c word splitting to expand multiple -e flags.
mutants_excludes := "-e 'src/rutabaga_gfx' -e 'src/hvf' -e 'src/devices/src/virtio/gpu' -e 'src/devices/src/virtio/snd' -e 'src/devices/src/virtio/input'"

# timeout: seconds per mutant test run (default 3600 for full run, use 60 for quick checks)
# jobs: parallel workers (default 4)
# Full mutation test suite. Produces mutants.out/outcomes.json.
mutants timeout="3600" jobs="4":
    cargo mutants \
      --features {{features}} \
      {{mutants_excludes}} \
      --timeout {{timeout}} \
      --jobs {{jobs}}

# Run mutation tests scoped to files changed vs origin/main (fast; suitable for CI on PRs).
mutants-diff timeout="60" jobs="4":
    cargo mutants \
      --features {{features}} \
      {{mutants_excludes}} \
      --in-diff origin/main..HEAD \
      --timeout {{timeout}} \
      --jobs {{jobs}}

# Preview mutants that will be generated (no tests run). Fast (~10s).
mutants-list:
    cargo mutants --list \
      --features {{features}} \
      {{mutants_excludes}} \
      --json

# Print summary of last mutation run from mutants.out/outcomes.json.
mutants-summary:
    jq '{total: length, caught: [.[] | select(.outcome == "caught")] | length, missed: [.[] | select(.outcome == "missed")] | length, unviable: [.[] | select(.outcome == "unviable")] | length, timeout: [.[] | select(.outcome == "timeout")] | length}' mutants.out/outcomes.json
