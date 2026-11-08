# Iroh 1.4.1 periodic custom selection patch

Initial actor registration subscribes before reading Noq's established-path
snapshot. A deferred accepting future can otherwise miss an already validated
standby permanently because the event stream does not replay history. Identical
snapshot/event observations do not emit duplicate opens or increment metrics.
The original handshake-path relay restoration policy is retained.

Upstream: published crates.io `iroh` 1.4.1, checksum
e3689cab508af9bcef7816c1201b403595ec6aa20e7d15e1f9683bb4be4a5b17.
Original SPDX MIT OR Apache-2.0 license files from exact upstream revision d6c6963b9602d1b8dd2ba53b6519e7eb19cc7940
and the additional BSD-3-Clause Tailscale-derived source notice are retained. No QUIC/TLS/relay wire change.

The published selector only runs after connection/path topology events. Add one
optional default-None selector refresh interval, bounded250ms..60s. The RDS
latency selector requests1s; ordinary/pinned selectors preserve original events.
Unchanged selections are not reapplied during refresh, and missed refresh ticks
are skipped rather than replayed in a burst after suspension/stall. The actor already owns
only weak connection references; refresh does not add connection ownership.
No default behavior change for other selectors. This temporary source patch
keeps the working substrate while the owned Noq backend retains its existing
periodic lowest-RTT policy. Upstream qualification and convergence remain work.

The RDS opt-in latency policy additionally reads the already-public Noq
congestion-controller snapshot, probes an established path, and can retire a
failed path after activating an established sibling in the same connection.
These small `PathSelectionData` methods remain in the same patched source file.
Noq's last-open-path guard and reliable retransmission remain unchanged. No
default selector, controller, handshake, TLS, transport eligibility or wire format
is replaced; the RDS controller adapter delegates the actual congestion algorithm.


The opt-in selector can also restore known relay/custom standbys after
abandonment beside an existing IP path. This uses the same client-only
open/validation machinery, deduplicates pending four-tuples and ignores stale
closed handles. Defaults/pinned opening behavior is unchanged. The selection
loop's intended one-IP invariant is repaired by retaining a live IP explicitly
instead of testing an unchanged event-updated map count for every close.
No connection owner is retained across an await and no new addresses are made.

QuicTransportConfigBuilder additionally forwards the default-false
prefer_same_path_acks option from the vendored Noq-proto patch. RDS enables
it only for latency preference, so standby proof can return on its own path.
See ../noq-proto/RDS-PATCH.md for scheduler scope and provenance.

Builder's default-false keep_relays_connected option retains independently
registered connections for at most three initially configured origins. The
existing actor task group, retry backoff and shutdown own those connections.
An actor-lifetime lease withdraws readiness on return, abort or panic. Connected
registrations are advertised; failures and map removal withdraw them. Dynamic
additions cannot expand the initial budget. Home status and default address
publication retain their prior behavior. This is readiness rather than QUIC
validation or an administrative relay-priority policy; no wire/crypto change.

Known-standby restoration no longer requires a surviving IP path: an established
relay-only connection can restore its explicitly known relay alternatives. No
new route is inferred from local relay configuration. Endpoint address watchers
publish connected registrations while home status remains distinct; explicit
close synchronously seals readiness while retaining the engine's normal drain.

The explicit `Endpoint::forget_unvalidated_ip_addrs` operation removes only the
specified failed IP hints without a live authenticated path through the owning
remote actor. It processes ready connection closures before deriving protected
paths from live connections. Open/Inactive cache history alone cannot preserve
an address after DHCP reuse. Obsolete selected/retry candidates are also removed.
It acknowledges removal before another dial, preserves live authenticated paths,
other peers and relay/custom addresses, and starts no new actor. Without this,
a wrong-key application hint remains cached: the fallback Initial races it
again and its fast TLS refusal wins over a valid relay. Ordinary connecting is
unchanged; RDS invokes this only after its bounded preferred attempt fails.
No key check, TLS, admission or wire rule changes. The manager integration test
uses a real signed directory and isolated relay with dead, wrong-key and
previously authenticated/reassigned hints; the path-state regression covers
live-authenticated/unrelated cache preservation. Owned Noq's address-reuse
fixture awaits its asynchronous UDP-driver close before binding the replacement.

## Upstream refresh

The pending four-tuple queue deduplication is now upstream and is no longer a
local change. The other opt-in adaptations above are carried onto the published
source. Default features and transport eligibility are retained. Qualification
is recorded separately; a successful source merge is not behavior evidence.
