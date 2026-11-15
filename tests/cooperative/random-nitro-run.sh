#!/bin/bash
# Random bytes after resume on arm64 AWS Nitro (m8g). Run from the directory that holds:
#   Image, build-id            arm64 kernel under test (recipe output) and its build-ID file
#   random-arm64               tests/cooperative/random.c built static for aarch64 (musl)
#   busybox-arm64              a static aarch64 busybox (the shell loop of short processes before the freeze)
#   random-initramfs.py, random-run.py, mkeif.py, receiver-image.py (+ its src/receiver_image), receiver.S
#   target/release/examples/nitro-random-copy (fork example)
# usage: random-nitro-run.sh OUTDIR FREEZES_PER_CPU_COUNT [CPU_COUNTS]
# RANDCOPY_CMDLINE: extra guest kernel arguments (test modes of random.c, passed to /init as environment).
# CIDs 2900 (source) and 2901-2904 (copies); at most 4 guests alive at once (the source closes before
# copies start). The launcher takes /run/holodeck/nitro-lifecycle.lock for every start and teardown and
# stops a copy that has not powered off after 90 s.
set -eu
out=$1; freezes=$2; cpu_counts=${3:-"1 4 8"}
extra=${RANDCOPY_CMDLINE:-}
mkdir -p "$out"
counters() {
    printf '%s slots_in_use=%s pool_in_use=%s leaked=%s\n' "$1" \
        "$(cat /sys/devices/virtual/misc/nitro_enclaves/pcie_slots_in_use)" \
        "$(cat /sys/class/misc/nitro_enclaves/mem_pool/in_use)" \
        "$(cat /sys/module/nitro_enclaves/parameters/leaked_donated_bytes)" >> "$out/counters.log"
}
python3 random-initramfs.py random-arm64 random.cpio busybox-arm64
as -o receiver.o receiver.S
ld -Ttext=0x70000000 -o receiver.elf receiver.o
objcopy -O binary receiver.elf receiver.bin
python3 receiver-image.py Image receiver.bin receiver-Image build-id
counters start
for cpus in $cpu_counts; do
    python3 mkeif.py build Image random.cpio "console=ttyS0 nokaslr panic=-1 rdinit=/init cooperative_template $extra" \
        "source-$cpus.eif" --mem 1024 --cpus "$cpus" > /dev/null
    python3 mkeif.py build receiver-Image - 'console=ttyS0 nokaslr panic=-1' "receiver-$cpus.eif" \
        --mem 1024 --cpus "$cpus" > /dev/null
    for i in $(seq 0 $((freezes - 1))); do
        label="cpu$cpus-$i"
        dir="$out/$label"
        mkdir -p "$dir"
        set +e
        timeout 300 sudo target/release/examples/nitro-random-copy "source-$cpus.eif" "receiver-$cpus.eif" \
            "$dir" "$label" "$cpus" 4 2900 > "$dir/run.log" 2>&1
        r=$?
        set -e
        sudo chown -R "$(id -u):$(id -g)" "$dir"
        echo "$label exit=$r" >> "$out/runs.log"
        if [ "$r" != 0 ]; then counters "after-failed-$label"; exit "$r"; fi
        # Analyse now; templates are 1 GiB each. Keep the first three failing templates.
        set +e
        timeout 600 python3 random-run.py --nitro-results "$dir" --output "$dir/analysis" --copies 4 \
            > "$dir/analysis.log" 2>&1
        a=$?
        set -e
        grep -E '^(PASS|FAILED) ' "$dir/analysis.log" >> "$out/results.log" || echo "ERROR $label analysis=$a" >> "$out/results.log"
        if [ "$a" = 0 ] || [ "$(ls "$out"/*/*.ram 2>/dev/null | wc -l)" -ge 3 ]; then rm -f "$dir/$label.ram"; fi
    done
    counters "after-cpus-$cpus"
done
counters end
echo "RANDOM_NITRO_RUNS_DONE" >> "$out/runs.log"
