# Desktop frame sender contract

Scope: partial writes, reference recovery and serving-session cancellation in
W6.1/W2.5/W2.6. This does not close real-network codec or presentation gates.

Each encoded frame has one tagged unidirectional stream. An in-flight keyframe
finishes before a newer frame is selected. A delta also finishes when the next
candidate is another delta, because the successor may depend on it. An
independent keyframe may replace an in-flight delta with RESET. When the producer
closes during a send, its final frame finishes.

The producer queue remains two slots. Losing any encoded frame to admission
backpressure requests an IDR. Queue collapse selects a recent keyframe when
available and requests recovery for discarded references after that keyframe;
each drain is bounded to two items. The sender admits deltas only after a
keyframe and only at the exact next sequence. Gaps, empty encode results and
sequence exhaustion invalidate the chain. Dependent deltas wait for an IDR,
without first being sent as undecodable work or requiring a client roundtrip.
Selection is checked again after pacing, when additional frames may be dropped.

The payload writer retains one pinned `write_all` future across producer
events. `write_all` may already have accepted a prefix when its other select
branch wins; restarting it from the beginning would duplicate that prefix.
The sender now continues the same operation. A write or FIN failure is a failed
send, including after supersession or producer closure; it cannot report a
successful completed frame.

One 30-second deadline includes obtaining stream credit, writing the tag and
header, and completing the payload. The stream guard sends RESET on error,
timeout, stale abandonment or cancellation of the frame-writing future. Only a
successful FIN disarms the guard. RESET affects that frame, preserving the
shared connection. The deadline is a local failure bound, not a frame-latency
target or a negotiated wire capability.

The serving future owns its writer, pacing and capture task groups. Dropping
that future aborts asynchronous children and queued capture work. Normal exit
joins asynchronous siblings. A native capture call already running can finish;
it then observes the closed bounded frame queue and releases its source. The
control send half resets on exit/cancellation, and replies have a 30-second
write deadline. No detached worker retains a connection after async teardown.

Regression coverage uses a 64-byte duplex buffer and a 4096-byte patterned
payload to force a partial write before each producer event. Both supersession
and closure previously delivered 4160 bytes; both now preserve exact bytes.
Other tests retain errors after the producer event, preserve a partial delta
before its successor, replace a delta with an independent keyframe without
waiting for a reader, and observe RESET after aborting an owned sender on real
iroh/noq connections while a subsequent stream still succeeds.

A separate real-session regression observes the producer being dropped after
server cancellation with the client connection still alive; this failed before
task-group ownership was added and passes on both backends.

Native H.264 coverage checks decode and a pixel across a dropped reference and
IDR recovery. This is separate from the synthetic QUIC impairment measurements;
it does not establish real-codec continuity under network loss/reordering.

Each frame stream leads with a route tag so the viewer's router lands it in
the right inbox. `DesktopV2` sessions carry the negotiated
`UniHello::DesktopFrames { id }` — the session ID minted by the viewer and
echoed in `StreamHello::DesktopV2` — while legacy `Desktop` sessions keep the
shared route. A delayed stream left over from an ended session is tagged with
that session's route and can never reach a replacement session's inbox; it is
dropped unrouted. Distinct session IDs also mean concurrent desktop sessions
can share one connection, each claiming its own route and re-checked against
the grant's display scope.

Remaining work includes real-codec network continuity and overload/IDR-rate
qualification, global encoded/decoded/native allocation accounting, managed viewers,
rendering/input release and platform/network acceptance. See the
[client receive contract](desktop-client-lifecycle.md). No frame
priority, dependency, installed binary or published release changes here.
