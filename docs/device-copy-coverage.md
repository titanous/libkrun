# Device-copy fixture coverage

`tests/cooperative/devices.c` uses userfaultfd only in `block_random_read`
and `unblock_random_read`. It registers a missing anonymous page, starts a
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
Production coverage must explicitly omit this check, rather than claim a
generic blocked operation is an equivalent substitute. The test-kernel
variant retains the original stalled-read check.

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
