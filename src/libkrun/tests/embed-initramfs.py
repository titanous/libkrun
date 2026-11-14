#!/usr/bin/env python3
"""Pack a static embed-init executable as /init in a deterministic newc archive."""
import pathlib
import sys


def entry(name, data, mode, ino):
    name = name.encode() + b"\0"
    fields = [ino, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name), 0]
    result = b"070701" + b"".join(f"{n:08x}".encode() for n in fields) + name
    result += b"\0" * (-len(result) % 4)
    result += data
    return result + b"\0" * (-len(data) % 4)


if __name__ == "__main__":
    init, output = map(pathlib.Path, sys.argv[1:])
    archive = entry("init", init.read_bytes(), 0o100755, 1)
    archive += entry("TRAILER!!!", b"", 0, 2)
    output.write_bytes(archive + b"\0" * (-len(archive) % 512))
