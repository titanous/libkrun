#!/usr/bin/env python3
"""Guest-initiated vsock test peer, exposed by the VMM as a Unix socket."""
import os
import socket
import struct
import sys
import threading
import time

path = sys.argv[1]
if os.path.exists(path):
    raise SystemExit(f"refusing existing socket path: {path}")
server = socket.socket(socket.AF_UNIX)
server.bind(path)
server.listen(64)

def serve(connection):
    with connection:
        while True:
            data = connection.recv(4096)
            if not data:
                return
            if data == b"time":
                connection.sendall(struct.pack("<Q", time.time_ns()))
            else:
                connection.sendall(data)

try:
    while True:
        connection, _ = server.accept()
        threading.Thread(target=serve, args=(connection,), daemon=True).start()
finally:
    server.close()
    os.unlink(path)
