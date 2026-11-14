"""Run as root from repo: stops.py IMAGE LOCK handle|term|kill|poweroff COUNT CID.

On m9g invoke only poweroff mode, one guest, under the outer /tmp/m9g.lock.
"""
import fcntl
import json
import os
import pathlib
import select
import signal
import subprocess
import sys
import time

image, lock_path, mode, count, cid = sys.argv[1:]
assert mode in ("handle", "term", "kill", "poweroff")
assert 200 <= int(cid) <= 399
root = pathlib.Path.cwd()
logs = root / ("stops-" + mode + "-" + str(time.time_ns()))
logs.mkdir()
lock = pathlib.Path(lock_path)
lock.touch(exist_ok=True)

def accounting():
    base = pathlib.Path("/sys/class/misc/nitro_enclaves")
    return {key: (base / key).read_text().strip()
            for key in ("pcie_slots_in_use", "mem_pool/in_use", "mem_pool/free")}

before = accounting()
for trial in range(int(count)):
    command = [str(root / "target/release/examples/nitro_stop"), image,
               str(lock), "signal" if mode in ("term", "kill") else mode, cid]
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    captured = bytearray()
    deadline = time.monotonic() + 10
    try:
        while b"STOP_TEST_READY" not in captured or (mode != "poweroff" and b"NITRO_IDLE_READY" not in captured):
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("VMM did not become ready")
            if select.select([process.stdout], [], [], 0.2)[0]:
                captured.extend(os.read(process.stdout.fileno(), 4096))
        if mode == "kill":
            # Daemon rule: lock BEFORE kill, retain through reaping. The VM fd
            # closes during process exit, before wait can report completion.
            with lock.open("r+") as daemon_lock:
                fcntl.flock(daemon_lock, fcntl.LOCK_EX)
                process.kill()
                captured.extend(process.communicate(timeout=15)[0])
                assert process.returncode == -signal.SIGKILL
                fcntl.flock(daemon_lock, fcntl.LOCK_UN)
        else:
            if mode == "term":
                process.terminate()
            captured.extend(process.communicate(timeout=20)[0])
            assert process.returncode == 0
            assert b"STOP_TEST_OK" in captured
    except BaseException:
        # Never leave a timed-out guest, and never kill it outside the lock.
        with lock.open("r+") as daemon_lock:
            fcntl.flock(daemon_lock, fcntl.LOCK_EX)
            if process.poll() is None:
                process.kill()
            captured.extend(process.communicate(timeout=15)[0])
        raise
    finally:
        (logs / (str(trial) + ".log")).write_bytes(captured)
    print("STOP_PASS mode=" + mode + " trial=" + str(trial), flush=True)
after = accounting()
summary = dict(mode=mode, count=int(count), before=before, after=after)
(logs / "summary.json").write_text(json.dumps(summary, indent=2))
print(json.dumps(summary), flush=True)
assert before == after, "shared accounting changed; inspect concurrent lanes before attributing"
