"""Reference EIF format copied from holodeck nitro-kernel guest/image/eif.py.
Source: kernel lane 0844636, 2026-11 review remediation; test-only oracle.
"""
import json
import struct
import sys
import zlib
from pathlib import Path
HEADER = struct.Struct(">4sHHQQHH32Q32QII")
SECTION = struct.Struct(">HHQ")
METADATA = json.dumps({"ImageName": "holodeck", "ImageVersion": "1", "BuildMetadata": {
    "BuildTime": "", "BuildTool": "holodeck", "BuildToolVersion": "1",
    "OperatingSystem": "Linux", "KernelVersion": ""}, "DockerInfo": {},
    "CustomMetadata": None}, separators=(",", ":")).encode()
def build(kernel, initrd, cmdline, memory=1024 << 20, cpus=2):
    sections = [(1, kernel), (2, cmdline)]
    if initrd is not None:
        sections.append((3, initrd))
    sections.append((5, METADATA))
    offsets, sizes, body = [0]*32, [0]*32, bytearray()
    for i, (kind, payload) in enumerate(sections):
        offsets[i], sizes[i] = HEADER.size + len(body), len(payload)
        body.extend(SECTION.pack(kind, 0, len(payload)))
        body.extend(payload)
    image = bytearray(HEADER.pack(b".eif", 4, 0xa41, memory, cpus, 0,
        len(sections), *offsets, *sizes, 0, 0)) + body
    crc = zlib.crc32(image[HEADER.size:], zlib.crc32(image[:HEADER.size-4]))
    struct.pack_into(">I", image, HEADER.size-4, crc)
    return bytes(image)
if __name__ == "__main__":
    initrd = None if sys.argv[1] == "none" else b"initramfs\0bytes"
    Path(sys.argv[2]).write_bytes(build(b"kernel\0bytes", initrd, b"console=hvc0 panic=-1"))
