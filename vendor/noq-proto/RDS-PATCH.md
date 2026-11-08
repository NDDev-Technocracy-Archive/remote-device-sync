# Noq-proto 1.4.0 path observation and optional ACK routing

`Connection::established_paths` now exposes a read-only snapshot of path IDs
whose initial establishment is informed and which have not been abandoned.
It includes the handshake path while the connection is open. The Noq adapter
forwards this for Iroh's subscribe-then-snapshot registration; no validation or
packet scheduling decision changes. A pending/abandoned/high-ID/closed-connection
regression covers the snapshot boundary.

Exact published crates.io noq-proto 1.4.0 source, checksum
c01a35be3ed37558c8db69554419f24eff8a63599b54f711bbe3863ec727c47d.
Published upstream revision: 34f75c49f93de5e2ff943483cabc5b6d0c5df918,
subdirectory noq-proto. Original MIT/Apache-2.0 license files are retained.
One additive PathStats bool
reports whether the path packet-number space retains unacknowledged STREAM
metadata. It reads existing sent-packet bookkeeping, exposes no application
bytes. The observation changes no wire, crypto, loss, pacing or retransmission algorithm.
RDS uses it to distinguish reliable stream work from PING-only debt before
path retirement.

TransportConfig::prefer_same_path_acks is a separate default-false opt-in.
When multipath is negotiated, ACKs for a validated, live path prefer that
path, including when it is Backup. ACKs for abandoned/unvalidated paths
can still travel on another path; handshake and single-path scheduling are
unchanged. RDS enables the option only for PathPreference::Latency, through
both Noq and the narrow Iroh builder forwarding method. No new wire frame,
crypto, loss detector or congestion controller is introduced. This avoids
making backup ACK delivery depend on the failed selected path.

Policy guidance: https://www.ietf.org/archive/id/draft-ietf-quic-multipath-21.html#section-5.5
permits same-path ACK scheduling; this is an implementation choice, not an
IETF-mandated default. Tests in rds-net's real Noq loss-socket fixtures verify
the ACK carrier, probe/STREAM distinction and clearing after ACK. The real
Iroh fixture restores a retired standby, transfers and drains IP work, then
recovers existing stream bytes after a later custom-link blackhole.
Upstream contribution/convergence and installed qualification remain work.

The MTU constructor's debug assertion keeps its numeric invariant but uses a
static message. CodeQL treated the formatted minimum-MTU value as configuration
data derived from test certificate setup. Removing the interpolation avoids
that diagnostic data flow without changing the MTU decision or disabling scans.

## Initial multipath validation deadline

A new unvalidated path now has a dedicated deadline independent of its idle
policy. The budget is three times the larger initial PTO and PTO of the live
validated paths. This conservative choice follows RFC 9000 §8.2.4 guidance;
using all live validated estimates avoids judging an unknown route solely from
an unrelated faster path. RFC 9000 migration retains its separate timer and
previous-route fallback. Validation success cancels the initial timer. Failure
closes the attempt with the existing TimedOut reason and PATH_ABANDON machinery;
last-path protection, CID retirement, ID nonreuse and retransmission remain in
the existing engine boundary. Qlog records the standard path-validation timer.

No new wire frame, credential, congestion controller or application replay is
introduced. Multipath draft-21 §3.1 requires explicitly closing a failed path
initiation; §3.4 prohibits reusing an abandoned path ID. Deterministic engine
regressions cover no-idle-policy timeout, fresh-ID recovery and last-path
protection. The complete engine unit suite runs on both supported CI targets.
A real Iroh actor fixture blocks an initially configured standby, restores it
and continues the original bidirectional stream through the same connection.

Sources: https://www.rfc-editor.org/rfc/rfc9000.html#section-8.2.4
and https://www.ietf.org/archive/id/draft-ietf-quic-multipath-21.html#section-3.1

## BBRv3 startup pacing calibration

The nominal startup pacing rate assumes a 1ms RTT. It now recalibrates once
from the first acknowledged tracked packet's measured RTT, with a 1us zero
floor. The first BBR rate sample uses packet departure/ACK time rather than
Noq's transport estimator, which is updated after congestion callbacks.
Subsequent bandwidth-driven STARTUP growth, clone state and send-quantum updates
retain their existing owners. Both RDS backends use this same engine.

This narrow adaptation follows [Google BBRv3's measured-RTT initialization](https://github.com/google/bbr/blob/v3/net/ipv4/tcp_bbr.c)
and the reviewed [moq-dev/noq PR7](https://github.com/moq-dev/noq/pull/7), merged
2026-09-25; it does not import that fork's broader feedback changes.
[Noq PR802](https://github.com/n0-computer/noq/pull/802) records the original
application-limited pacing defect. Deterministic tests fail on the published
engine and cover measured vs initial RTT, one-time calibration, clone/growth,
sub-millisecond and zero RTT. This corrects an independently reproduced defect;
it does not establish the cause or closure of the retained 521ms CI control tail.
