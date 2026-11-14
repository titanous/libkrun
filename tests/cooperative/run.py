#!/usr/bin/env python3
"""Real-KVM cooperative clone matrix; run through just cooperative-clone."""
from console_markers import userspace_bytes
import argparse
import concurrent.futures
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import socket
import statistics
import subprocess
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument("--kernel", required=True, type=Path)
parser.add_argument("--initramfs", required=True, type=Path)
parser.add_argument("--control", required=True)
parser.add_argument("--output", required=True, type=Path)
parser.add_argument("--cycles", type=int, default=20)
parser.add_argument("--concurrent", type=int, default=16)
parser.add_argument("--passed-fd", action="store_true", help="share one sealed memfd across new VMM processes")
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=False)
launcher = Path("target/release/examples/cooperative-console").resolve()
peer_path = args.output / "echo.sock"
peer = subprocess.Popen(["python3", str(Path(__file__).with_name("echo.py")), str(peer_path)])
tcp_probe = socket.socket()
tcp_probe.bind(("127.0.0.1", 18080))
tcp_probe.listen(128)
tcp_probe.setblocking(False)
measurements = []
template_fds = {}

def template_fd(image):
    if image not in template_fds:
        fd = os.memfd_create("cooperative-test-template", os.MFD_ALLOW_SEALING)
        with image.open("rb") as source, os.fdopen(os.dup(fd), "wb") as target:
            shutil.copyfileobj(source, target)
        fcntl.fcntl(fd, fcntl.F_ADD_SEALS,
                    fcntl.F_SEAL_WRITE | fcntl.F_SEAL_GROW | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_SEAL)
        template_fds[image] = fd
    return template_fds[image]

def ready():
    deadline = time.monotonic() + 10
    while not peer_path.exists():
        if peer.poll() is not None or time.monotonic() > deadline:
            raise RuntimeError("echo peer failed readiness")
        # Event.wait is an interruptible bounded readiness retry, not guest timing.
        threading.Event().wait(.02)

def environment(cpus, disk):
    return dict(os.environ, CLONE_KERNEL=str(args.kernel), CLONE_INITRAMFS=str(args.initramfs),
                CLONE_VCPUS=str(cpus), CLONE_DISK=str(disk), CLONE_ECHO_SOCKET=str(peer_path))

