# Wayland native failure and stop notifications

Execution: [resource quality #210](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/210).
This increment removes three steady-state polling deadlines from the prepared
Wayland host. It does not qualify a whole-machine CPU or latency reduction.

The coordinator waits for the existing portal closure and explicit input/capture
failure notifications instead of scanning native workers every 20 ms. Capture
callbacks seal their affected slot and signal the original group. Native worker
exit, including Rust unwind outside a foreign callback, also signals failure.
No clipboard, frame or input payload is carried by these notifications.

The PipeWire worker registers an owned Unix socket in its existing native loop
and waits without the old 10 ms timeout. Local stop seals admission and shuts
down the paired socket's write direction once; EOF wakes the loop even when no
pixels change. The native receiver stays owned by the registered I/O source.
No extra process, worker thread, unbounded message queue or product unsafe block
is introduced. Limits on prepared workers and retained images stay unchanged.

The EI actor also waits for its native socket and a private nonblocking eventfd.
Request admission, caller timeout, lease closure and local stop notify it without
waiting for the old 10 ms request poll. Notifications coalesce and carry no input
bytes. Handshake, request and canceled-sync drain retain their original absolute
deadlines; interrupted waits recompute the remaining time instead of extending
the deadline. A failed notification seals its queued input and native admission.

After a sync drains, expired queue entries cannot strand fresh input behind a
consumed notification. Rejected entries drain in bounded batches, interleaving
native I/O; only actual remaining queue work requests an immediate iteration.
There is no idle retry deadline. Every admission rechecks native stop, device
generation, caller lifetime and the original request deadline.

Failure notification is not a drain acknowledgement. The coordinator retains
the state/permission owner and joins the actual native workers before releasing
its state lock. Explicit stop and portal revocation retain their existing
classification and priority over native failure. Callback failure still closes
the prepared session; it does not probe another backend or request consent.

The Linux portal cases exercise an actual PipeWire loop with the same registered
I/O source: an idle interval has no dispatched iterations, a local stop wakes
it, and an EOF queued before the loop starts also wakes it. Existing worker
unwind cases additionally check failure publication and invalidated retained
pixels/readiness. EI poll cases use actual Unix/event descriptors with a silent
peer to check local wake, early coalesced wake and an unextended idle deadline.
The real EI cancellation fixture additionally queues an expiring request and a
fresh request behind a held native sync, checking no replay and fresh progress
after that same sync drains. It runs sequentially in the existing fixture lane
to preserve the production bound on prepared native owners.

Native CI executes these cases alongside the existing EI,
clipboard, portal-serving and drain regressions before this increment merges.
Build-free source checks do not establish those executable outcomes.

The initial capture/coordinator cut `e50aa97939e9894ce28eed14241c60418af4f430`
passed [its Linux portal job](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/actions/runs/42603891516/job/129114762068):
32 desktop portal cases, including the real native loop, and 36 agent cases.
That is partial evidence for the earlier cut; combined EI/native qualification
and the remaining exact-head gates are recorded on the pull request.

Bounded startup readiness checks remain separate work. Physical consent restoration, native input effects,
clipboard publication, WAN recovery and sustained CPU/power measurements remain
open; this report does not close their product gates.
