# Requested-body refusal

Native clipboard metadata can refuse an already requested body. The body owner
now records that signal before publishing the request, matches the complete
desktop/offer/request identity and retains the first refusal. Foreign desktop
identities fail validation; unknown and completed requests have no new effect.

The signal interrupts a not-yet-arrived body, accepted wire reads, selected-file
reader/completion and native destination reception. Repeated reads after an
error return the same typed terminal error rather than polling a completed
future again. Verified bytes expose the retained refusal signal to their native
publication owner. Reservations and original authority remain in the actual
native operations through cleanup; a refusal cannot manufacture body credit.
There is no new payload task, staging file, timer or unbounded queue.

## Qualification

Runtime snapshot SHA-256:
`071dd5d01e83902188b68a6feb40bbc79b047ff64538160db7699e1c882112b7`.
On physical Apple Silicon, denied all-target Clippy passed for the seven native
consumer packages, all 204 desktop library cases passed, and the selected
CLI/agent targets and all 19 packaging cases passed.

Three actual Iroh/Noq loopback cases cover early exact/sticky refusal and reuse
of the same connection, an accepted wire with repeated failed reads, and
interruption of a selected 64 MiB generated file reader and its completion.
The earlier ownership, queue-admission, stream-integrity and native-drain
regressions remain enabled.

Linux qualification of that snapshot passed denied Clippy and the private-bus
native case, then failed one existing descriptor fixture: 236 desktop cases
passed, one failed and ten native-display cases were outside that invocation.
That failure is retained. The fixture assumed that peer EPIPE immediately
followed local FD closure; a shared open description makes this inference
invalid, including during a concurrent process spawn.

The fixture now observes the exact local descriptor identity in its original
owner's destructor. It holds a duplicate to keep the inode alive and make
FD-number reuse distinguishable. Peer writes deliberately remain successful
through that duplicate. Socket cancellation, FIFO teardown and invalid-direction
admission still have to close the local FD before releasing their original
owner. This directly tests the required ordering without a delay or retry.

The corrected snapshot is
`d386851ceb0c9697e64ecbdcc087ce99c11d9042a052d9193e96b33723581001`.
On physical Ubuntu x64 it passed denied all-target desktop Clippy, the actual
private-bus native case, and all 237 desktop library cases; ten native display
cases were outside that invocation. A deliberately reversed FD/purpose
field-order counterexample, SHA-256
`1077973cd6e22ef86bfdb0eb9fcd7e974ae87a19694a2f9d546a3b9ae9e025a9`,
failed the same socket cleanup case at the direct descriptor identity assertion.
Only those two production field declarations were reversed in that private
counterexample. This proves that the corrected assertion detects an actual
cleanup-order violation while tolerating a shared open description.

## Acceptance boundary

These APIs prepare the negotiated native body owner. Native metadata dispatch,
the manager/viewer integration and actual text/image/file pastes across the OS
matrix remain unfinished. No new mode is advertised and no installed host,
consent, device identity or wire version changes in this increment.

Reference: [Tokio watch sender semantics](https://docs.rs/tokio/latest/tokio/sync/watch/struct.Sender.html#method.send_if_modified)
retain a value for future receivers even before the first waiter subscribes.
The [Linux fork contract](https://man7.org/linux/man-pages/man2/fork.2.html)
describes shared open file descriptions; a duplicate models that condition
deterministically without claiming that the failed run traced a particular fork.
