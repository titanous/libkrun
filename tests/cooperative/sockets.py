#!/usr/bin/env python3
"""Forwarded sockets across a guest copy on x86-64 KVM.

Usage: sockets.py --kernel VMLINUX --init STATIC_SOCKETS_BINARY --output NEW_DIR
The guest program (sockets.c, as /init) leaves forwarded sockets in each state, freezes
(the source VMM exits with a template), and the copy checks that every forwarded socket
fails definitely on its next use and recovers when used afresh, while native sockets keep
working. Host side, on the host's own interface address: TCP echo 29101, TCP 29102 with a
full accept queue (SYNs dropped, so connects stay pending), UDP echo 29103. Exit 0 only if
the copy reports no failures and neither console has a kernel warning.
"""
import argparse
import os
from pathlib import Path
import socket
import subprocess
import sys
import threading

parser = argparse.ArgumentParser()
parser.add_argument("--kernel", required=True)
parser.add_argument("--init", required=True)
parser.add_argument("--output", required=True, type=Path)
parser.add_argument("--launcher", default=str(Path(__file__).resolve().parents[2]
                                                / "target/release/examples/cooperative-console"))
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=False)
WARNINGS = ("WARNING:", "BUG:", "refcount_t:", "leaking memory", "Oops", "general protection")


def host_address():
    """The host's own interface address (a guest UDP connect to loopback stays guest-local)."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
        route.connect(("192.0.2.1", 9))
        return route.getsockname()[0]


HOST = host_address()


def cpio(path, init):
    """A newc archive holding /init and /address."""
    data = Path(init).read_bytes()

    def entry(name, mode, body, ino):
        name_bytes = name.encode() + b"\0"
        header = "070701" + "".join(f"{v:08x}" for v in (
            ino, mode, 0, 0, 1, 0, len(body), 0, 0, 0, 0, len(name_bytes), 0))
        out = header.encode() + name_bytes
        out += b"\0" * (-len(out) % 4) + body
        return out + b"\0" * (-len(out) % 4)

    Path(path).write_bytes(entry("init", 0o100755, data, 1)
                           + entry("address", 0o100644, (HOST + "\n").encode(), 2)
                           + entry("TRAILER!!!", 0, b"", 0))


stop = threading.Event()


def serve_tcp_echo(listener):
    def echo(connection):
        with connection:
            connection.settimeout(30)
            try:
                while data := connection.recv(65536):
                    connection.sendall(data)
            except OSError:
                pass

    while not stop.is_set():
        try:
            connection, _ = listener.accept()
        except socket.timeout:
            continue
        except OSError:
            return
        threading.Thread(target=echo, args=(connection,), daemon=True).start()


def serve_udp_echo(server):
    while not stop.is_set():
        try:
            data, peer = server.recvfrom(65536)
            server.sendto(data, peer)
        except socket.timeout:
            continue
        except OSError:
            return


tcp = socket.socket()
tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
tcp.bind((HOST, 29101))
tcp.listen(64)
tcp.settimeout(0.2)
held = socket.socket()
held.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
held.bind((HOST, 29102))
held.listen(0)
filler = socket.create_connection((HOST, 29102), timeout=5)  # fills the accept queue
udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
udp.bind((HOST, 29103))
udp.settimeout(0.2)
threads = [threading.Thread(target=serve_tcp_echo, args=(tcp,), daemon=True),
           threading.Thread(target=serve_udp_echo, args=(udp,), daemon=True)]
for thread in threads:
    thread.start()

peer_path = args.output / "agent.sock"
peer = subprocess.Popen([sys.executable, str(Path(__file__).with_name("echo.py")), str(peer_path)])
initramfs = args.output / "initramfs.cpio"
cpio(initramfs, args.init)
image = args.output / "template"
env = dict(os.environ, CLONE_KERNEL=args.kernel, CLONE_INITRAMFS=str(initramfs), CLONE_VCPUS="2",
           CLONE_ECHO_SOCKET=str(peer_path), CLONE_INTERNET="1")
failures = []
try:
    for _ in range(200):
        if peer_path.exists():
            break
        stop.wait(0.05)
    source = subprocess.run([args.launcher, "freeze", str(image), "0"], env=env, capture_output=True, timeout=180)
    source_text = (source.stdout + source.stderr).decode(errors="replace")
    (args.output / "source.log").write_text(source_text)
    print(f"source exit={source.returncode} ready={'COPY-SOCKETS READY' in source_text}", flush=True)
    if source.returncode or "COPY-SOCKETS READY" not in source_text:
        failures.append("source")
    copy = subprocess.run([args.launcher, "resume", str(image), "0"], env=env, capture_output=True, timeout=180)
    copy_text = (copy.stdout + copy.stderr).decode(errors="replace")
    (args.output / "copy.log").write_text(copy_text)
    for line in copy_text.splitlines():
        if "copy-socket" in line or "COPY-SOCKETS" in line or "crng reseeded" in line:
            print("  " + line.strip(), flush=True)
    print(f"copy exit={copy.returncode}", flush=True)
    if copy.returncode or "COPY-SOCKETS failures=0" not in copy_text:
        failures.append("copy")
    for name, text in (("source", source_text), ("copy", copy_text)):
        warned = [w for w in WARNINGS if w in text]
        print(f"{name} console_warnings={warned}", flush=True)
        if warned:
            failures.append(name + "-console")
finally:
    stop.set()
    peer.terminate()
    peer.wait(timeout=5)
    for s in (tcp, held, filler, udp):
        s.close()
    image.unlink(missing_ok=True)
print(f"COPY SOCKET TEST failures={failures}", flush=True)
sys.exit(1 if failures else 0)
