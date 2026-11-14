#!/usr/bin/env python3
"""Retain VM descriptors across VMM SIGKILL, then serialize final closes."""
import array
import fcntl
import json
import os
import pathlib
import random
import socket
import subprocess
import sys
import time

image, lock_path, count, width, first_cid = sys.argv[1:]
count, width, first_cid = map(int, (count, width, first_cid))
assert width in (1, 2) and 200 <= first_cid <= 398
binary = pathlib.Path('target/release/examples/nitro_supervised').resolve()
root = pathlib.Path('supervised-results')
root.mkdir(exist_ok=True)
pathlib.Path(lock_path).touch(exist_ok=True)

def accounting():
    base = pathlib.Path('/sys/class/misc/nitro_enclaves')
    return {name: (base / name).read_text().strip() for name in ('pcie_slots_in_use', 'mem_pool/in_use', 'mem_pool/free') if (base / name).exists()}

baseline = accounting()
rng = random.Random(20261114)
for trial in range(count):
    processes, descriptors, connections, listeners = [], [], [], []
    try:
        for index in range(width):
            path = root / f'{os.getpid()}-{trial}-{index}.sock'
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.settimeout(15)
            listener.bind(str(path))
            listener.listen(1)
            listeners.append((listener, path))
            log = open(root / f'{trial}-{index}.log', 'wb')
            process = subprocess.Popen([str(binary), image, lock_path, str(path), str(first_cid + index)], stdout=log, stderr=subprocess.STDOUT)
            log.close()
            processes.append(process)
            connection, _ = listener.accept()
            connection.settimeout(10)
            connections.append(connection)
            data, ancillary, flags, _ = connection.recvmsg(1, socket.CMSG_SPACE(4), socket.MSG_CMSG_CLOEXEC)
            assert data == b'F' and not flags & socket.MSG_CTRUNC
            received = array.array('i')
            for level, kind, value in ancillary:
                if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                    received.frombytes(value)
            assert len(received) == 1
            descriptors.append(received[0])
        for connection in connections:
            connection.sendall(b'G')
        # Includes immediate kill during start and later firmware/guest execution.
        time.sleep([0, 0.001, 0.015, 0.080, 0.300][trial % 5] + rng.random() * 0.001)
        for process in processes:
            process.kill()
        for process in processes:
            assert process.wait(timeout=15) == -9
        # Every final close is serialized, including the simultaneous two-VMM kill.
        while descriptors:
            descriptor = descriptors[0]
            with open(lock_path, 'rb') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                os.close(descriptor)
                descriptors.pop(0)
                fcntl.flock(lock, fcntl.LOCK_UN)
        current = accounting()
        assert current == baseline, (baseline, current)
        print(json.dumps({'trial': trial, 'guests': width, 'accounting': current}), flush=True)
    finally:
        for process in processes:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=15)
        for descriptor in descriptors:
            with open(lock_path, 'rb') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                os.close(descriptor)
        for connection in connections:
            connection.close()
        for listener, path in listeners:
            listener.close()
            path.unlink(missing_ok=True)
print(json.dumps({'passed': count * width, 'baseline': baseline, 'final': accounting()}), flush=True)
