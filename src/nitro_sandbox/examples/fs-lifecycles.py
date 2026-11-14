"""Run as root from repo: fs-lifecycles.py Image INITRAMFS MODE COUNT."""
import json
import os
import pathlib
import subprocess
import sys
root = pathlib.Path.cwd()

def accounting():
    base = pathlib.Path('/sys/class/misc/nitro_enclaves')
    result = {key: (base/key).read_text().strip() for key in
              ('pcie_slots_in_use', 'mem_pool/in_use', 'mem_pool/free')}
    result['harness_fds'] = len(list(pathlib.Path('/proc/self/fd').iterdir()))
    return result

before = accounting()
count = int(sys.argv[4])
env = dict(os.environ, NITRO_FS_TRIALS=str(count))
with (root/'fs-lifecycles-guests.log').open('wb') as output:
    result = subprocess.run([str(root/'target/release/examples/nitro_fs'), *sys.argv[1:4]],
                            env=env, stdout=output, stderr=subprocess.STDOUT,
                            timeout=max(30, count*4))
text = (root/'fs-lifecycles-guests.log').read_bytes()
after = accounting()
summary = dict(requested=count, passed=text.count(b'NITRO_FS_PASS'), exit=result.returncode,
               before=before, after=after)
print(json.dumps(summary), flush=True)
assert result.returncode == 0 and summary['passed'] == count
assert before == after, 'shared accounting changed; check other-lane activity'
