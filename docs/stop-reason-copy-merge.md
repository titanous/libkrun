# Combining stop reasons with Nitro memory copying

This is merge guidance, not a claim that the Nitro memory-copy branch has been
merged or tested with these changes. The first reviewed overlap was
`nitro-clone-nitro` at `e34ba088`, based on `b194fdf4`; that branch later also
changed `src/vmm/src/capture_request.rs`, so ten files are shared.

Preserve the following rules in the ten shared files (paths are repository
relative):

| File | Required combination |
|---|---|
| `src/libkrun/src/lib.rs` | Keep the shared `VmExit` and terminal-facts resolver, typed errors, console drop counters and raw console observer. Do not restore the backend-local exit enum or `panic_seen`. A frozen source is successful but distinct from poweroff. |
| `src/nitro_sandbox/src/console.rs` | Run copy-control discovery and the embedder's raw console observer before diagnostic budgets and sink writes. Keep bounded parser state. Do not restore fork-level panic classification. |
| `src/nitro_sandbox/src/host.rs` | Retain typed host/guest errors, descriptor handoff and one-operation lifecycle locking. Source-close failures must remain errors, not successful frozen outcomes. |
| `src/nitro_sandbox/src/lib.rs` | Carry frozen state through terminal facts rather than reviving a separate public exit enum. Retain raw reason-byte validation and observer configuration. |
| `src/nitro_sandbox/src/runtime.rs` | Integrate parking and capture polling without reason-alone termination. Apply the capture guard below and recheck after stopping the source. Suspend configured liveness while intentionally parked and resume with a fresh deadline. |
| `src/nitro_sandbox/src/transport.rs` | Preserve raw reason bytes: 0 clears, 1--4 state intent, 5 and invalid bytes do not clear retained intent. Keep capture transport failures distinct from guest ends. |
| `src/vmm/src/builder.rs` | Keep console counter snapshots and observer installation. Do not restore obsolete console-ending flag registration. Preserve copy setup and inherited stop-channel configuration. |
| `src/vmm/src/lib.rs` | Preserve checked CPU joins with one shared deadline, sticky join failure, late CPU-error aggregation and post-join reason validation before template publication. Never clear unchecked CPU handles and then claim successful joins. |
| `src/vmm/src/capture_request.rs` | Keep both: the copying branch's restored-copy refusal and `timed_out` state (a late result after the waiter timed out is dropped by `complete()`), and this branch's `validate_stopped_source`. Order: a capture that timed out never publishes later, even if its source then validates; a reason rejection (`GuestStoppedDuringCapture`) always reaches the caller that is still waiting, and is dropped like any late result only once that caller has timed out. |
| `src/vmm/src/linux/vstate.rs` | Preserve checked CPU joins and sticky join failure while combining CPU parking/copy state. A detached or failed CPU must not be treated as successfully stopped or permit console ring draining. |

## Capture acceptance guard

Nitro `poll_capture` must reject a capture before accepting it when a final
reason 1--4 has already arrived. Preparing (5) alone is not a terminal reason.
Recheck the retained reason after stopping/closing the source and before
publishing a template; a reason arriving during acceptance must also reject
publication. Propagate close/join errors before publication. Set
`TerminalFacts::frozen` only after the source has actually closed successfully.
The shared resolver makes a final reason outrank frozen state, but that resolver
alone cannot undo an already-published template.

The reason byte is only set on Nitro when the guest kernel has the reason-device
driver and holodeck enables `Builder::nitro_stop_reason`. Until both are in the
base, the byte stays 0 and this guard alone checks nothing on Nitro: the guard
must then also reject (before accepting, and again before publishing) when the
run's host stop event is pending, which is how holodeck's console stop watch
ends a Nitro guest today.

## Stop-channel inheritance for copies

A copy has the same stop-channel configuration as its template. On KVM the copy
resumes the template's kernel, which keeps `holodeck.stop_port=1`, and the VMM
always registers port 0xea, so nothing extra is needed. On Nitro, a copy's VMM
must enable the reason device exactly when its template's VMM did (holodeck
passes the same `nitro_stop_reason` setting from the template's spec), and the
reason region of the copy starts at 0: a template's retained reason is never
inherited, because a template that stated a reason is never published (guard
above). Test it: a copy of a template must report a typed reason for each ending
kind.

## Console and daemon policy

Holodeck currently stops Nitro guests on complete kernel-prefixed stop markers,
including panic, through the pre-budget raw observer. Preserve that compatibility
policy until privileged reason transport and configured liveness are enabled
and verified together. Copy-control parsing must not depend on the diagnostic
sink retaining a marker. On KVM, attach lossless control parsing before
`BufferedOutput::write_volatile` enqueues diagnostic data; retain the bounded
per-guest-write back-pressure budget and stop wakeup.

## Required combined verification

Run capture-before-reason and reason-during-capture tests, close/join failure
injection, copy inheritance, parked-liveness suspension/resumption, all terminal
kinds, and console budget-loss cases. Verify no template is published on any
rejected capture. Run real Nitro copy and ending cases with exact owned slot and
memory baselines restored. Unit tests alone do not establish combined hardware
behavior.
