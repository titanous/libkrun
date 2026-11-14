#!/usr/bin/env python3
"""Write a newc initramfs holding /init plus /dev/console (5:1) and /dev/null (1:3).

usage: random-initramfs.py INIT_BINARY OUT.cpio
Used for the arm64 Nitro run of the random-copy test, where the fixture is cross-built rather than
packaged by tests/cooperative/image.nix.
"""
import pathlib
import sys


def entry(name, mode, data=b"", rdev=(0, 0), ino=1):
    fields = [ino, mode, 0, 0, 1, 0, len(data), 0, 0, rdev[0], rdev[1], len(name) + 1, 0]
    head = b"070701" + "".join(f"{v:08x}" for v in fields).encode() + name.encode() + b"\0"
    head += b"\0" * (-len(head) % 4)
    return head + data + b"\0" * (-len(data) % 4)


init = pathlib.Path(sys.argv[1]).read_bytes()
archive = b"".join([
    entry("dev", 0o040755, ino=1),
    entry("dev/console", 0o020600, rdev=(5, 1), ino=2),
    entry("dev/null", 0o020666, rdev=(1, 3), ino=3),
    entry("init", 0o100755, init, ino=4),
    entry("TRAILER!!!", 0, ino=0),
])
archive += b"\0" * (-len(archive) % 512)
pathlib.Path(sys.argv[2]).write_bytes(archive)
