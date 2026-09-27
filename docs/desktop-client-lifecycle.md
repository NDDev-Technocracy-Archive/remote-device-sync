# Desktop client receive and cancellation contract

Scope: part of W6.2 and W2.5/W2.6. The graphical renderer and desktop manager
API remain unimplemented; this contract describes the existing Rust viewing
session, including its headless header tap.

`DesktopSession` claims its uni-stream inbox before opening a control stream.
The default `connect` mints a random session ID, claims
`UniHello::DesktopFrames { id }` and greets with `StreamHello::DesktopV2`;
`connect_opts` with `session: None` keeps the legacy shared `Desktop` route for
older peers. A claim on an already-live route fails locally without starting
another remote session, while sessions with distinct IDs share the connection.
One 30-second deadline covers stream credit, hello write and ACK. Cancellation
resets the send half and stops the receive half, releasing the inbox.

One supervisor owns the control writer, event reader and frame receiver.
Dropping the session aborts that task group. Control EOF/error ends sibling
work even if the session handle or transport connection remains alive. The
frame receiver owns its reader group; incomplete readers cannot outlive it.
Each control write has a 30-second deadline. Queued control calls report an
error after the writer ends. The shared connection remains usable by other
services.

Receive budgets are local implementation limits, not negotiated capabilities:

| Resource | Limit / behavior |
|---|---|
| Encoded readers, completed bodies and active decode input | Four per session, eight per process; reject excess streams immediately |
| Encoded payload | 32 MiB per admitted frame; at most 256 MiB payload across those slots |
| Header plus body completion | One 30-second deadline per admitted stream |
| Native decode calls | Two per process, on Tokio blocking workers |
| Decoded output queue | Four frames, newest replaces oldest |
| Header/event/control queues | 64 / 128 / 64 records |
| Software BGRA output | Positive dimensions, at most 8192 per side and 7680×4320 pixels |

`receive_stats()` exposes aggregate receive counts and limits without peer or
display identifiers. These are not global RSS limits: transport buffers, codec
reference memory, allocator capacity and decoded images are separate. Global
decoded-image accounting and native codec allocation qualification remain open.
The BGRA size check occurs before the Rust output allocation; it does not
prevent every allocation inside the native decoder.

A blocking native decode already running at cancellation may finish. Its
encoded and decoder permits remain held until it returns. It owns no connection,
control sender or presentation queue, so its result cannot publish after the
async caller has been canceled. Input payload ownership moves into the decoder
without an extra full-payload copy.

Zero/oversized dimensions and `u64::MAX` frame sequences are refused before
body reads. Input and heartbeat counters fail on exhaustion instead of wrapping.
Decoded dimensions must agree with the header. A sequence gap or decode failure
invalidates the reference chain; deltas wait for a keyframe, and the decoder is
recreated on that blocking path. All automatic IDR requests share a 500 ms
limit, including the first request at session time zero. The header tap still
reports complete, valid, non-stale frames independently of decoder success.

On the v2 route a delayed tag from an ended session binds the ended session's
ID — the router drops it instead of delivering it to a replacement session's
inbox; foreign-route streams arrive tagged `Desktop` or a different
`DesktopFrames { id }` and never reach the session's readers. The legacy
shared `UniHello::Desktop` route still has no per-session wire ID: a delayed
tag from an old session can collide with a later claim on the same
connection, so legacy reuse is not isolated viewer switching. Sender-chain
correctness, rendering/focus/input release, managed viewer integration and
native platform/network qualification retain their remediation gates.
