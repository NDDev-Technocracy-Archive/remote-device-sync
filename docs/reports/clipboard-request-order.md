# Clipboard request admission and metadata order

The negotiated body owner now owns one metadata queue for its entire desktop
scope. Native selected-file capabilities use this same owner for requests,
receipts and cancellation; they no longer accept a second independently chosen
queue. This gives the metadata session one authoritative request sequence.

Request admission reserves queue capacity before body credit or ID allocation.
One short synchronous critical section allocates the sequence, claims the exact
receive route and commits the request metadata. It contains no await or native
I/O. Concurrent callers cannot publish their IDs in the opposite order.
A saturated/closed queue refuses before request allocation. The original
per-request owner is retained before metadata publication; its cleanup precedes
release of body capacity. File reads and destination writes keep that same
owner through their existing streaming and native-I/O contracts.

Snapshot SHA-256:
`540ce5fc44f5961dd7c49e187bd418dbc6e8e4e7d838693f46cb5f7d61a998d0`.
Physical Linux x64 denied all-target Clippy and all 234 desktop library cases
passed, with ten dedicated-display cases excluded. Four concurrent producers
commit 256 requests in sequence on each actual Iroh/Noq loopback body owner.
A separate capacity/closure case proves that refusal consumes no ID or body
slot and that an admitted request's original cleanup still holds its credit.
The selected-file receipt, cancellation, forwarding and native-operation drain
cases remain passing through the stream-owned metadata path.

Physical Apple Silicon denied Clippy and all 201 desktop library cases passed.
The normal development probe build passed strict native signature verification
and its 17 private-pasteboard cases; no installed Host or permission changed.
The native source adapters are unchanged by this increment. Native mode
handshake, peer replay/state handling, manager/viewer routing and actual
cross-device application pastes remain separate acceptance work.
