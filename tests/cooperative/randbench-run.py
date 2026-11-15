#!/usr/bin/env python3
"""Cost of the copy bookkeeping on the random read path (guest: randbench.c).

Boots each kernel twice per round, as an ordinary guest and as a template guest (cooperative_template on the
command line), and reports getrandom(16 bytes) calls per second with 1 and 8 threads: the median of five
one-second samples per boot, then the median over rounds. Run from the fork checkout on a quiet machine:
    python3 tests/cooperative/randbench-run.py --initramfs IMAGE --output DIR --rounds 3 NAME=VMLINUX ...
--nitro-logs DIR analyses consoles written on Nitro instead (NAME-MODE-ROUND.log, MODE ordinary|template).
"""
import argparse
import json
import os
from pathlib import Path
import re
import statistics
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--initramfs")
parser.add_argument("--output", required=True, type=Path)
parser.add_argument("--rounds", type=int, default=3)
parser.add_argument("--cpus", default="8")
parser.add_argument("--nitro-logs", type=Path)
parser.add_argument("kernels", nargs="*", help="NAME=VMLINUX")
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=True)
launcher = str(Path("target/release/examples/cooperative-console").resolve())
SAMPLE = re.compile(rb"RANDBENCH threads=(\d+) rep=\d+ calls_per_sec=(\d+)")


def parse(output, where):
    if b"RANDBENCH DONE" not in output:
        raise RuntimeError(f"benchmark guest did not finish: {where}")
    samples = {}
    for threads, rate in SAMPLE.findall(output):
        samples.setdefault(int(threads), []).append(int(rate))
    return {t: statistics.median(v) for t, v in samples.items()}


results = {}
if args.nitro_logs:
    for log in sorted(args.nitro_logs.glob("*-*-*.log")):
        name, mode, _ = log.stem.rsplit("-", 2)
        for t, rate in parse(log.read_bytes(), log).items():
            results.setdefault((name, mode, t), []).append(rate)
else:
    for round_ in range(args.rounds):
        for spec in args.kernels:
            name, kernel = spec.split("=", 1)
            for mode in ("ordinary", "template"):
                env = dict(os.environ, CLONE_KERNEL=kernel, CLONE_INITRAMFS=args.initramfs, CLONE_VCPUS=args.cpus,
                           CLONE_MEMORY="512", CLONE_KERNEL_ARGS="cooperative_template" if mode == "template" else "")
                result = subprocess.run([launcher, "ordinary", "-", "0"], env=env, stdout=subprocess.PIPE,
                                        stderr=subprocess.STDOUT, timeout=120)
                log = args.output / f"{name}-{mode}-{round_}.log"
                log.write_bytes(result.stdout)
                for t, rate in parse(result.stdout, log).items():
                    results.setdefault((name, mode, t), []).append(rate)
summary = {f"{n} {m} threads={t}": {"median_calls_per_sec": statistics.median(v), "per_round": v}
           for (n, m, t), v in sorted(results.items())}
(args.output / "summary.json").write_text(json.dumps(summary, indent=2))
for key, value in summary.items():
    print(f"RANDBENCH {key} median={value['median_calls_per_sec']:.0f} rounds={value['per_round']}")