def run_source(cpus, cycle):
    prefix = args.output / f"cpu{cpus}-{cycle}"
    disk = prefix.with_suffix(".ext4")
    with disk.open("wb") as f:
        f.truncate(64 * 1024 * 1024)
    subprocess.run(["mkfs.ext4", "-q", "-F", str(disk)], check=True)
    image = prefix.with_suffix(".ram")
    start = time.monotonic_ns()
    trigger = frozen = None
    source_log = prefix.with_suffix(".freeze.log")
    with source_log.open("wb") as output:
        process = subprocess.Popen([str(launcher), "freeze", str(image), args.control],
                                   env=environment(cpus, disk), stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        # Source process is independently bounded even if a line blocks.
        timer = threading.Timer(30, process.kill)
        timer.start()
        try:
            console = b""
            for line in process.stdout:
                output.write(line)
                now = time.monotonic_ns()
                console += line
                # Kernel printk can split a userspace serial write mid-marker.
                # Preserve the raw log; remove only timestamped kernel lines for
                # marker recognition, never guest assertions or VMM results.
                markers = userspace_bytes(console)
                if trigger is None and b"CLONE FREEZE" in markers: trigger = now
                if b"COOPERATIVE FROZEN" in line: frozen = now
            result = process.wait()
        finally:
            timer.cancel()
    log = source_log.read_bytes()
    if result or b"result=Frozen" not in log or trigger is None or frozen is None:
        raise RuntimeError(f"source failed: {prefix}")
    return disk, image, (time.monotonic_ns() - start) / 1e6, (frozen-trigger)/1e6

def run_denied_child(cpus, source_disk, image, name):
    disk = args.output / f"{name}.deny.ext4"
    subprocess.run(["cp", "--reflink=auto", str(source_disk), str(disk)], check=True)
    env = environment(cpus, disk)
    env["CLONE_DENY"] = "1"
    fd = template_fd(image) if args.passed_fd else None
    mode, source = ("resume-fd", str(fd)) if fd is not None else ("resume", str(image))
    result = subprocess.run([str(launcher), mode, source, args.control], env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=30,
                            pass_fds=(fd,) if fd is not None else ())
    (args.output / f"{name}.deny.log").write_bytes(result.stdout)
    if result.returncode or b"CLONE POLICY denied" not in result.stdout or b"CLONE PASS" in result.stdout:
        raise RuntimeError(f"default-closed clone policy failed: {name}")
    disk.unlink()

def run_child(cpus, source_disk, image, name):
    disk = args.output / f"{name}.ext4"
    subprocess.run(["cp", "--reflink=auto", str(source_disk), str(disk)], check=True)
    log_path = args.output / f"{name}.resume.log"
    start = time.monotonic_ns()
    with log_path.open("wb") as log:
        fd = template_fd(image) if args.passed_fd else None
        mode, source = ("resume-fd", str(fd)) if fd is not None else ("resume", str(image))
        process = subprocess.Popen([str(launcher), mode, source, args.control],
                                   env=environment(cpus, disk), stdout=log, stderr=log,
                                   pass_fds=(fd,) if fd is not None else ())
        deadline = time.monotonic() + 30
        userspace = None
        private_kib = None
        try:
            while process.poll() is None:
                data = log_path.read_bytes()
                if userspace is None and b"CLONE USERSPACE" in data:
                    userspace = time.monotonic_ns()
                if userspace is not None and private_kib is None and time.monotonic_ns() - userspace >= 10_000_000_000:
                    smaps = Path(f"/proc/{process.pid}/smaps_rollup").read_text()
                    private_kib = sum(int(v) for v in re.findall(r"^Private_(?:Clean|Dirty):\s+(\d+)", smaps, re.M))
                if time.monotonic() > deadline:
                    raise TimeoutError(f"child timed out: {name}")
                threading.Event().wait(.005)
            if process.returncode:
                raise RuntimeError(f"child process failed: {name}")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    data = userspace_bytes(log_path.read_bytes()).decode(errors="replace")
    required = ("CLONE TSC nondecreasing", "CLONE VECTOR width=", "CLONE FORWARDING disabled",
                "CLONE VMGENID irq=", "CLONE CLOCK error_ns=", "CLONE USERSPACE clock_error_ns=")
    if any(marker not in data for marker in required):
        raise RuntimeError(f"missing independent device check: {name}")
    if "CLONE PASS" not in data or "CLONE FAIL" in data or "SIMD=ok" not in data or f"cpus={cpus}" not in data:
        raise RuntimeError(f"child assertions failed: {name}")
    if userspace is None or private_kib is None:
        raise RuntimeError(f"missing timing/private-memory sample: {name}")
    fsck = subprocess.run(["e2fsck", "-fn", str(disk)], capture_output=True)
    (args.output / f"{name}.fsck.log").write_bytes(fsck.stdout + fsck.stderr)
    if fsck.returncode:
        raise RuntimeError(f"filesystem check failed: {name}")
    disk.unlink()
    generation = re.search(r"COOPERATIVE GENERATION (\[[^\n]+\])", data)
    if generation is None:
        raise RuntimeError(f"missing generation identifier: {name}")
    random = re.search(r"CLONE RANDOM ([0-9a-f]{64})", data)
    if random is None or b"crng reseeded due to virtual machine fork" not in log_path.read_bytes():
        raise RuntimeError(f"missing guest randomness/reseed proof: {name}")
    pending = re.search(r"CLONE PENDING_RANDOM ([0-9a-f]{64}) EVENT=ok", data)
    if pending is None:
        raise RuntimeError(f"missing independent reader/event proof: {name}")
    source_data = userspace_bytes(image.with_suffix(".freeze.log").read_bytes()).decode(errors="replace")
    available = "CLONE CHECKS userfaultfd=available blocked_random=required pending_random=required devices=required"
    unavailable = "CLONE CHECKS userfaultfd=unavailable blocked_random=skipped pending_random=required devices=required"
    if source_data.count(available) + source_data.count(unavailable) != 1:
        raise RuntimeError(f"missing or ambiguous source check declaration: {name}")
    blocked = re.search(r"CLONE BLOCKED_RANDOM ([0-9a-f]{64})", data)
    blocked_required = available in source_data
    skipped = "CLONE BLOCKED_RANDOM skipped userfaultfd=unavailable" in data
    if (blocked_required and (blocked is None or skipped)) or (not blocked_required and (blocked is not None or not skipped)):
        raise RuntimeError(f"blocked random-read coverage mismatch: {name}")
    return {"blocked_random": blocked.group(1) if blocked else None, "cpus": cpus, "name": name, "resume_ms": (userspace-start)/1e6,
            "private_kib_10s": private_kib, "generation": generation.group(1),
            "random": random.group(1), "pending_random": pending.group(1)}

try:
    ready()
    last = None
    for cpus in (1, 4, 8):
        for cycle in range(args.cycles):
            disk, image, source_ms, freeze_ms = run_source(cpus, cycle)
            if cycle == 0:
                run_denied_child(cpus, disk, image, f"cpu{cpus}-{cycle}")
            record = run_child(cpus, disk, image, f"cpu{cpus}-{cycle}-child")
            record["source_boot_freeze_export_ms"] = source_ms
            record["freeze_ms"] = freeze_ms
            measurements.append(record)
            (args.output / "measurements.json").write_text(json.dumps(measurements, indent=2))
            print(f"PASS cpu={cpus} cycle={cycle}", flush=True)
            if last:
                if last[1] in template_fds:
                    os.close(template_fds.pop(last[1]))
                last[0].unlink(); last[1].unlink()
            last = (disk, image)
    # One stopped template, many independent VMM processes alive concurrently.
    if args.concurrent:
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrent) as pool:
            results = list(pool.map(lambda i: run_child(8, *last, f"concurrent-{i}"), range(args.concurrent)))
        measurements.extend(results)
    (args.output / "measurements.json").write_text(json.dumps(measurements, indent=2))
    for cpus in (1,4,8):
        samples = [m for m in measurements if m["cpus"] == cpus and m["name"].startswith("cpu")]
        for field in ("freeze_ms", "resume_ms", "private_kib_10s"):
            values = [m[field] for m in samples]
            print(f"cpu={cpus} {field} median={statistics.median(values)} worst={max(values)}")
    if len({m["generation"] for m in measurements}) != len(measurements):
        raise RuntimeError("duplicate VM generation identifier")
    if len({m["random"] for m in measurements}) != len(measurements):
        raise RuntimeError("duplicate guest random bytes")
    blocked_samples = [m["blocked_random"] for m in measurements if m["blocked_random"] is not None]
    if len(set(blocked_samples)) != len(blocked_samples):
        raise RuntimeError("duplicate blocked-reader random bytes")
    if len({m["pending_random"] for m in measurements}) != len(measurements):
        raise RuntimeError("duplicate independent-reader random bytes")
    try:
        unexpected, _ = tcp_probe.accept()
    except BlockingIOError:
        pass
    else:
        unexpected.close()
        raise RuntimeError("disabled forwarding reached live host TCP listener")
    print("ACCEPTANCE PASS", flush=True)
finally:
    tcp_probe.close()
    for fd in template_fds.values():
        os.close(fd)
    peer.terminate()
    try: peer.wait(timeout=5)
    except subprocess.TimeoutExpired:
        peer.kill(); peer.wait()
    peer_path.unlink(missing_ok=True)
