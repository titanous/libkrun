#!/bin/bash
# getrandom cost on arm64 AWS Nitro (guest: randbench.c), ordinary and template guests, per kernel.
# Run from a directory holding randbench-arm64, random-initramfs.py, mkeif.py, randbench-run.py, a receiver
# image is not needed. usage: randbench-nitro.sh OUTDIR ROUNDS NAME=IMAGE_DIR ...   (IMAGE_DIR holds Image)
# Boots one guest at a time at CID 2900 with nitro-random-copy as the source only: the guest powers off after
# the benchmark, so the launcher reports "source did not freeze" by design; the console is the result.
set -u
out=$1; rounds=$2; shift 2
mkdir -p "$out"
python3 random-initramfs.py randbench-arm64 randbench.cpio
for round in $(seq 0 $((rounds - 1))); do
    for spec in "$@"; do
        name=${spec%%=*}; dir=${spec#*=}
        for mode in ordinary template; do
            extra=""; [ $mode = template ] && extra=cooperative_template
            python3 mkeif.py build "$dir/Image" randbench.cpio "console=ttyS0 nokaslr panic=-1 rdinit=/init $extra" \
                bench.eif --mem 1024 --cpus 8 > /dev/null
            work="$out/work-$name-$mode-$round"; mkdir -p "$work"
            timeout 300 sudo target/release/examples/nitro-random-copy bench.eif bench.eif "$work" bench 8 0 2900 \
                > "$work/run.log" 2>&1
            sudo chown -R "$(id -u):$(id -g)" "$work"
            cp "$work/bench.source.log" "$out/$name-$mode-$round.log"
            rm -f "$work"/*.ram
        done
    done
done
python3 randbench-run.py --nitro-logs "$out" --output "$out/analysis"
