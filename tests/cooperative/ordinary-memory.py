#!/usr/bin/env python3
"""Inspect ordinary guest RAM after real user entry; always kill/reap the probe."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--launcher', required=True)
p.add_argument('--kernel', required=True)
p.add_argument('--initramfs', required=True)
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=False)
env = dict(os.environ, CLONE_KERNEL=a.kernel, CLONE_INITRAMFS=a.initramfs)
with (a.output / 'guest.log').open('wb') as log:
    vm = subprocess.Popen([a.launcher, 'ordinary', '/unused', '0'], env=env, stdout=log, stderr=log)
    try:
        end = time.monotonic() + 30
        while b'ORDINARY USERSPACE READY' not in (a.output / 'guest.log').read_bytes():
            if vm.poll() is not None or time.monotonic() >= end:
                raise RuntimeError('ordinary guest failed to reach user code')
            time.sleep(0.02)
        smaps = Path(f'/proc/{vm.pid}/smaps').read_text()
        regions = re.split(r'(?=^[0-9a-f]+-[0-9a-f]+ )', smaps, flags=re.M)
        ram = []
        for region in regions:
            lines = region.splitlines()
            if not lines:
                continue
            size = re.search(r'^Size:\s+(\d+) kB', region, re.M)
            if size and int(size.group(1)) >= 200 * 1024:
                fields = {line.split(':', 1)[0]: line.split(':', 1)[1].strip()
                          for line in lines[1:] if ':' in line}
                ram.append({'mapping': lines[0], **{k: fields[k] for k in
                            ('Size', 'Anonymous', 'AnonHugePages', 'Shared_Dirty', 'VmFlags')}})
        if not ram or any('/memfd:' in x['mapping'] or x['mapping'].split()[1] != 'rw-p' for x in ram):
            raise RuntimeError('ordinary RAM is not anonymous private memory')
        (a.output / 'memory.json').write_text(json.dumps(ram, indent=2))
        print(json.dumps(ram))
    finally:
        vm.kill()
        vm.wait(timeout=10)
