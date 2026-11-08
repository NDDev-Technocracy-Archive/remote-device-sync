# Path observation and metrics

Scope: W0 measurement correctness, W3.6 path policy, W10.1 observability.
This contract does not close those tasks or promote the owned backend.

## Sources of truth

`Connection::path_stats_snapshot()` returns normalized path counters plus
`PathStatsCoverage`. `path_stats()` is a convenience projection of its paths.

- `BackendSnapshot` uses iroh's own path manager, including its selected and
  relay flags.
- `PolicyObserved` uses Noq paths whose validation the RDS policy has observed:
  the completed TLS handshake's initial path, then consumed `Established`
  events. Pending probes and advertised candidate addresses are not validation
  evidence. Zero lost events does not assert that all engine paths are known:
  events can be queued or precede subscription. The Noq 1.4.0 wrapper exposes
  individual paths and bounded event streams, not a complete live validated-path
  snapshot.

Noq policy and the connection facade share weak path metadata. Subscription and
handshake seeding occur before returning the connection. Additional paths are
published after selection. There is no numeric ID scan or fixed ID ceiling;
retired paths are pruned, and metadata follows the concurrent observed path set.
Weak path handles can retain final counters after closure, so reads also check
path status. Closed connections and stopped observers expose no live paths.

Neither metadata nor the observer guard owns connection I/O. Canceling the
observer, including before its first poll, clears its view and marks it stopped.
The endpoint continues to own and join the policy task as before.

## Selection and event loss

Noq `selected` means the last successfully applied policy preference, still
`Available` when observed. More than one observed Available path suppresses the
selection flag. These separately read path properties are not an atomic engine
snapshot or proof of which path carried an individual packet. Relay flags come
from each path's actual synthetic relay address, not its ID.

`PolicyObserved.lost_events` accumulates the engine's broadcast lag count. It is
sticky for the connection; receiving later events or reselecting known paths
cannot repair an unknown inventory. Any loss suppresses the selected-path
estimate. `driver_running` means the policy observer has not stopped and the
connection has no known close reason. It does not supervise the engine's I/O
task or prove reachability/responsiveness. A complete resynchronization API
remains required.

`current_path_stats()` returns None when selection is unknown. It never chooses
the historically busiest path as a substitute. Desktop pacing already accepts
None: it retains its current rate, subject to cadence-miss reduction. Path-epoch
reset and stronger adaptation policy remain W6.7.

## Sampled metrics

`ConnSampler` classifies observed cumulative deltas into direct/relay buckets.
It retains backend weak observation handles and metadata only: no connection
facade, uni-stream router or strong transport handle. `Registry::sampler` keeps
its existing by-value signature, but passing the last connection handle no
longer preserves I/O. Callers must retain their service's actual connection or
stream owners. Streams may outlive all facades and continue to be observed.

`run` registers a weak closure notification before waiting. Last-I/O drop and
explicit/peer close wake it independently of the sampling interval; canceled
or never-started sampler tasks release their gauges through normal drop.
Reads briefly upgrade weak handles synchronously, never across an await.
Closed iroh connections expose no live paths even while closed handles remain.

Its per-path baselines are retired with observed paths, bounding retained map
entries by concurrency instead of connection churn. Path IDs are never reused
by the pinned engine. Samples can miss short-lived paths and final increments
between the last sample and retirement/closure; this is not lossless byte
accounting. A final sample does not recover already retired paths.
These are the inner connection's QUIC UDP counters; relayed bytes do not
include the outer tunnel's additional encapsulation cost.

Existing RTT and congestion-window gauges describe the endpoint's last sample,
not a sum or a per-connection series. Their validity and values share one locked
observation; a registry snapshot cannot combine different samples' RTT/cwnd.
The sample also records a metadata-only owner token. Dropping that sampler
invalidates its selection/RTT/cwnd; dropping an older sampler cannot erase a
newer sampler's selected observation. No stale sample from another connection
is substituted. One-shot callers must keep the sampler through their scrape.
The rest of the registry contains independently read counters, not an atomic
transport-wide transaction. Unknown selection produces zero RTT/cwnd and
`rds_net_selected_path_known = 0`. Export uses a try-lock: a busy or poisoned
selected-sample lock gives the same unknown result for that scrape, without
waiting for the sampler or clearing its stored observation. Other counters
remain available.

Additional exported values:

| Name | Type | Meaning |
|---|---|---|
| `rds_net_policy_observed_connections` | gauge | Sampled connections using the policy-observed view |
| `rds_net_degraded_path_observers` | gauge | Sampled policy views with event loss or a stopped observer |
| `rds_net_path_events_lost_total` | counter | Observed cumulative lag deltas, counted once by each sampler |
| `rds_net_selected_path_known` | gauge | Whether a known RTT/cwnd selection can be observed in this scrape |

One sampler per connection is required to avoid double accounting. Coverage
gauges follow sampler lifetime and return to zero on drop; lost-event totals
remain. `rds_net_live_paths` counts observed live paths and may undercount the
engine. Noq byte totals currently exclude unobserved probes and missed paths.
The CLI prints coverage before its path rows.
Manual `sample()` users still own their polling and drop schedule: the registry
does not spawn a watcher for them. `rds_net_active_connections` counts live
sampler objects, not independently discovered open connections. Between samples
their gauges describe the last observation until another sample or owner drop.

## Validation and remaining work

Regression fixtures cover actual relay-only authenticated streams/datagrams,
migration away from path zero, pending-probe exclusion, 70 sequential path opens
past ID 63, actual event-buffer overflow with deliberately delayed consumption,
sticky loss, unknown selection, observer cancellation, and post-close emptiness.
Existing drop/stream ownership and failed-engine shutdown cases remain required.
The [sampler lifecycle receipt](reports/rds-sampler-lifecycle-20260925.md)
covers both backends, last-handle drop, stream survival, closure with a one-hour
sampling interval, cancellation, late start and shared selected-gauge ownership.

Full validated-path reconciliation, lossless retirement accounting, per-session
metrics, native macOS and real network qualification remain open. Aggregate
endpoint counters now reach the authenticated [admin surface](observability.md);
this does not resolve their sampling limits. No latency or throughput improvement is
claimed from these correctness tests.

Historical reports are preserved. Noq path-kind and selected-path evidence
collected before this correction must be rerun for qualification; old values
cannot be repaired by changing their labels after collection.
