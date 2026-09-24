# Owned path-driver lifecycle — 2026-09-25

Scope: the terminal-task part of audit T05 / W2.5. Base `a89e7dc`. Synthetic
Linux loopback fixtures. No remediation wave closes and iroh remains the default.

## Reproduction and correction

Two real-QUIC regressions failed before the fix. The path-policy future did not
finish when a connection closed while handles remained alive, or when its last
I/O handle dropped. The interval branch stayed enabled after both event streams
ended, and a failed weak upgrade only made path selection return, not the loop.

The driver now registers noq's weak `on_closed` notification and exits when it
fires. No strong connection is held across an await. Ending both event sources
also ends the loop. A live stream continues to own the connection after the
application's Connection wrapper drops; the driver survives for that valid I/O
lifetime and exits when the last I/O handle disappears.

The owned endpoint tracks policy tasks and waits for their cleanup during
`close()`. A shared admission mutex serializes spawning with shutdown: closing
the task tracker alone would still allow a late handshake to add another task.
Closed endpoints refuse that admission and close the connection. Concurrent
close calls share the same barrier. Finished tasks release tracker storage
immediately instead of accumulating completed join handles.

QNT pending-success history is removed on establishment/abandonment and pruned
against live paths, including after lagged events. Selection drops closed weak
path handles by checking path status: an upgrade alone can still succeed while
the weak handle retains final statistics. This does not qualify the whole T05
candidate/validated-path policy or address-churn load behavior.

`tokio-util` 0.7.19, already present in Cargo.lock, becomes a direct optional
dependency of the owned backend for `TaskTracker`. This avoids a separate
supervisor queue or retained result collection for tasks with no result value.
No package/version, wire format, unsafe code or external runtime program was
added. Task tracking here governs policy tasks, not every task in the product.

## Coverage

Five tests cover both original failures, stream-only ownership, 32 sequential
connection cycles (local close, remote close and final-handle drop), and closing
eight live connection pairs with retained handles. Each cycle requires both
endpoints' policy-task count to return to zero. Concurrent endpoint close must
return with zero tasks and a later connect must fail. Existing transport,
cross-backend and owned-relay fixtures remain enabled in the final matrix.

Formatting and default/X11/all-feature Clippy passed with warnings denied.
The workspace passed **301 tests across 54 targets** (one previously
qualified capacity test ignored); the all-feature network/agent/CLI/relay run
passed **86 tests across 25 targets**.
[Machine-readable evidence](rds-driver-lifecycle-20260925-data.json) records
commands, timings, counts and source hashes. Full logs remain in private local evidence storage.
`cargo-deny` is not installed; no unregistered checkpoint was invoked.

## Remaining scope

- Agent/service task groups, admission budgets, disk-worker cancellation and
  bounded relay queues remain W2.5 work. Policy-task completion is not full
  endpoint resource or graceful QUIC packet-draining qualification.
- Relay tunnel pumps and server task ownership still have separate open work.
  R08 is the relay Drain framing reproduction, not this driver defect; R08 and
  R09 candidate fallback remain open.
- Validated-path selection, withdrawal handling, real NAT/failover, latency
  percentiles, RSS/FD load bounds and native macOS remain unqualified here.
- No deployed process, live identity or service configuration was changed.
