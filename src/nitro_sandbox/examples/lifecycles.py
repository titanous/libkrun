#!/usr/bin/env python3
"""Bounded one-process-per-guest acceptance run; validate every guest marker."""
import concurrent.futures
import json
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import threading
import time
root = pathlib.Path.cwd()
binary = pathlib.Path(os.environ.get('NITRO_BINARY', str(root/'target/debug/examples/nitro_smoke')))
image = pathlib.Path(os.environ.get('NITRO_IMAGE', str(root/'smoke-console.eif')))
count, concurrency = int(sys.argv[1]), int(sys.argv[2])
duration = int(sys.argv[3]) if len(sys.argv) > 3 else 0
assert 1 <= concurrency <= 8
logs = root / ('acceptance-' + str(int(time.time())))
logs.mkdir()
markers = [b'NITRO_DISK_OK', b'NITRO_VSOCK_OK', b'NITRO_SMOKE_OK']
def accounting():
    result = {}
    for name in ['pcie_slots', 'pcie_slots_in_use', 'mem_pool/total', 'mem_pool/in_use', 'mem_pool/free']:
        p = pathlib.Path('/sys/class/misc/nitro_enclaves') / name
        if p.is_file():
            try: result[name] = p.read_text().strip()
            except OSError: pass
    result['harness_fds'] = len(list(pathlib.Path('/proc/self/fd').iterdir()))
    result['harness_threads'] = len(list(pathlib.Path('/proc/self/task').iterdir()))
    survivors = []
    for proc in pathlib.Path('/proc').iterdir():
        if not proc.name.isdigit(): continue
        try:
            command = (proc / 'cmdline').read_bytes()
            if b'nitro_smoke' in command and str(root).encode() in command:
                survivors.append(int(proc.name))
        except (OSError, PermissionError): pass
    result['lane_vmm_processes'] = survivors
    result['file_nr'] = pathlib.Path('/proc/sys/fs/file-nr').read_text().strip()
    result['loadavg'] = pathlib.Path('/proc/loadavg').read_text().strip()
    return result
before = accounting()
def launch(i, slot=None):
    slot = i % concurrency if slot is None else slot
    disk = logs / f'disk-{i}.ext4'
    shutil.copyfile(root / 'smoke.ext4', disk)
    path = str(logs / f'v-{i}.sock')
    payload = b'libkrun-nitro-ext4-verified\0'
    echo_result = []
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
        server.settimeout(25)
        server.bind(path)
        server.listen(1)
        def echo():
            try:
                connection, _ = server.accept()
                with connection:
                    connection.settimeout(5)
                    data = connection.recv(128)
                    connection.sendall(data)
                    echo_result.append(data == payload)
                    # Guest closes after verifying the complete echo. Keep the
                    # stream alive until then so this test does not conflate
                    # payload delivery with immediate peer-close behavior.
                    connection.recv(1)
            except Exception as e:
                echo_result.append(str(e))
        thread = threading.Thread(target=echo)
        thread.start()
        start = time.monotonic()
        with (logs / f'guest-{i}.log').open('wb') as output:
            try:
                p = subprocess.run(['sudo', 'env', f'NITRO_MEMORY_MIB={os.environ.get("NITRO_MEMORY_MIB", "1024")}', f'NITRO_CID={200+slot}', f'NITRO_CONSOLE_LOG={logs / f"console-{i}.log"}', str(binary), str(image), str(disk), path], stdout=output, stderr=subprocess.STDOUT, timeout=35)
                code = p.returncode
            except subprocess.TimeoutExpired:
                code = 'timeout'
        thread.join(30)
    data = (logs / f'guest-{i}.log').read_bytes()
    console = (logs / f'console-{i}.log').read_bytes() if (logs / f'console-{i}.log').exists() else b''
    passed = code == 0 and echo_result == [True] and all(m in data for m in markers) and b'NITRO_VIRTIO_CONSOLE_OK' in console and b'NITRO_SMOKE_FAIL' not in data
    disk.unlink()
    pathlib.Path(path).unlink(missing_ok=True)
    result = {'guest': i, 'passed': passed, 'exit': code, 'seconds': time.monotonic()-start, 'echo': echo_result}
    print(json.dumps(result), flush=True)
    return result
start = time.monotonic()
results = []
# Each worker retains one CID and refills only after its previous VMM exits.
# No more than concurrency guests or writable disks are alive at once.
stop = threading.Event()
result_lock = threading.Lock()
def worker(slot):
    i = slot
    while not stop.is_set() and ((time.monotonic()-start < duration) if duration else (i < count)):
        result = launch(i, slot)
        with result_lock: results.append(result)
        if not result['passed']: stop.set()
        i += concurrency
with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
    list(pool.map(worker, range(concurrency)))
elapsed = time.monotonic()-start
summary = {'requested': count, 'duration_requested': duration, 'concurrency': concurrency, 'completed': len(results), 'passed': sum(r['passed'] for r in results), 'seconds': elapsed, 'launches_per_second': len(results)/elapsed, 'before': before, 'after': accounting()}
(logs/'summary.json').write_text(json.dumps(summary, indent=2))
print(json.dumps(summary), flush=True)
complete = elapsed >= duration if duration else len(results) == count
sys.exit(0 if complete and all(r['passed'] for r in results) else 1)
