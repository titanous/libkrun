#!/usr/bin/env python3
"""Inspect EIF section metadata and extract its kernel/initramfs for test builds."""
import pathlib
import struct
import sys
p = pathlib.Path(sys.argv[1]).read_bytes()
h = struct.unpack('>4sHHQQHH32Q32QII', p[:548])
print('magic/version/flags/memory/cpus/sections', h[:7])
root = pathlib.Path(sys.argv[2])
root.mkdir(exist_ok=True)
for i in range(h[6]):
    at = h[7+i]
    kind, flags, size = struct.unpack('>HHQ', p[at:at+12])
    data = p[at+12:at+12+size]
    print(i, kind, flags, size)
    (root / f'section-{kind}-{i}').write_bytes(data)
