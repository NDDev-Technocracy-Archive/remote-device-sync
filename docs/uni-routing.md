# Tagged uni-stream routing

The current v3 protocol routes inbound uni streams by `UniHello`. The additive
`SyncTransfer { id }` variant isolates managed file transfers by random ID, and
`SyncTransferV2 { id }` routes chunk streams for the negotiated v2 session —
the same ID is then repeated inside every `Session` frame, so a stale stream
can reach an inbox but never satisfy a different transfer's frames.
`UniHello::DesktopFrames { id }` does the same for desktop: the viewer mints a
random session ID, echoes it in `StreamHello::DesktopV2`, and claims the
matching route, so a delayed frame stream from an ended session can never
satisfy a replacement session's inbox. `Connection::uni_streams` permits one
live inbox per kind, which also lets concurrent desktop sessions share one
connection without colliding. Services order their own payloads by frame
sequence or chunk identity: parallel tag reads need not complete in the order
streams were accepted. Legacy `Sync` and shared `Desktop`
service-kind routing remain available only for explicit direct compatibility.

## Ownership and termination

Facade Connection handles and their UniStreams inboxes share the router owner.
An inbox can outlive all facade handles and still receive streams. The driver
holds a backend connection plus a weak reference to the router owner; it never
owns a facade Connection. Route workers also use weak owner references and do
not hold a strong owner while awaiting a tag or queue space.

Dropping the last facade handle and inbox requests driver cancellation. The
driver's task group cancels remaining workers when dropped, releasing its
backend handle. Raw send/receive streams held by callers can still legitimately
keep QUIC alive. Drop does not perform a synchronous join; the runtime performs
the canceled futures' cleanup.

On explicit or remote QUIC closure, the driver stops accepting, cancels and
joins its workers, then clears registered inbox producers. Thus a full inbox
cannot retain a blocked route worker indefinitely. Already-buffered streams can
be drained after closure; this bounded queue is not silently flushed. A new
claim on an already-closed connection returns an ended inbox immediately.

Dropping an inbox removes its closed route, allowing its kind to be reclaimed.
At most 32 live routes are registered; historical transfer IDs do not accumulate. Cleanup from an old sender
removes a route only if the map still holds that sender's channel. This prevents
old cleanup from deleting a replacement inbox, but does not provide session-ID
isolation for late streams whose tags have not yet been read.

## Local budgets

| Resource | Bound / behavior |
|---|---|
| Tag-read and queue-handoff tasks together | 64 per connection |
| Streams buffered in each kind's inbox | 128 |
| Initial tag read | 10 seconds; existing bounded frame decoder |
| Additional streams during saturation | Acceptance pauses; QUIC stream/flow credit supplies further backpressure |
| Connection closure while saturated | Closure remains polled; pending workers are canceled and joined |

`Connection::uni_routing_stats()` reports the pending worker count and local
limits. Counts include tasks parked on a full inbox. It does not measure raw
QUIC buffers, disk work, media decoding, other service tasks or process RSS/FDs.
These local implementation limits are not advertised as negotiated capabilities.

Sync retains queue backpressure instead of silently discarding chunk streams.
A saturated kind can occupy the pending-task budget; per-service reservations,
negotiated limits and desktop stale-frame policy remain W2/W7 work. This change
does not claim quality-of-service isolation under saturation.

The [2026-09-25 receipt](reports/rds-uni-routing-20260925.md) records real-loopback
ownership, stalled-tag, queue saturation and cleanup checks on both backends.
