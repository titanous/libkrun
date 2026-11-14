"""Prepare a read-only committed treefs snapshot locally, without editing its lane.
Run from fork root: python3 examples/treefs-nitro/prepare.py /path/to/holodeck-treefs
"""
import pathlib
import subprocess
import sys
import tarfile
import io
root = pathlib.Path.cwd()
source = pathlib.Path(sys.argv[1])
target = root/'target/treefs-source'
target.mkdir(parents=True, exist_ok=True)
archive = subprocess.check_output(['git','-C',str(source),'archive','HEAD'])
with tarfile.open(fileobj=io.BytesIO(archive)) as tree:
    tree.extractall(target, filter='data')
manifest = target/'Cargo.toml'
text = manifest.read_text()
old = 'devices = { path = "../libkrun/src/devices" }'
assert text.count(old) == 1
manifest.write_text(text.replace(old,'devices = { path = "../../src/devices" }'))
manifest = target/'crates/treefs/Cargo.toml'
text = manifest.read_text()
manifest.write_text(text.replace('[package]\n','[package]\nworkspace = "../.."\n',1))
print('treefs_snapshot=' + subprocess.check_output(['git','-C',str(source),'rev-parse','HEAD'], text=True).strip())
