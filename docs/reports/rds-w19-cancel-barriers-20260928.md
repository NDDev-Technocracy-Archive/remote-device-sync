# Chunk-boundary cancellation barriers — 2026-09-28

Scope: W1.9 stronger physical cancellation barriers. Linux, synthetic
loopback fixtures. No native macOS, physical power-loss or real-network
qualification is claimed.

## What existed

Typed `Cancel` frames, caller `CancellationToken`s on
`send_file_cancel`/`recv_file_cancel`, session abort on control-stream
closure, and a bounded 32-permit `disk_job` pool already owned the
transfer lifecycle. The gap: once a transfer was *reported* canceled,
blocking work already dispatched could still run to completion — a
manifest scan hashed the whole source, and journal assembly could finish
and install the destination in the background after the caller was told
the transfer aborted.

## Implementation

A running filesystem syscall cannot be interrupted, so the barrier is
placed between chunks — the smallest unit the loops already iterate.

- `manifest_reader_cancellable_with(r, min, avg, max, stop)` — streaming
  FastCDC scan consults `stop` once per produced chunk and returns
  `ErrorKind::Interrupted`. `manifest_of_reader_cancellable` is the
  default-bound wrapper. `manifest_of_reader`/`manifest_reader_with` keep
  their exact signatures and delegate with `&|| false`, so existing
  non-cancellable callers are byte-identical.
- `Journal::assemble_cancellable(stop)` — the per-chunk verify/hash/write
  loop stops at the next boundary; `assemble()` delegates with
  `&|| false`. A stopped assembly abandons private staging without
  touching the live destination, and stored parts still complete a later
  un-canceled assembly.
- `cancel_flag_watcher` projects a caller `CancellationToken` into an
  `Arc<AtomicBool>` the blocking loops can poll; the watcher task is
  aborted once the transfer future resolves, keeping cancellation scoped
  to its own transfer.
- `send_file_inner`/`recv_file_inner`/`receive`/`receive_chunks` carry
  the flag. A canceled manifest scan re-maps to
  "sync transfer canceled by caller" instead of surfacing an io error.
- Assembly is gated by two sources: `cancel_flag` (caller token) and
  `assemble_stop`, an `Arc<AtomicBool>` set when the peer's typed
  `Cancel` arrives or the control stream dies mid-receive. Either source
  stops assembly at the next chunk — a canceled receive can no longer
  publish the destination after reporting the abort.

## Honest barrier semantics

The granularity is one chunk (≤ `MAX_CHUNK` = 256 KiB per check), not a
syscall. `serve_inner` (agent pull-serving) passes `None` — its abort
path was already peer-driven and its semantics are unchanged.

## Verification

- `cancellable_manifest_stops_at_chunk_boundary` — set stop →
  `Interrupted`; unset stop → manifest identical to the slice chunker.
- `manifest_scan_honors_cancel_flag` — raised flag aborts the blocking
  scan through `disk_job`; a fresh handle rescans to the same root.
- `cancel_flag_watcher_projects_token_state` — token flips the flag
  promptly; `None` wires to no task.
- `canceled_assembly_never_installs_destination` — stopped assembly
  leaves the prior destination byte-identical, keeps stored parts
  complete, and a reopened journal assembles successfully.
- Existing `v2_token_cancel_writes_typed_cancel_and_server_observes_abort`
  and `v2_pull_completes_and_resumes_after_cancel` e2e paths unchanged.

Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets --
-D warnings`, `cargo clippy --workspace --all-targets --features
rds-desktop/x11 -- -D warnings`, `cargo test --workspace` — all green
locally on Linux x86_64.

## Still open under W1.9

Peer-ignored-cancel hardening beyond stream closure, inactive/legacy
journal collection (W1.10), physical power-loss and native macOS
qualification.
