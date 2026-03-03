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

# Compound target: safety checks.
# Phase 1: check
# Phase 4: + fuzz-all (60s per target)
# Later phases add: asan miri kani
safety: check fuzz-all

# ── Stubs for tools added in later phases ────────────────────────────────────
# These targets are extended by later implementation phases.
# Running them before the corresponding phase is complete will exit with an error.

# Miri: run pure-logic unit tests under Miri (requires nightly)
miri:
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p arch -- gdt
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p vmm --features snapshot -- dirty_bitmap snapshot::tests::test_header
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p vmm --features uffd,snapshot -- uffd::page_tracker
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p devices --features net -- balloon::reclaimed_bitmap
    MIRIFLAGS="-Zmiri-backtrace=full" \
    cargo +nightly miri test -p devices --features blk -- virtio::block::request

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

# Loom: exhaustive concurrency testing on all bitmap/tracker types
# Requires --release for performance (loom is computationally intensive).
loom:
    RUSTFLAGS="--cfg loom" cargo test --release -p vmm -- dirty_bitmap::tests::loom_tests
    RUSTFLAGS="--cfg loom" cargo test --release -p vmm --features uffd -- uffd::page_tracker::tests::loom_tests
    RUSTFLAGS="--cfg loom" cargo test --release -p devices --features net -- virtio::balloon::reclaimed_bitmap::tests::loom_tests

# Run a single fuzz target for a given duration.
# Usage: just fuzz fuzz_snapshot_deser
#        just fuzz fuzz_fuse_parsing 120
fuzz target duration="60":
    cargo +nightly fuzz run --manifest-path fuzz/Cargo.toml {{target}} -- -max_total_time={{duration}}

# Run all fuzz targets sequentially, each for the given duration.
# Usage: just fuzz-all
#        just fuzz-all 300
fuzz-all duration="60":
    for target in $(just fuzz-list); do \
        echo "--- Fuzzing $target for {{duration}}s ---"; \
        just fuzz $target {{duration}}; \
    done

# List all available fuzz targets.
fuzz-list:
    @cargo +nightly fuzz list --manifest-path fuzz/Cargo.toml 2>/dev/null \
        || grep '^name = ' fuzz/Cargo.toml | grep -v 'libkrun-fuzz' | sed 's/name = "\(.*\)"/\1/'

# Show corpus statistics for a fuzz target.
# Usage: just fuzz-corpus fuzz_snapshot_deser
fuzz-corpus target:
    @if [ -d "fuzz/corpus/{{target}}" ]; then \
        echo "Corpus for {{target}}:"; \
        ls -lh fuzz/corpus/{{target}}/; \
        echo "Total: $(ls fuzz/corpus/{{target}}/ | wc -l) files"; \
    else \
        echo "No corpus directory yet: fuzz/corpus/{{target}}/"; \
        echo "Run 'just fuzz {{target}}' to start generating one."; \
    fi

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
