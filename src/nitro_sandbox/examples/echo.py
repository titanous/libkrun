#!/usr/bin/env python3
import os
import socket
import sys
path = sys.argv[1]
try:
    os.unlink(path)
except FileNotFoundError:
    pass
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
    server.settimeout(35)
    server.bind(path)
    server.listen(1)
    connection, _ = server.accept()
    with connection:
        connection.settimeout(5)
        data = connection.recv(128)
        connection.sendall(data)
        if data != b'libkrun-nitro-ext4-verified\0':
            raise RuntimeError(f'unexpected payload {data!r}')
        print('HOST_VSOCK_OK', flush=True)
os.unlink(path)
