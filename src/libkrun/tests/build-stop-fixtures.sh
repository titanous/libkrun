#!/usr/bin/env bash
# Build the guest fixtures of the real-KVM stop suite (tests/stop.rs) into
# target/embed of this checkout, from the sources beside this script:
#   initramfs.cpio  embed-init.c as a static /init (declared endings, bursts)
#   flood.cpio      stop-flood.c as a static /init (console pressure, unread pipe)
#   fault reset declared-reset cleared invalid sticky-panic
#                   ring-0 kernels from stop-fixture.S (raw stop-port mechanisms)
# Usage (x86-64 only): src/libkrun/tests/build-stop-fixtures.sh
# Then run the suite with EMBED_INITRAMFS, FLOOD_INITRAMFS and STOP_FIXTURES
# pointing into target/embed (see docs/embedding.md, "Stop reasons").
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
out="$root/target/embed"
mkdir -p "$out"
# A static musl C compiler and a host assembler/linker, both from Nix.
out_path() { timeout 600 nix build --no-link --print-out-paths "$1^out"; }
musl_cc=$(out_path nixpkgs#pkgsStatic.stdenv.cc)/bin/x86_64-unknown-linux-musl-gcc
host_cc=$(out_path nixpkgs#gcc)/bin/gcc

"$musl_cc" -static -O2 -I"$here" -o "$out/init" "$here/embed-init.c"
python3 "$here/embed-initramfs.py" "$out/init" "$out/initramfs.cpio"
"$musl_cc" -static -O2 -o "$out/flood-init" "$here/stop-flood.c"
python3 "$here/embed-initramfs.py" "$out/flood-init" "$out/flood.cpio"

raw() {
    local name=$1
    shift
    # No property note: newer linkers place it right after the text, in guest memory the
    # VMM uses at boot (the original fixtures had it at 0x400190).
    "$host_cc" -nostdlib -static -no-pie -Wa,-mx86-used-note=no -Wl,--build-id=none \
        -Wl,-Ttext=0x100000 -Wl,-e,_start "$@" \
        -o "$out/$name" "$here/stop-fixture.S"
}
raw fault
raw reset -DSTOP_RESET
raw declared-reset -DSTOP_REASON=1 -DSTOP_RESET
raw cleared -DSTOP_REASON=1 -DSTOP_CLEAR
raw invalid -DSTOP_REASON=9
raw sticky-panic -DSTOP_REASON=4 -DSTOP_CLEAR
echo "stop fixtures in $out"
