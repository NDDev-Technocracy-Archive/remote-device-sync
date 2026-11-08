# Noq 1.4.0 established-path snapshot

The published crates.io package is the baseline. Archive
SHA-256: `6249a065adc1edbc1704a95436f38dd60476064dbc3b7c47cf279e02d1590eb5`.
Upstream revision: `34f75c49f93de5e2ff943483cabc5b6d0c5df918`, subdirectory
`noq`. Original Apache-2.0/MIT licenses, source and package manifest are retained.

The only adapter code addition is `Connection::established_paths`, a read-only
projection of the protocol engine's established path IDs under the existing
connection mutex. Iroh subscribes to path events before reading this snapshot.
An event subscription alone cannot recover paths established before registration.
The manifest binds the sibling `noq-proto` source so standalone adapter builds
use the same additive API as the enclosing workspace.

The protocol engine excludes paths still pending initial establishment,
abandoned paths retained for accounting, and closed connections. It enumerates
actual IDs rather than assuming that IDs stay below the concurrent-path limit.
Ordinary subsequent address migration/validation remains owned by Noq; this
snapshot does not grant a path permission to carry packets or change selection,
cryptography, wire format, congestion control or retransmission rules.

Vendoring is needed because the published adapter exposes an event stream and
individual-ID lookups but no current established-path snapshot. Do not edit the
Cargo registry cache, infer IDs from counters or duplicate protocol state in an
observer. Upstream convergence remains a separate tracked task.
