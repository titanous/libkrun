# Feature set used by all targets.
# AC2.11: All targets use this same variable.
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
    cargo test -p devices --features net,snapshot
    cargo test -p vmm --features snapshot
    cargo test -p devices --features net,vhost-user

# Run integration tests.
# Requires libkrunfw.so in test-prefix/lib64/ (nix shellHook creates this symlink).
# Usage:
#   just integration         — run all tests
#   just integration <name>  — run single test by name
integration test="all":
    mkdir -p test-prefix/lib64
    cd tests && RUST_LOG=trace LD_LIBRARY_PATH="$(realpath ../test-prefix/lib64/)" ./run.sh test --test-case "{{test}}"

# Compound target: all fast tests (extended in later phases)
# Phase 1: check + unit tests
# Phase 2: add miri proptest loom
# Later phases add: shuttle
all: check test miri proptest loom

# Compound target: safety checks (extended in later phases)
# Phase 1: check only
# Later phases add: asan miri fuzz-all kani
safety: check

# ── Stubs for tools added in later phases ────────────────────────────────────
# These targets are extended by later implementation phases.
# Running them before the corresponding phase is complete will exit with an error.

# Miri: run pure-logic unit tests under Miri (requires nightly)
miri:
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p vmm -- dirty_bitmap
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p devices --features net -- balloon::reclaimed_bitmap

# proptest: property-based tests for bitmap invariants and address translation (Phase 3 adds tests)
proptest:
    cargo test -p vmm --features snapshot -- proptest
    cargo test -p devices --features net -- proptest

# proptest-long: extended proptest runs (10x cases)
proptest-long:
    PROPTEST_CASES=10000 cargo test -p vmm --features snapshot -- proptest
    PROPTEST_CASES=10000 cargo test -p devices --features net -- proptest

# Loom: exhaustive concurrency testing on bitmap types (Phase 3 adds tests)
loom:
    RUSTFLAGS="--cfg loom" cargo test --release -p vmm -- dirty_bitmap
    RUSTFLAGS="--cfg loom" cargo test --release -p devices --features net -- balloon::reclaimed_bitmap

fuzz target:
    @echo "fuzz: set up in Phase 4 (Fuzzing)"
    @exit 1

fuzz-all duration="60":
    @echo "fuzz-all: set up in Phase 4 (Fuzzing)"
    @exit 1

fuzz-list:
    @echo "fuzz-list: set up in Phase 4 (Fuzzing)"
    @exit 1

fuzz-corpus target:
    @echo "fuzz-corpus: set up in Phase 4 (Fuzzing)"
    @exit 1

asan:
    @echo "asan: set up in Phase 5 (ASan + Shuttle)"
    @exit 1

integration-asan:
    @echo "integration-asan: set up in Phase 5 (ASan + Shuttle)"
    @exit 1

shuttle iterations="1000":
    @echo "shuttle: set up in Phase 5 (ASan + Shuttle)"
    @exit 1

kani:
    @echo "kani: set up in Phase 6 (Kani Proofs)"
    @exit 1

kani-proof name:
    @echo "kani-proof: set up in Phase 6 (Kani Proofs)"
    @exit 1

mutants:
    @echo "mutants: set up in Phase 8 (Mutation Testing)"
    @exit 1

mutants-diff:
    @echo "mutants-diff: set up in Phase 8 (Mutation Testing)"
    @exit 1
