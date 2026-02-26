#!/usr/bin/env bash
#
# Runs the integration test suite repeatedly under CPU, I/O, and memory
# stress to shake out race conditions and flaky failures.
#
# Requirements: stress-ng
#
# Usage:
#   tests/stress-test.sh [ITERATIONS]   (default: 15)
#
# Logs for every run are saved to a timestamped directory under /tmp.
# Failures are summarised on stdout and the full log path is printed.

set -uo pipefail

# Run from repo root (Makefile lives there)
cd "$(dirname "$0")/.." || exit 1

ITERATIONS="${1:-15}"
LOG_DIR="/tmp/libkrun-stress-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$LOG_DIR"
echo "Logs: $LOG_DIR"
echo "Iterations: $ITERATIONS"

if ! command -v stress-ng &>/dev/null; then
    echo "ERROR: stress-ng not found. Install it first." >&2
    exit 1
fi

# Start stress in background
stress-ng --cpu 64 --io 16 --vm 4 --vm-bytes 256M --timeout $((ITERATIONS * 90 + 60))s &
STRESS_PID=$!
trap 'kill "$STRESS_PID" 2>/dev/null; wait "$STRESS_PID" 2>/dev/null' EXIT
echo "stress-ng pid=$STRESS_PID"
echo

PASS_COUNT=0
FAIL_COUNT=0

for run in $(seq 1 "$ITERATIONS"); do
    printf "=== Run %2d === " "$run"
    RUN_LOG="$LOG_DIR/run-${run}.log"

    if make test FEATURE_FLAGS="--features embedded_init" >"$RUN_LOG" 2>&1; then
        tail -1 "$RUN_LOG"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        RESULT=$(grep -E '(FAIL|OK) \(' "$RUN_LOG" | tail -1)
        echo "$RESULT"
        # Show which tests failed
        grep -B3 '^FAIL$' "$RUN_LOG" | grep '^\[' | sed 's/^/  /'
        echo "  log: $RUN_LOG"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
done

echo
echo "=== Summary ==="
echo "Passed: $PASS_COUNT / $ITERATIONS"
echo "Failed: $FAIL_COUNT / $ITERATIONS"
echo "Logs:   $LOG_DIR"
