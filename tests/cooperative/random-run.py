#!/usr/bin/env python3
"""Random bytes across copies for running threads (guest: random.c).

For every source freeze, read each worker's control page and buffer from the template's guest RAM, start
several copies of the template, read each copy's record rings from its disk, and require for every thread:
  - the first value returned to user space after resume differs between all copies;
  - no value appears twice among the first three values after resume of all copies;
  - no copy returns after resume bytes that the kernel had generated before the freeze.
"Returned after resume" is decidable: a system-call worker stores the result in the instruction right after
the call, so a template showing "in call, not stored" means the thread had not executed a user instruction
after the call when it froze. A vDSO worker counts as returned before the freeze when the template shows its
vDSO state no longer in use (the vDSO's own final generation check passed) and the whole buffer written.
The guest's pages are found in template RAM by marker (per-boot nonce + tag), see random.c.
"""
import argparse
import collections
import concurrent.futures
import json
import mmap
import os
from pathlib import Path
import re
import struct
import subprocess

PENDING = -9999
RING_MAGIC = 0x31474E49524E4152
RING_OFFSET = 1 << 20
RING_STRIDE = 5 << 20
LEAD = 64
THREADS = 16
RECORD = struct.Struct("<QqQ16s16sQ")
KINDS = ("getrandom", "vdso", "urandom", "random")
SIZES = (8, 32, 4096, 1 << 20)
M64 = (1 << 64) - 1

parser = argparse.ArgumentParser()
parser.add_argument("--kernel")
parser.add_argument("--initramfs")
parser.add_argument("--output", required=True, type=Path)
parser.add_argument("--nitro-results", type=Path,
                    help="analyse arm64 Nitro runs instead of starting KVM guests: DIR holds, per freeze LABEL, "
                         "LABEL.ram (exported template), LABEL.source.log and LABEL-copyN.log (copy consoles)")
parser.add_argument("--freezes", type=int, default=70, help="source freezes per CPU count")
parser.add_argument("--copies", type=int, default=4)
parser.add_argument("--cpus", default="1,4,8")
parser.add_argument("--memory", default="512")
parser.add_argument("--keep-going", action="store_true", help="measure every freeze even after a failure")
parser.add_argument("--no-shell", action="store_true",
                    help="the image has no /bin/busybox: do not require the shell loop of short processes")
parser.add_argument("--no-churn", action="store_true",
                    help="guest test mode randcopy_churn=0: no exiting-task phase (isolates another property)")
parser.add_argument("--protect", action="store_true",
                    help="guest test mode randcopy_protect=1 (one CPU): each copy makes the buffers of system-call "
                         "workers caught in their call read-only before they resume; their refresh must be skipped "
                         "with a warning and every copy must finish")
parser.add_argument("--samesite", action="store_true",
                    help="guest test mode randcopy_samesite=1: reads of /dev/zero from the instruction of an earlier "
                         "/dev/urandom read, with identical registers, must never be rewritten in a copy")
parser.add_argument("--bomb", type=int, default=0,
                    help="guest test mode randcopy_bomb=N: three waves of N short getrandom processes and N/4 threads "
                         "before the freeze; waves 2 and 3 together must keep less than 1 MiB of kernel memory")
parser.add_argument("--omit-seed", action="store_true",
                    help="copies start with KRUN_TEST_OMIT_COPY_SEED=1 (launcher built with the copy-fault-injection "
                         "feature: the host gives no fresh seed); every copy must stop with the guest's refusal "
                         "before running user space")
args = parser.parse_args()
if args.protect and args.cpus != "1" and not args.nitro_results:
    parser.error("--protect needs --cpus 1")
GUEST_ARGS = " ".join(word for word, on in (
    ("randcopy_churn=0", args.no_churn), ("randcopy_protect=1", args.protect),
    ("randcopy_samesite=1", args.samesite), (f"randcopy_bomb={args.bomb}", args.bomb > 0)) if on)
