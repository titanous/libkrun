# Device-copy fixture coverage

## Reproduction

Build the fixture with `tests/cooperative/image.nix` and the release
`cooperative-console` example as described in `cooperative-clone.md`.
The regular matrix uses `tests/cooperative/run.py --cycles 20 --concurrent 16`.
For 50 copies of one template, use the same required `--kernel`,
`--initramfs`, `--control 0`, and fresh `--output` arguments, adding
`--single-template-copies 50 --concurrent 10 --passed-fd`. This mode hashes
the template bytes the copies actually map before and after all copies (the
sealed memfd with `--passed-fd`, otherwise the exported file) and requires
unchanged SHA-256. It requires each copy's random, pending-reader and
generation values to differ from all other copies, runs the default-closed
policy child, and checks that the host TCP listener was never reached.
Results and hashes are in `measurements.json`. Concurrency is capped at 16 in
this mode. Copies are compared only with each other: a source sample taken
before the freeze differs from any later output even with no refresh, so it
proves nothing.

On the test-kernel variant, add `--require-blocked-random`: the host then
fails the run unless every copy executed the userfaultfd-held random read
(a declared skip is an error). Without it the host accepts the skip that the
production kernel needs.

### Random bytes of running threads

`tests/cooperative/random.c` with `tests/cooperative/random-run.py` checks the
running-thread case on the production kernel, with no debugger and no
userfaultfd. Build the image with `--argstr workload random`, then run
`random-run.py --kernel ... --initramfs ... --output ... --freezes 70`
(70 source freezes on each of 1, 4 and 8 CPUs, 4 copies each). Sixteen threads
loop on the getrandom system call, the vDSO getrandom, `/dev/urandom` and
`/dev/random` with 8, 32, 4096 and 1 MiB requests while the source freezes.
The host reads every thread's state from template RAM: whether it was inside
a call, whether the kernel had already written bytes for that call, and
whether user space had stored the result. A system call's result is stored by
the instruction right after the call, so "returned after resume" is decidable.
For every thread the first value returned after resume must differ across
copies, the first three values of all copies must all differ, and no copy may
return bytes generated before the freeze. The vDSO counts as returned before
the freeze only when its state shows the final generation check passed.

## Coverage

`tests/cooperative/devices.c` uses userfaultfd only in `detect_random_fault`
(which creates the descriptor, or declares the skip on ENOSYS),
`block_random_read` and `unblock_random_read`. It registers a missing anonymous page, starts a
thread reading 32 bytes from `/dev/random` into that page, and requires a
userfaultfd page-fault event before freezing. After each copy resumes it
resolves the fault, joins the thread, and prints `CLONE BLOCKED_RANDOM`.
The host matrix requires these outputs to differ between copies. This tests
random output already generated inside a read that is stalled in
copy-to-user across the freeze, not merely a thread waiting for input.
The test kernel's patch 0105 supports freezing this deliberately stalled
operation; production intentionally omits both that patch and USERFAULTFD.

A pipe/socket read or futex wait does not hold generated random bytes in
copy-to-user, so it cannot establish this particular freshness property.
Production coverage explicitly omits this check, rather than claiming a
generic blocked operation is an equivalent substitute. The test-kernel
variant retains the original stalled-read check. The fixture prints a
`CLONE CHECKS` declaration before freeze; the host requires it and requires
either the blocked-read result or the matching explicit skip. With
`--require-blocked-random` (test variant) the skip itself fails the run, so a
test kernel without the feature, or the production kernel passed by mistake,
cannot pass. Missing or failed independent checks remain fatal.

## Stalled-read check and production applicability

A bounded source investigation considered delaying a private file-backed
mapping through a userspace FUSE server. Linux 6.18's
`fs/fuse/dev.c:request_wait_answer` uses interruptible, killable, then
uninterruptible waits, not a freezer-aware wait. Delaying a block-device read
likewise does not establish a freezer-aware copy-to-user stop. No working
production alternative was established; neither is counted as coverage.

`0105-freeze-userfault-copy-waits.patch` adds `<linux/freezer.h>` and calls
`try_to_freeze()` immediately after `schedule()` in the missing-page wait in
`fs/userfaultfd.c`, before restoring `TASK_RUNNING`. It lets the deliberately
stalled reader enter the freezer with its wait entry still valid. It changes
no random generation, pending-result tracking, or refresh implementation.

The published x86-64 production image has SHA-256
`8ed97f7240e80a987c148f4cff2b900937640058787235f2f9b91feaaee3f51e`.
Its matching test variant is
`/nix/store/sgrxmzz4f08j0ny2cicwjzyrs5mfd86v-holodeck-guest-kernel-x86_64-6.18.62`.
Their manifests identify the same base source
`f58942188089e4d82b60f9d9a6d6700353985852` and the same production patch
list, with only 0105 added for the test variant. This includes the same
0102 pending-random refresh, 0108 template gating, 0109 post-copy random-work
disable, and 0110 boot confirmation patches. A complete configuration diff
contains only `CONFIG_USERFAULTFD` and its dependent
`CONFIG_HAVE_ARCH_USERFAULTFD_WP`, `CONFIG_HAVE_ARCH_USERFAULTFD_MINOR`, and
`CONFIG_PTE_MARKER_UFFD_WP` entries. In particular `CONFIG_X86_64`,
`CONFIG_JUMP_LABEL`, `CONFIG_FREEZER`, `CONFIG_HIBERNATION`, and
`CONFIG_VDSO_GETRANDOM` are identical. Thus the random-refresh implementation
and its relevant configuration are shared; carrying the test-variant
stalled-read result over to production is a source/configuration inference,
not a production runtime result.

Residual risk: a production-only interaction affecting random output held
inside a blocked copy-to-user across freeze remains untested on the exact
production image.

The independent ptrace child stops a successful `getrandom` at syscall exit
before userspace receives it. After resume the parent detaches it, checks its
32-byte result and the pollable freeze notification (`path=3 seq=1`); the
host requires distinct `CLONE PENDING_RANDOM` results. This check does not
use userfaultfd and remains required on production.

All other device-fixture checks are independent of userfaultfd: trigger
permissions and CAP_SYS_ADMIN, vmgenid driver and interrupt, ext4 writes
and record integrity under freeze/resume, SIMD state, TSC and monotonic and
wall clocks, dead inherited vsock and working fresh connection, forwarding
policy, hwrng read and fresh getrandom, CPU count, and host e2fsck. The host
also checks generation identifiers, random uniqueness, private-memory and
resume timing observations, and default-closed policy. None may be disabled
because userfaultfd is unavailable.
