#!/usr/bin/env python3
"""Assert ordinary guests cannot freeze and retain an established vsock channel."""
import argparse
import os
from pathlib import Path
import socket
import subprocess
import threading

p = argparse.ArgumentParser()
p.add_argument('--kernel', required=True)
p.add_argument('--initramfs', required=True)
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()
a.output.mkdir()
path = a.output / 'echo.sock'
server = socket.socket(socket.AF_UNIX)
server.bind(str(path))
server.listen()
server.settimeout(1)
stop = threading.Event()
workers = []
def echo(connection):
    with connection:
        connection.settimeout(5)
        try:
            while data := connection.recv(4096):
                connection.sendall(data)
        except OSError:
            pass

def serve():
    while not stop.is_set():
        try:
            connection, _ = server.accept()
            thread = threading.Thread(target=echo, args=(connection,))
            thread.start()
            workers.append(thread)
        except socket.timeout:
            pass
thread = threading.Thread(target=serve)
thread.start()
try:
    env = dict(os.environ, CLONE_KERNEL=a.kernel, CLONE_INITRAMFS=a.initramfs, CLONE_ECHO_SOCKET=str(path))
    result = subprocess.run(['target/release/examples/cooperative-console', 'ordinary', 'unused', '0'], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=45)
    (a.output / 'guest.log').write_bytes(result.stdout)
    assert result.returncode == 0 and b'ORDINARY TRIGGER PASS absent vsock-alive' in result.stdout
    if os.environ.get('CLONE_TEST_UNMARKED_ARM'):
        assert b'UNMARKED ARM typed TemplateMarkerMissing' in result.stdout
    print('PASS ordinary trigger absent; established vsock survives')
finally:
    stop.set()
    thread.join(2)
    server.close()
    path.unlink(missing_ok=True)
    for worker in workers:
        worker.join(6)
