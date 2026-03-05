#!/bin/sh

# This script has to be run with the working directory being "test"
# This runs the tests on the libkrun instance found by pkg-config.
# Specify PKG_CONFIG_PATH env variable to test a non-system installation of libkurn.

set -e

# Run the unit tests first (this tests the testing framework itself not libkrun)
cargo test -p test_cases --features guest

GUEST_TARGET_ARCH="$(uname -m)-unknown-linux-musl"

if [ -z "${KRUN_NO_RUN_SH_GUEST_AGENT}" ]; then
    cargo build --target=$GUEST_TARGET_ARCH -p guest-agent
fi
cargo build -p runner
cargo build -p test-daemon
cargo build -p test-vsock-proxy

export KRUN_TEST_GUEST_AGENT_PATH="target/$GUEST_TARGET_ARCH/debug/guest-agent"

# Detect the actual build output directory.  When a .cargo/config.toml sets an
# explicit [build] target (e.g. x86_64-unknown-linux-gnu), Cargo places binaries
# under target/$TARGET/debug rather than target/debug.
_HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
if [ -f "target/$_HOST_TRIPLE/debug/test-daemon" ]; then
    _DAEMON_DIR="target/$_HOST_TRIPLE/debug"
else
    _DAEMON_DIR="target/debug"
fi
export KRUN_TEST_DAEMON_PATH="$_DAEMON_DIR/test-daemon"
export KRUN_TEST_VSOCK_PROXY_PATH="$_DAEMON_DIR/test-vsock-proxy"

if [ -f "$_DAEMON_DIR/runner" ]; then
    _RUNNER="$_DAEMON_DIR/runner"
else
    _RUNNER="target/debug/runner"
fi

# Build runner args: pass through all arguments
RUNNER_ARGS="$*"

# Add --base-dir if KRUN_TEST_BASE_DIR is set
if [ -n "${KRUN_TEST_BASE_DIR}" ]; then
	RUNNER_ARGS="${RUNNER_ARGS} --base-dir ${KRUN_TEST_BASE_DIR}"
fi

if [ -z "${KRUN_NO_UNSHARE}" ] && which unshare 2>&1 >/dev/null; then
	unshare --user --map-root-user --net -- /bin/sh -c "ifconfig lo 127.0.0.1 && exec $_RUNNER ${RUNNER_ARGS}"
else
	echo "WARNING: Running tests without a network namespace."
	echo "Tests may fail if the required network ports are already in use."
	echo
	$_RUNNER ${RUNNER_ARGS}
fi