SEED_REFUSAL = b"copy resumed without a fresh random seed from the host"
mode_stats = collections.Counter()
# Tasks that read random bytes and exited before the freeze (random.c churn phase): the source must have
# run them, and every copy must thaw, reap them and report so.
CHURN_BEFORE = re.compile(rb"RANDCOPY CHURN before-freeze shell=(\d+) processes=(\d+) threads=(\d+)")
CHURN_AFTER = b"RANDCOPY CHURN after-resume"


def check_churn_source(label, output):
    if args.bomb:
        b = re.search(rb"RANDCOPY BOMB processes=(\d+) threads=(\d+) mem_available_kib=(-?\d+)\.\.(-?\d+)\.\.(-?\d+)"
                      rb"\.\.(-?\d+) kernel_kib=(-?\d+)\.\.(-?\d+)\.\.(-?\d+)\.\.(-?\d+)", output)
        if b is None or int(b.group(1)) != 3 * args.bomb:
            raise RuntimeError(f"{label}: fork bomb did not run")
        mem = [int(b.group(i)) for i in range(3, 7)]
        kernel = [int(b.group(i)) for i in range(7, 11)]
        if min(mem) < 0 or min(kernel) < 0:
            raise RuntimeError(f"{label}: fork bomb could not read /proc/meminfo: {b.group(0).decode()}")
        for wave in (1, 2, 3):
            key = f"bomb wave {wave} max drop kib"
            mode_stats[key] = max(mode_stats[key], mem[wave - 1] - mem[wave])
            key = f"bomb wave {wave} max kernel growth kib"
            mode_stats[key] = max(mode_stats[key], kernel[wave] - kernel[wave - 1])
        # Bounded: the second and third equal waves (2 x (N + N/4) exited tasks) must not keep kernel memory:
        # unreclaimable slab + kernel stacks + page tables must grow < 1 MiB over both (~21 bytes per task; on
        # the base kernel that sum moves < 100 KiB per wave on x86 and < 250 KiB on arm64 Nitro). MemAvailable
        # is reported, not checked: even settled, on the base kernel, which keeps nothing per task, it dropped
        # 3608 KiB over waves 2 and 3 on arm64 Nitro and 8904 KiB in one wave on x86 (observed).
        if kernel[3] - kernel[1] >= 1024:
            raise RuntimeError(f"{label}: fork-bomb waves 2 and 3 kept memory: {b.group(0).decode()}")
    if args.no_churn:
        return
    m = CHURN_BEFORE.search(output)
    if m is None:
        raise RuntimeError(f"{label}: source did not run the exiting-task phase")
    if int(m.group(1)) != (0 if args.no_shell else 300) or int(m.group(2)) < 158 or int(m.group(3)) < 100:
        raise RuntimeError(f"{label}: exiting-task phase incomplete: {m.group(0).decode()}")


def check_churn_copy(name, output):
    if args.no_churn:
        if b"RANDCOPY CHURN off" not in output:
            raise RuntimeError(f"copy failed: {name}: did not finish (console kept)")
    elif CHURN_AFTER not in output or not re.search(rb"RANDCOPY CHURN after-resume .* ok", output):
        raise RuntimeError(f"copy failed: {name}: exiting tasks not reaped after resume (console kept)")


def check_modes_copy(name, output):
    """Per-copy checks of the test modes; returns the threads this copy protected (excluded from analysis)."""
    protected = set()
    if args.protect:
        if not re.search(rb"RANDCOPY PROTECT protected=\d+ survived", output):
            raise RuntimeError(f"copy failed: {name}: protected workers did not survive (console kept)")
        protected = {int(m) for m in re.findall(rb"RANDCOPY PROTECT thread=(\d+)", output)}
    if args.samesite:
        m = re.search(rb"RANDCOPY SAMESITE frozen_phase=(\d+) zero_reads=(\d+) failures=(\d+)", output)
        if m is None:
            raise RuntimeError(f"copy failed: {name}: same-site thread did not report (console kept)")
        mode_stats[f"samesite frozen_phase={int(m.group(1))}"] += 1
        if int(m.group(3)):
            raise RuntimeError(f"SAMESITE FAIL {name}: a zero read returned random bytes: a result was rewritten "
                               f"after the task had run ({m.group(0).decode()})")
    return protected


