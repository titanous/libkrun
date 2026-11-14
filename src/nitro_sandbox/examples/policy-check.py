#!/usr/bin/env python3
"""Real Nitro backend rule conformance: mapped host listener must not be reached."""
import os
import pathlib
import socket
import subprocess
root = pathlib.Path.cwd()
path = root/'policy-denied.sock'
path.unlink(missing_ok=True)
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
    listener.bind(str(path))
    listener.listen(1)
    listener.settimeout(0.2)
    with (root/'merged-policy.log').open('wb') as output:
        result = subprocess.run(['sudo','env','NITRO_DENY_ALL=1','NITRO_CID=215',f'NITRO_CONSOLE_LOG={root}/policy-console.log',str(root/'target/release/examples/nitro_smoke'),str(root/'policy-deny.eif'),str(root/'smoke.ext4'),str(path)],stdout=output,stderr=subprocess.STDOUT,timeout=25)
    assert result.returncode == 0
    assert b'NITRO_POLICY_DENY_OK' in (root/'merged-policy.log').read_bytes()
    try:
        connection,_ = listener.accept()
        connection.close()
        raise AssertionError('configured deny rule was ignored')
    except socket.timeout:
        pass
path.unlink()
print('NITRO_POLICY_HOST_UNREACHED_OK',flush=True)
