# Real treefs adapter acceptance on Nitro

This standalone package ports `crates/treefs/tests/guest.rs` and `guest-init.c`
from holodeck treefs commit `c70edfb` to the Nitro embedding API. It adds 1,000
read-only source files and complete 64 MiB read/mmap validation, retaining 3,000
writable create/write/rename/delete operations, tree replacement, symlink and
captured-layer checks. The guest powers off instead of rebooting.

Run from the fork root (12 build jobs maximum):

```
python3 examples/treefs-nitro/prepare.py /path/to/holodeck-treefs
CARGO_BUILD_JOBS=12 CARGO_TARGET_DIR=$PWD/target cargo build --release --manifest-path examples/treefs-nitro/Cargo.toml -j12
gcc -static -O2 examples/treefs-nitro/guest-init.c -o treefs-init
python3 src/libkrun/tests/embed-initramfs.py treefs-init treefs.cpio
timeout 65 sudo target/release/treefs-nitro-acceptance Image treefs.cpio none
```

The preparation script snapshots committed sources into `target/treefs-source`
and changes only that snapshot's relative devices dependency to this fork. It
never edits the treefs worktree or the shared integration fork. It prints the
exact source commit. The standalone package shares this worktree's build output.

For explicit read-only DAX, compile the guest with `-DDAX=1` and invoke the host
with `dax` instead of `none`. The read-only mount requests `dax=always`; writable
operations remain ordinary FUSE writes. Uses CID 215 and one 1 GiB guest at a time,
plus a 64 MiB DAX window when enabled. Never run on m9g without the required outer
host lock and host-specific lifecycle serialization.

Observed on m8g: `none` passes all checks. At treefs `c70edfb`, `dax` fails with
EINVAL on the first read: Linux sends setupmapping with `fh=u64::MAX`, while the
read-only adapter rejects handles not equal to inode. The requested length is
2 MiB (the correct chunk size), flags are read=2, offset=0. This requires a treefs
adapter decision; the VMM must not fabricate a filesystem handle. Do not count
that mode as passing. Kernel console is retained as `treefs-console-MODE.log`.
