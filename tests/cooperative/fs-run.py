#!/usr/bin/env python3
"""Real-copy virtio-fs proof: fresh servers, retained handles/watches, bounded readiness."""
import argparse
import os
from pathlib import Path
import subprocess
p=argparse.ArgumentParser()
p.add_argument("--kernel",required=True)
p.add_argument("--initramfs",required=True)
p.add_argument("--output",required=True,type=Path)
p.add_argument("--children",type=int,default=20)
a=p.parse_args();a.output.mkdir(parents=True,exist_ok=False)
launcher=str(Path("target/debug/examples/cooperative-console").resolve())
for mode in ("plain","dax"):
    env=dict(os.environ,CLONE_KERNEL=a.kernel,CLONE_INITRAMFS=a.initramfs,CLONE_VCPUS="2",CLONE_FS=mode)
    image=a.output/f"{mode}.ram"
    def run(kind,name,environment=env):
        result=subprocess.run([launcher,kind,str(image),"0"],env=environment,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,timeout=40)
        (a.output/f"{name}.log").write_bytes(result.stdout)
        return result
    source=run("freeze",f"{mode}-source")
    if source.returncode or b"result=Frozen" not in source.stdout:
        raise RuntimeError(f"{mode} source freeze failed")
    wrong=dict(env,CLONE_FS="plain" if mode=="dax" else "dax")
    mismatch=run("resume",f"{mode}-mismatch",wrong)
    if not mismatch.returncode or b"topology mismatch" not in mismatch.stdout:
        raise RuntimeError("mismatched DAX topology was not rejected")
    for i in range(a.children):
        result=run("resume",f"{mode}-child-{i}")
        if result.returncode or b"CLONE FS PASS" not in result.stdout or any(x in result.stdout for x in (b"CLONE FS FAIL",b"WARNING:",b"BUG:")):
            raise RuntimeError(f"{mode} child {i} failed")
        if result.stdout.count(b"CLONE FS INIT")!=2:
            raise RuntimeError(f"{mode} child {i} did not initialize both fresh servers")
        print(f"PASS fs={mode} child={i}",flush=True)
print("FS CLONE ACCEPTANCE PASS",flush=True)
