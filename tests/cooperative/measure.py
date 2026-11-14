#!/usr/bin/env python3
"""Release first-user-return and private RSS matrix for lightly populated guests."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import subprocess
import threading
import time

p = argparse.ArgumentParser()
p.add_argument('--kernel', required=True)
p.add_argument('--initramfs', required=True)
p.add_argument('--output', required=True, type=Path)
a = p.parse_args()
a.output.mkdir(exist_ok=False)
launcher = Path('target/release/examples/cooperative-console').resolve()
rows = []

def private(pid):
    text = Path(f'/proc/{pid}/smaps_rollup').read_text()
    return sum(map(int, re.findall(r'^Private_(?:Clean|Dirty):\s+(\d+)', text, re.M)))

def execute(size, mode, image, fd=None):
    env = dict(os.environ, CLONE_KERNEL=a.kernel, CLONE_INITRAMFS=a.initramfs,
               CLONE_MEMORY=str(size), CLONE_VCPUS='1')
    command = [str(launcher), mode, str(fd if fd is not None else image), '0']
    start = time.monotonic_ns()
    child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, pass_fds=() if fd is None else (fd,))
    timer = threading.Timer(60, child.kill)
    timer.start()
    row = {'memory_mib': size, 'mode': mode}
    samples = []
    def sample_idle():
        try:
            row['private_kib_10s'] = private(child.pid)
        except FileNotFoundError:
            row['private_kib_10s'] = None
    sample = sum(row['memory_mib'] == size and row['mode'] == mode for row in rows)
    row['sample'] = sample
    with (a.output / f'{size}-{mode}-{sample}.log').open('wb') as log:
        for line in child.stdout:
            log.write(line)
            if b'TIMING FIRST USER' in line:
                row['first_user_ms'] = (time.monotonic_ns() - start) / 1e6
                row['private_kib_start'] = private(child.pid)
                sampler = threading.Timer(10, sample_idle)
                sampler.start()
                samples.append(sampler)
    code = child.wait()
    timer.cancel()
    for sampler in samples:
        sampler.join(12)
    if code or (mode != 'freeze' and 'private_kib_10s' not in row):
        raise RuntimeError(f'failed size={size} mode={mode} exit={code}')
    if mode != 'freeze':
        rows.append(row)
        (a.output / 'measurements.json').write_text(json.dumps(rows, indent=2))
        print(json.dumps(row), flush=True)

for size in (256, 2048, 8192):
    image = a.output / f'{size}.ram'
    execute(size, 'freeze', image)
    fd = os.memfd_create('measurement-template', os.MFD_ALLOW_SEALING)
    os.ftruncate(fd, image.stat().st_size)
    with image.open('rb') as source:
        offset = 0
        while block := source.read(1024 * 1024):
            for index in range(0, len(block), 4096):
                page = block[index:index + 4096]
                if any(page):
                    os.pwrite(fd, page, offset + index)
            offset += len(block)
    fcntl.fcntl(fd, fcntl.F_ADD_SEALS, fcntl.F_SEAL_WRITE | fcntl.F_SEAL_GROW |
                fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_SEAL)
    for sample in range(3):
        execute(size, 'resume', image)
        execute(size, 'resume-fd', image, fd)
    os.close(fd)
    image.unlink()
print('MEASUREMENT MATRIX PASS', flush=True)
