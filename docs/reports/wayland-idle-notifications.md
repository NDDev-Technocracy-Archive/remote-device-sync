# Wayland native failure and stop notifications

Execution: [resource quality #210](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/210).
This increment removes two steady-state polling deadlines from the prepared
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

Failure notification is not a drain acknowledgement. The coordinator retains
the state/permission owner and joins the actual native workers before releasing
its state lock. Explicit stop and portal revocation retain their existing
classification and priority over native failure. Callback failure still closes
the prepared session; it does not probe another backend or request consent.

The Linux portal cases exercise an actual PipeWire loop with the same registered
I/O source: an idle interval has no dispatched iterations, a local stop wakes
it, and an EOF queued before the loop starts also wakes it. Existing worker
unwind cases additionally check failure publication and invalidated retained
pixels/readiness. Native CI executes these cases alongside the existing EI,
clipboard, portal-serving and drain regressions before this increment merges.
Build-free source checks do not establish those executable outcomes.

The EIS actor's own 10 ms protocol/request poll and bounded startup readiness
checks remain separate work. Physical consent restoration, native input effects,
clipboard publication, WAN recovery and sustained CPU/power measurements remain
open; this report does not close their product gates.
