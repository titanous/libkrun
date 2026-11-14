# Combining stop reasons with Nitro memory copying

This is merge guidance, not a claim that the Nitro memory-copy branch has been
merged or tested with these changes. The reviewed overlap was
`nitro-clone-nitro` at `e34ba088`, based on `b194fdf4`.

Preserve the following rules in the nine shared files (paths are repository
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