def check_protect_skips(label, states, outputs, protected):
    """A protected thread caught after the kernel generated its bytes must produce a skipped refresh."""
    needs = [i for i in protected if classify(KINDS[i // 4], states[i])[0]
             in ("kernel-finished-not-returned", "kernel-mid-fill")]
    mode_stats["protect threads with a pending refresh"] += len(needs)
    for c, output in enumerate(outputs):
        if needs and b"random refresh skipped: buffer not writable" not in output:
            raise RuntimeError(f"{label} copy {c}: protected threads {needs} had a pending refresh but no skip "
                               f"was reported")
if not args.nitro_results and not (args.kernel and args.initramfs):
    parser.error("--kernel and --initramfs are required unless --nitro-results is given")
args.output.mkdir(parents=True, exist_ok=False)
launcher = str(Path("target/release/examples/cooperative-console").resolve())


def pattern(thread, seq, word):
    return ((seq * 0x9E3779B97F4A7C15) ^ (word * 0xC2B2AE3D27D4EB4F) ^ (thread * 0x165667B19E3779F9)
            ^ 0x5BD1E9955BD1E995) & M64


def hash_words(data):
    h = 0xCBF29CE484222325
    for (w,) in struct.iter_unpack("<Q", data):
        h = ((h ^ w) * 0x100000001B3) & M64
    return h


def environment(kind, cpus, disk):
    env = dict(os.environ, CLONE_KERNEL=args.kernel, CLONE_INITRAMFS=args.initramfs, CLONE_VCPUS=str(cpus),
               CLONE_DISK=str(disk), CLONE_MEMORY=args.memory, CLONE_KERNEL_ARGS=GUEST_ARGS)
    if args.omit_seed and kind == "resume":
        env["KRUN_TEST_OMIT_COPY_SEED"] = "1"
    return env


def run(kind, image, cpus, disk, log):
    try:
        result = subprocess.run([launcher, kind, str(image), "0"], env=environment(kind, cpus, disk),
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=120)
    except subprocess.TimeoutExpired as expired:
        # A guest that never thaws (for example a kernel oops at the copy) gives no exit: keep its console.
        log.write_bytes((expired.stdout or b"") + b"\n[random-run: launcher killed after 120 s]\n")
        raise RuntimeError(f"{kind} timed out, console kept in {log}") from None
    log.write_bytes(result.stdout)
    return result


def find_markers(image, nonce):
    """Map (tag, thread) -> file offset of the marker, for markers written by random.c `mark`."""
    check = struct.pack("<Q", ~struct.unpack_from("<Q", nonce)[0] & M64)
    found = {}
    with image.open("rb") as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as ram:
        at = ram.find(nonce, 4096)
        while at >= 0:
            word, = struct.unpack_from("<Q", ram, at + 16)
            if ram[at + 24:at + 32] == check and chr(word >> 56) in "CHTV" and (word & 0xFF) < THREADS \
                    and word & 0x00FFFFFFFFFFFF00 == 0:
                key = (chr(word >> 56), word & 0xFF)
                if key in found:
                    raise RuntimeError(f"ambiguous marker {key} in template")
                found[key] = at
            at = ram.find(nonce, at + 1)
        states = []
        for t in range(THREADS):
            kind, size = KINDS[t // 4], SIZES[t % 4]
            for tag in "CHT" + ("V" if kind == "vdso" else ""):
                if (tag, t) not in found:
                    raise RuntimeError(f"marker {tag} of thread {t} not found in template")
            control = found[("C", t)] + 32
            state, seq, commit, k, s, recorded = struct.unpack_from("<QQqQQQ", ram, control)
            if (KINDS[k], s) != (kind, size):
                raise RuntimeError(f"control page of thread {t} disagrees with layout")
            head_at = found[("H", t)] + LEAD
            head_len = min(size, 4096 - head_at % 4096)
            head = ram[head_at:head_at + head_len]
            tail_end = found[("T", t)]
            tail_len = min(size, tail_end % 4096)
            tail = ram[tail_end - tail_len:tail_end]
            whole = head if head_len == size else None
            first_word = struct.unpack_from("<Q", head)[0]
            last_word = struct.unpack_from("<Q", tail, tail_len - 8)[0]
            first_written = first_word != pattern(t, seq, 0)
            last_written = last_word != pattern(t, seq, size // 8 - 1)
            in_use = None
            if kind == "vdso":
                in_use = ram[found[("V", t)] - 2048 + 137] != 0
            states.append({"state": state, "seq": seq, "commit": commit, "recorded": recorded,
                           "first_written": first_written, "last_written": last_written,
                           "buffer_hash": hash_words(whole) if whole is not None else None,
                           "head": head[:16], "tail": tail[-16:], "in_use": in_use})
    return states


def read_rings(disk):
    rings = []
    with disk.open("rb") as f:
        for i in range(THREADS):
            f.seek(RING_OFFSET + i * RING_STRIDE)
            magic, index, recorded, ring = struct.unpack("<4Q", f.read(32))
            if magic != RING_MAGIC or index != i:
                raise RuntimeError(f"missing ring {i} on copy disk")
            f.seek(RING_OFFSET + i * RING_STRIDE + 4096)
            records = {}
            for seq, ret, h, head, tail, _ in RECORD.iter_unpack(f.read(RECORD.size * ring)):
                if seq:
                    records[seq] = (ret, h, head, tail)
            rings.append(records)
    return rings


def same_head(record, t):
    # Records keep 16 bytes from each end of the buffer; an 8-byte buffer's template view has 8.
    return record[2][:len(t["head"])] == t["head"]


def same_tail(record, t):
    return record[3][-len(t["tail"]):] == t["tail"]


def console_rings(log):
    """Records printed by a copy without a disk: RANDCOPY REC thread seq ret hash head tail."""
    rings = [{} for _ in range(THREADS)]
    for m in re.finditer(rb"RANDCOPY REC (\d+) (\d+) (-?\d+) ([0-9a-f]{16}) ([0-9a-f]{32}) ([0-9a-f]{32})", log):
        rings[int(m.group(1))][int(m.group(2))] = (int(m.group(3)), int(m.group(4), 16),
                                                     bytes.fromhex(m.group(5).decode()),
                                                     bytes.fromhex(m.group(6).decode()))
    return rings


def classify(kind, t):
    """Return (category, first sequence number that user space receives after resume)."""
    if t["state"] == 0:
        return "outside-call", t["seq"] + 1
    if t["commit"] != PENDING:
        return "returned-before-freeze", t["seq"] + 1
    full = t["first_written"] and t["last_written"]
    if kind == "vdso" and t["in_use"] is False and full:
        return "vdso-returned-before-freeze", t["seq"] + 1
    if kind == "vdso" and full:
        # The vDSO clears in_use one store after its final generation check passes, so "in use, buffer
        # written" is either before the check (a copy retries) or in that gap (the bytes count as returned
        # before the freeze). User memory cannot tell them apart: judge from the next value on.
        return "vdso-at-final-check", t["seq"] + 1
    if full:
        return "kernel-finished-not-returned", t["seq"]
    if t["first_written"]:
        return "kernel-mid-fill", t["seq"]
    return "in-call-nothing-written", t["seq"]


stats = collections.Counter()
failures = []
examples = []


def analyse(label, states, copies, protected=frozenset()):
    bad = 0
    for i, t in enumerate(states):
        kind, size = KINDS[i // 4], SIZES[i % 4]
        name = f"{kind}-{size}"
        category, first = classify(kind, t)
        stats[(name, category)] += 1
        if i in protected:
            # --protect: this thread's buffer became read-only in the copies, so it keeps the template's
            # bytes by construction; check_protect_skips covers it.
            stats[(name, "protected")] += 1
            continue
        values, firsts = [], []
        for c, rings in enumerate(copies):
            records = rings[i]
            sequence = [records.get(s) for s in range(first, first + 3)]
            if any(r is None for r in sequence):
                raise RuntimeError(f"{label} {name} copy {c}: ring lacks records {first}..{first + 2}")
            # Harness self-check: a value stored before the freeze is exactly the template buffer.
            if category in ("returned-before-freeze", "vdso-returned-before-freeze"):
                prior = records.get(t["seq"])
                if prior is None or not same_head(prior, t) or not same_tail(prior, t) or \
                        (t["buffer_hash"] is not None and prior[1] != t["buffer_hash"]):
                    raise RuntimeError(f"{label} {name} copy {c}: template readback disagrees with stored value")
            if sequence[0][0] < size:
                stats[(name, "short-first-return")] += 1
            firsts.append(sequence[0])
            values.extend(sequence)
        problems = []
        if len({r[1] for r in firsts}) != len(firsts):
            problems.append("first value after resume repeated across copies")
        if len({r[1] for r in values}) != len(values):
            problems.append("value repeated among first three after resume")
        if t["state"] == 1 and first == t["seq"]:
            stale = [r for r in firsts if (t["first_written"] and same_head(r, t))
                     or (t["last_written"] and same_tail(r, t))
                     or (t["buffer_hash"] is not None and t["first_written"] and r[1] == t["buffer_hash"])]
            if stale:
                problems.append(f"{len(stale)} copies returned bytes generated before the freeze")
        if problems:
            bad += 1
            stats[(name, "FAIL")] += 1
            entry = {"freeze": label, "thread": name, "category": category, "problems": problems,
                     "template": {k: (v.hex() if isinstance(v, bytes) else v) for k, v in t.items()},
                     "first_values": [{"ret": r[0], "hash": f"{r[1]:016x}", "head": r[2].hex()} for r in firsts]}
            failures.append(entry)
            print("FAIL", json.dumps(entry), flush=True)
    return bad


cycle_count = 0
try:
    for template_path in sorted(args.nitro_results.glob("*.ram")) if args.nitro_results else ():
        label = template_path.stem
        source_log = (args.nitro_results / f"{label}.source.log").read_bytes()
        nonce = re.search(rb"RANDCOPY NONCE ([0-9a-f]{32})", source_log)
        if nonce is None:
            raise RuntimeError(f"no nonce in {label}.source.log")
        check_churn_source(label, source_log)
        states = find_markers(template_path, bytes.fromhex(nonce.group(1).decode()))
        logs = sorted(args.nitro_results.glob(f"{label}-copy*.log"))
        if len(logs) != args.copies:
            raise RuntimeError(f"{label}: expected {args.copies} copy consoles, found {len(logs)}")
        copies, outputs, protected = [], [], set()
        for log in logs:
            data = log.read_bytes()
            if args.omit_seed:
                if SEED_REFUSAL not in data or b"RANDCOPY DONE" in data:
                    raise RuntimeError(f"OMIT-SEED FAIL {log.name}: a copy without a host seed was not refused")
                continue
            if b"RANDCOPY DONE" not in data or b"RANDCOPY FAIL" in data:
                raise RuntimeError(f"copy failed: {log.name}")
            check_churn_copy(log.name, data)
            protected |= check_modes_copy(log.name, data)
            copies.append(console_rings(data))
            outputs.append(data)
        if args.omit_seed:
            cycle_count += 1
            print(f"PASS {label} every copy refused without a host seed", flush=True)
            continue
        check_protect_skips(label, states, outputs, protected)
        bad = analyse(label, states, copies, protected)
        cycle_count += 1
        caught = [f"{KINDS[i // 4]}-{SIZES[i % 4]}:{classify(KINDS[i // 4], s)[0]}"
                  for i, s in enumerate(states) if classify(KINDS[i // 4], s)[0] != "outside-call"]
        print(f"{'FAILED' if bad else 'PASS'} {label} threads_failed={bad} " + " ".join(caught), flush=True)
    for cpus in (int(x) for x in args.cpus.split(",")) if not args.nitro_results else ():
        for cycle in range(args.freezes):
            label = f"cpu{cpus}-{cycle}"
            prefix = args.output / label
            disk, image = prefix.with_suffix(".disk"), prefix.with_suffix(".ram")
            with disk.open("wb") as f:
                f.truncate(96 << 20)
            source = run("freeze", image, cpus, disk, prefix.with_suffix(".freeze.log"))
            nonce = re.search(rb"RANDCOPY NONCE ([0-9a-f]{32})", source.stdout)
            if source.returncode or b"result=Frozen" not in source.stdout or b"RANDCOPY FREEZE" not in source.stdout \
                    or nonce is None:
                raise RuntimeError(f"source failed: {label}")
            check_churn_source(label, source.stdout)
            states = find_markers(image, bytes.fromhex(nonce.group(1).decode()))

            def copy(c):
                child = args.output / f"{label}-copy{c}.disk"
                subprocess.run(["cp", "--reflink=auto", str(disk), str(child)], check=True)
                result = run("resume", image, cpus, child, args.output / f"{label}-copy{c}.log")
                if args.omit_seed:
                    child.unlink()
                    if SEED_REFUSAL not in result.stdout or b"RANDCOPY DONE" in result.stdout \
                            or b"RANDCOPY CHURN after-resume" in result.stdout:
                        raise RuntimeError(f"OMIT-SEED FAIL {label} copy {c}: a copy without a host seed was not "
                                           f"refused before user space ran (console kept)")
                    return None, result.stdout
                if result.returncode or b"RANDCOPY DONE" not in result.stdout or b"RANDCOPY FAIL" in result.stdout:
                    raise RuntimeError(f"copy failed: {label} copy {c} (console kept in {label}-copy{c}.log)")
                check_churn_copy(f"{label} copy {c}", result.stdout)
                rings = read_rings(child)
                child.unlink()
                return rings, result.stdout

            with concurrent.futures.ThreadPoolExecutor(max_workers=args.copies) as pool:
                results = list(pool.map(copy, range(args.copies)))
            if args.omit_seed:
                cycle_count += 1
                print(f"PASS {label} every copy refused without a host seed", flush=True)
                image.unlink()
                disk.unlink()
                continue
            copies = [rings for rings, _ in results]
            outputs = [output for _, output in results]
            protected = set()
            for c, output in enumerate(outputs):
                protected |= check_modes_copy(f"{label} copy {c}", output)
            check_protect_skips(label, states, outputs, protected)
            bad = analyse(label, states, copies, protected)
            cycle_count += 1
            caught = [f"{KINDS[i // 4]}-{SIZES[i % 4]}:{classify(KINDS[i // 4], s)[0]}"
                      for i, s in enumerate(states) if classify(KINDS[i // 4], s)[0] != "outside-call"]
            print(f"{'FAILED' if bad else 'PASS'} {label} threads_failed={bad} " + " ".join(caught), flush=True)
            if bad and len(examples) < 3:
                examples.append(label)  # keep the template and logs of the first failing freezes
            else:
                image.unlink()
            disk.unlink()
            if bad and not args.keep_going:
                raise SystemExit(1)
finally:
    summary = {"freezes": cycle_count, "copies_per_freeze": args.copies, "guest_args": GUEST_ARGS,
               "omit_seed": args.omit_seed, "modes": dict(mode_stats),
               "counts": {f"{k[0]} {k[1]}": v for k, v in sorted(stats.items())},
               "failures": len(failures), "kept_templates": examples}
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2))
    (args.output / "failures.json").write_text(json.dumps(failures, indent=2))
    print(json.dumps(summary, indent=2), flush=True)
if failures:
    print(f"RANDOM ACCEPTANCE FAIL freezes={cycle_count} failing_threads={len(failures)}", flush=True)
    raise SystemExit(1)
print(f"RANDOM ACCEPTANCE PASS freezes={cycle_count} copies={cycle_count * args.copies}", flush=True)
