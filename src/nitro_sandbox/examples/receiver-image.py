#!/usr/bin/env python3
"""Inject a linked arm64 parked receiver into a Linux Image's PE entry.

The receiver must reserve [0x70000000,0x70200000), matching kernel patch
0012. The Linux Image remains a firmware-loadable PE container; it is never
booted as Linux. Only use a receiver with the exact kernel identity captured
in the sealed template manifest.
"""
import pathlib
import struct
import sys


def inject(image, code):
    data = bytearray(image)
    if len(data) < 64 or data[:2] != b'MZ' or not code:
        raise ValueError('invalid arm64 PE image or empty receiver')
    pe = struct.unpack_from('<I', data, 0x3c)[0]
    if pe + 24 > len(data) or data[pe:pe + 4] != b'PE\0\0':
        raise ValueError('invalid PE header')
    machine, count = struct.unpack_from('<HH', data, pe + 4)
    optional = struct.unpack_from('<H', data, pe + 20)[0]
    if machine != 0xaa64 or optional < 20 or pe + 24 + optional > len(data):
        raise ValueError('not a bounded arm64 PE image')
    entry = struct.unpack_from('<I', data, pe + 40)[0]
    sections = pe + 24 + optional
    if count > (len(data) - sections) // 40:
        raise ValueError('truncated section table')
    for index in range(count):
        section = sections + 40 * index
        _, virtual_size, address, raw_size, raw = struct.unpack_from('<8sIIII', data, section)
        flags = struct.unpack_from('<I', data, section + 36)[0]
        if address <= entry < address + min(virtual_size, raw_size):
            target = (entry + 4 + 4095) & ~4095
            if (raw != address or not flags & 0x20000000 or entry % 4
                    or target + len(code) > address + raw_size
                    or address + raw_size > len(data)
                    or target - entry >= 1 << 27):
                raise ValueError('receiver does not fit executable identity-layout section')
            data[target:target + len(code)] = code
            struct.pack_into('<I', data, entry, 0x14000000 | ((target - entry) >> 2))
            return bytes(data)
    raise ValueError('PE entry is outside file-backed executable sections')


if __name__ == '__main__':
    if len(sys.argv) != 4:
        raise SystemExit('usage: receiver-image.py Image receiver.bin output-Image')
    result = inject(pathlib.Path(sys.argv[1]).read_bytes(), pathlib.Path(sys.argv[2]).read_bytes())
    pathlib.Path(sys.argv[3]).write_bytes(result)
    print('RECEIVER_IMAGE bytes=%d' % len(result))
