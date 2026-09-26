# RDS remediation execution

Started: 2026-09-24. Plan: [remediation-plan.md](remediation-plan.md).
Audit baseline: `87e7aeabaad861b0d3c63f6641c91539be19a3e0`.
Audit/plan snapshot: `6772d4a`. Historical findings stay in the dated audit;
this file records implementation progress rather than rewriting that evidence.

## Current state

All waves remain open. W1.1–W1.4 and W1.6–W1.8 passed the local Linux check
matrix; other tasks remain planned unless listed below. No deployment or owned-backend
promotion has occurred. Native macOS checks still require their platform lane.

The historical readiness review at source `ec48d75` found an external SSH-client
requirement; the later [native Rust SSH increment](ssh.md) removes that local
requirement while retaining a configured remote SSH server. The repository is
still not a completed remote access product. Frame presentation returns unavailable;
ScreenCaptureKit, image-copy and PipeWire capture probes remain placeholders.
The historical throughput scenario stopped timing at sender finish; the later
[verified transfer increment](benchmark-transfer.md) adds a receiver byte/digest
receipt and EOF barrier. Known-rate and phase calibration remain W0.2. The other
implementation gaps remain in W5 and W6/W7 alongside the open plan tasks. That Linux
receipt records 431 workspace, 238 expanded and 2 isolated iroh tests passing;
those results do not establish missing functionality or native platform/network
qualification. The O1 observability foundation recorded 444 workspace and 239 expanded
tests plus the opt-in infrastructure pipeline. The subsequent authenticated
admin/source-metrics increment is described below and in its own receipt.
Neither increment closes these product gaps or any wave.

| Task | State | Evidence / remaining scope |
|---|---|---|
| W0.1 | Partial | R01/R10 are agent regressions; R02 is now covered by transactional record/delete regressions; R03/R04 are journal regressions, with failures observed before fixing. R05 is covered by planted-link and directory-substitution tests. R06 failed before the name proof fix; R07 is covered by server expiry checks. R08 has failing-before actual-client Drain/drop regressions and passing framing/grace checks. R09 has failing-before direct/relay candidate regressions and passing family/cancellation checks. Desktop byte/cancellation and lifecycle regressions now cover the W6.1/W6.2 increments; broader native/network qualification remains open. |
| W0.2 | Partial; receiver completion barrier | Versioned transfer goodput waits for exact received byte count, BLAKE3 digest and response EOF under one operation deadline. A missing-receipt regression failed before the fix; corrupt/truncated/reordered payloads, invalid receipts, delayed reception and real iroh/noq forwarded-TCP checks cover the boundary. Known-rate calibration, connect/auth/service phase timings and topology/load qualification remain open; see [contract](benchmark-transfer.md). |
| W0.3 | Partial; Linux bench worlds contained | `Transports` bounds peer-path kinds per endpoint: iroh removes the transport itself (`clear_ip_transports`/`clear_relay_transports`) because its in-band QNT exchange cannot be disabled; noq suppresses direct candidates from `addr()`, dialing, advertisement and learned-candidate opens while keeping the UDP socket its relay attachment rides. `max_multipath_paths=1` now actually pins iroh via a first-path selector (config floors would otherwise leave migration open); noq pins through its own policy. noq bench worlds run the owned `rds-relay` with impairment proxies on each endpoint↔relay leg; noq impaired-direct uses socket-level impairment. Integrity enforcement now requires offered-vs-observed coverage (every sent datagram entered an impairment device) and a delay floor on measured path RTT; both caught the real iroh/noq escapes this change fixed. `resolve-connect` runs on noq through the owned relay. iroh relay-impaired remains an explicit skip (TCP relay leg, no UDP impairment model); recovery-path scenarios and real-network qualification remain open. |
| W0.4 | Partial; comparator faults refused | Absent metrics, nonfinite values, failed scenarios, insufficient samples and backend/impairment profile mismatches now refuse comparison (seed excluded from profile). `checkpoint.sh` registers `r0-evidence` with a CLI negative-fixture battery; the c1 noq suite gains its missing `transport-noq` feature flag. Gate invocation and re-qualification of historical reports remain open; see section below. |
| W0.5 | Partial; versioned matrix landed | `docs/capability-matrix.md` v1 records implemented/experimental/stub/unavailable per capability with runtime prerequisites separate from state. Tests enforce every `ServiceKind` variant and every `rds-bench` lane name has a matrix row, the state vocabulary is closed, placeholders name what is missing and README links the matrix. Live `rds info` advertisement ↔ matrix agreement beyond enum coverage, per-report capability tagging and release-gate enforcement remain open. |
| W0.6 | Partial; hash-chained receipt schema landed | `docs/receipts/rds-receipts.jsonl` is append-only JSONL, one receipt per gate/report: full commit SHA, dirty flag, bench-binary and Cargo.lock digests, toolchain channel, features, OS/arch, topology class, repetitions/failures/skips, budgets and cited-report digests — no host identifiers. `prev_hash`/`hash` chaining rejects tampered, reordered or mid-deleted lines with the offending line number; `rds-bench receipt`/`validate-receipts` are the writer/reader and `write_checkpoint` now records every gate run. Historical receipt backfill, `--report` coverage of bench JSONs inside gates and release-gate consumption remain open. |
| W1.1 | Implemented; Linux checks passed | Denylist replacement retains its value without observers; atomic modification preserves concurrent revocations. Subscribe-before-check and initial watchdog snapshot check remove missed-update windows. Durable feed freshness remains W1.4. |
| W1.2 | Implemented; Linux checks passed | One authorization state owns admission, replay reservation and watchdog. ACK failure/cancellation closes the connection and releases the grant. Service admission checks live validity/revocation. Connection future teardown runs RAII cleanup. |
| W1.3 | Implemented; Linux checks passed | Client trust anchor, per-name domain-separated signatures, exact name/record binding, current validity and volatile anti-rollback. Native directory HTTPS/DNS added; durable revision linkage stays W1.4 and native macOS verification remains open. |
| W1.4 | Implemented; Linux checks passed | Shared durable policy acceptance, positive epochs/revisions, domain-separated signatures, bounded revocation leases, restart/boot rules, dual-signed rotation, atomic feed ownership and live closure. Name trust persists across CLI processes. Native macOS/power-loss qualification and external GDS rollback anchoring remain open. |
| W1.5 | Partial | Transactional bounded disk store, tombstones, generation anchor, publisher revisions, exact retry, durable announce, leased expiry, retained floors, bounded collection, configured enrollment, fair write admission, strict HTTP framing and explicit format-2 offline migration are implemented. A 4096-identity Linux capacity/churn/reopen run passed. Legacy cutover, release-load/startup profiling, physical failure and native macOS qualification remain open; see validation below. |
| W1.6 | Implemented; Linux checks passed | Reused bytes are verified and stored before `have`; edits, insertions, deletions, repeated chunks and destination removal/restart are tested. |
| W1.7 | Implemented; Linux checks passed | Exclusive staging and RAII cleanup preserve ordinary/link siblings and colliding names; failed assembly retains the old file. Current staging uses reserved names inside locked private state, allowing deterministic recovery. |
| W1.8 | Implemented; Linux checks passed | Directory-relative no-follow journal/destination I/O and a held source file replace path-check-then-open. Link planting and substitutions after open are tested. Native macOS verification remains pending. |
| W1.9 | Partial | Pull path and Done-root binding, exact frame decoding, canonical Need, batch bounds, requested/unique chunks, verified completion, actual wire-byte accounting and absolute session budgets are implemented. Explicit transfer IDs/negotiation, stronger cancellation barriers and native macOS qualification remain open. |
| W1.10 | Partial; Linux transaction checks passed | Root and destination-parent locks cover overlapping roots and filesystem aliases. Reserved private staging, both-parent sync and bounded known-name recovery are implemented. 23 transaction/cleanup boundaries cover process exit and two returned-error classes. Physical power loss, native macOS, large-file campaign and inactive/legacy journal collection remain open. |
| W2.1 | Partial; endpoint settings checked on Linux | Shared version-1 endpoint JSON, explicit file/flag precedence, typed backend/relay validation, preflight before identity creation, owned-relay CLI/agent selection and canonical TCP targets shared with client and agent policy are implemented. Role-level service/authority settings and timeout policy remain open. |
| W2.2 | Partial; exact ALPN selection | Immutable per-protocol TLS offers prevent silent fallback and concurrent request interference. Managed single-file transfers use fresh control/uni routing IDs. Capability/limit/version negotiation and desktop session routing IDs remain open. |
| W2.3 | Partial; destination-bound renewable grants and directional scopes | Grant v2 adds a strict signature domain, audience and stable session ID across positive lease revisions. Same-scope renewal preserves streams/revocation, retains one replay slot/watchdog and enforces wall/continuous expiry. Explicit managed renewal uses IPC v3 and a control-completion barrier. `SyncRead`/`SyncWrite` and `DesktopView`/`DesktopControl` are enforced before filesystem/input operations. Tenant/policy binding, per-path/account scopes and automatic GDS issuer integration remain open. See [contract](grant-leases.md). |
| W2.4 | Partial; default connectivity manager | Agent local control is enabled by default; ordinary ticket/ping/info/SSH/forward/send/recv commands and keyless `rds session` reuse its endpoint. Same-UID IPC, pinned streams, cancellation and aggregate metrics are implemented. Agent/direct CLI/owned relay acquire exclusive ownership of a validated seed inode. Viewer manager APIs, coordinated installed-binary migration, native macOS and real multi-user/relay qualification remain open. See [contract](local-sessions.md) and [migration receipt](reports/rds-identity-migration-20260925.md). |
| W2.5 | Partial; transport and agent task ownership | Owned policy tasks terminate, including explicit shutdown after stopped protocol I/O; uni routing is bounded and acyclic. Agent and client forwarding groups own cancellation, normal joins and positive admission budgets. Client relay queues/peer leases and server admission/owned shutdown are bounded. Metric samplers use weak backend observations, release their gauges on drop and wake on closure independently of the sampling interval. Global RSS/FD bounds, per-service fairness and broader disk/media cancellation remain open. |
| W2.6 | Partial; client preludes bounded | One request deadline covers stream credit, writes, replies and Ping echo; canceled Authz closes its connection. Agent and owned relay handshake/shutdown budgets exist; canceling relay drain does not cancel cleanup. Agent local startup no longer waits indefinitely for an iroh relay, including disabled/unavailable relay mode. Global timeout classes, retry jitter, broader startup recovery and desktop/media deadlines remain open. |
| W3.1 | Partial; fair bounded candidate race | Canonical direct candidates alternate supported families under one eight-address cap, plus attached relay; attempts share a deadline and one authenticated winner. Independent relay bootstrap, progressive probing, remote scope/interface discovery and real topology qualification remain open. |
| W3.2 | Partial; owned binary runtime checked on Linux | Both server binaries share strict backend/allow/key/limit/TLS config, persistent relay identity, local readiness and checked joined shutdown. Real processes forward inner authenticated traffic and retain identity/catalog across restart. Malformed datagrams are charged before parsing, and routing uses authenticated key-table lookup. Unexpected service-runner completion now initiates joined host shutdown with retained failure. Hung-task/recovery policy, global/reconnect/control budgets and platform/network qualification remain open. |
| W3.3 | Partial; relay control and grace | Shared exact bounded codec, actual Drain/PeerGone receipt, usable grace traffic and stale-slot ownership are checked. Warm secondary relay and measured active-session migration remain open. |
| W3.6 | Partial; validated selection and local child failure isolation | Extra paths become eligible on Established; weak policy ownership includes bounded backoff for temporary connection-ID/path-credit exhaustion and candidate-address snapshots. Relay-link route loss and known tunnel closure retire stale relay selection. Mux child send/receive failures are isolated; policy withdraws failed advertisements, excludes failed routes even when last-path close is refused, and closes held connections after all-child loss. Real loopback fixtures preserve open-stream traffic and accept new connections on the surviving child. Policy-observed telemetry exposes sticky event loss and unknown selection. Full path-event resynchronization, lossless retirement metrics, interface/socket recreation, per-service scheduling and physical-network qualification remain open. |
| W4.4 | Partial; directional service boundaries | Real iroh/noq agents refuse writes with `SyncRead` and reads with `SyncWrite`, permit authorized transfers, and remain usable after refusal/cancellation. A view-only desktop never calls its input sink; failed injection never emits a success ACK. Linux/macOS account isolation, per-path policy, native seat/focus boundaries, consent and concurrent-role qualification remain open. See [receipt](reports/rds-service-scopes-20260926.md). |
| W5.1/W5.3/W5.5 | Partial; native SSH client and standard PTY | `rds-ssh` uses russh 0.63.3 over pinned managed/direct streams. Explicit host pins, key/agent authentication, PTY/exec acknowledgements, terminal restoration, resize, cancellation and complete exit/output handling have Linux regression coverage and an OpenSSH interop fixture. SSH-specific fixed telemetry names are accepted by Vector; JSON uses a separate private file and terminal console logging pauses during SSH. GDS host/account provisioning, certificates/MFA, native macOS, broker/reattachment and mixed-load/network qualification remain open. See [contract](ssh.md). |
| W6.1 | Partial; sender byte correctness and reference recovery | Pinned writes preserve progress; deltas finish before dependent successors and may be replaced by an independent keyframe. Queue loss requests IDR, and a sender sequence guard refuses broken delta chains after selection/pacing. Failure/cancellation resets streams under one deadline; serving owns child tasks. Failing-before byte/lifecycle regressions, native codec recovery and real iroh/noq RESET coverage pass. Real-codec network/overload qualification, wire session IDs and native presentation gates remain open; see [contract](desktop-frame-delivery.md). |
| W6.2 | Partial; client receive ownership and limits | Four encoded frames per session/eight per process, two blocking decoder calls, owned task groups and cancellation, full handshake deadlines, dimension/sequence checks and rate-limited keyframe recovery are implemented. Real iroh/noq lifecycle and native codec regressions passed locally. Per-session wire IDs, global decoded/native memory accounting, renderer and native platform/network acceptance remain open; see [contract](desktop-client-lifecycle.md). |
| W6.4 | Partial; ACK semantics and native X11 mapping | ACK follows successful backend acceptance. A bounded session-owned input worker reuses its sink. X11 maps evdev keys/buttons, bounds coordinates and fractional scroll, uses relative motion correctly, selects the explicit screen, checks native errors and releases owned holds on drop. Four failing-before native regressions and two-screen Xvfb checks cover the increment; the Linux CI lane requires actual native execution. Exclusive seat/controller ownership, focus races, custom layouts/IME, dynamic geometry and input-to-visible qualification remain open; see [contract](x11-input.md). |
| W10.1/W10.2 | Partial; O1 foundation and O2 admin/source increment | Shared bounded Rust telemetry and Vector/OpenObserve pipeline; opt-in authenticated loopback metrics on agent/relay/server, aggregate source observations and old public metrics removal. Durable policy/catalog observations and effective agent revocation revision/lease are implemented. Finer queue/task and upstream adapter coverage, phase/reason correlation, support bundles, private rollout, independent liveness and overhead/platform qualification remain O2–O6; see [contract](observability.md). |

## Authorization change

`rds-agent::authz` owns Pending → Authorizing → Granted → Closed. Services
remain pending while an authorization response is being written. The watcher
is installed before that I/O; a 15-second response deadline bounds it.
Duplicate Authz cannot replace a pending or granted lease. Failed verification,
response write, interrupted commit and canceled admission close the connection.
Closing the connection cannot be undone by a late admission task.

The watchdog checks the current denylist immediately, then observes updates
and wall-clock expiry. Each service also checks policy directly so correctness
does not depend only on watchdog scheduling. Replay reservations and watchdogs
are released through owned drop guards, including cancellation of the
connection service future.

Regression evidence before implementation:

- `revocations_survive_without_watchers`: failed; initial revoked ID was lost.
- `concurrent_revocations_are_not_lost`: failed; fewer than 16 concurrent IDs survived.
- `failed_authz_reply_closes_connection_and_releases_grant`: failed; stopped
  reply retained the connection beyond the test's bounded observation window.

Additional coverage exercises simultaneous admission ownership, a snapshot
already present before watchdog startup, admission cancellation, immediate
service-scope revocation/expiry and reservation cleanup.

Local validation on 2026-09-24: `cargo fmt --check`; workspace clippy with
default features, `rds-desktop/x11`, and all features (all with `-D warnings`);
`cargo test --workspace`; `cargo test -p rds-agent --all-features` (5 unit and
15 integration tests). All passed. Native macOS execution is still pending.
`cargo-deny` is not installed locally. No wave checkpoint is claimed: these
changes do not close W1 or replace historical benchmark evidence.

The signed revocation feed still needs durable anti-rollback/freshness/offline
policy (W1.4) at this historical patch; subsequent sections record its completion.
The later [grant v2 increment](grant-leases.md) adds audience binding and explicit
renewable leases, while other W2.3 and task-ownership W2.5 work remains open.

## Sync correctness and filesystem change

Baseline `cargo test -p rds-sync --test journal` failed four regressions:
partial reuse was lost on reopening, repeated reused chunks could not assemble,
and both the ordinary `.rds-part` sibling and its link variants were deleted.
The new journal materializes reused content, including size-changing deltas,
then assembles only verified parts. `fetched` counts network chunks, not reuse.

`confined::Directory` holds directory handles and uses safe `rustix` bindings
for relative no-follow opens, rename and unlink. The private state directory
has mode 0700, including journals created by the earlier implementation.
Staging files use exclusive creation and random 128-bit names; collision
handling never removes someone else's entry. All state reads are length-bounded
and refuse special files, symlinks and multiply linked part files. Cleanup
does not recursively walk arbitrary journal entries.
The journal namespace is reserved regardless of ASCII case; a second check
compares opened directory identities to refuse filesystem case/Unicode aliases
of `.rds-sync` before descending into a peer-requested path.

The configured root's ancestors and the local OS identity are trusted. A held
directory is an inode capability, including after rename; this does not promise
an absolute pathname boundary against a privileged/local actor relocating that
inode. A remote peer cannot select arbitrary roots or enter `.rds-sync`.
Journal and destination directory handles stay pinned throughout I/O; pull
chunk reads use the same opened file as the manifest, with positioned reads
shared across senders.

Receives within one root are serialized by one persistent lock inode. This
intentionally also prevents alias races on case/Unicode-insensitive filesystems
and avoids an unbounded collection of per-filename locks. Separate roots remain
independent. More granular parallel receives require a proven alias model.
Existing parts remain readable after upgrading, but old and new receive
processes must not share a root concurrently: old binaries do not take the
new lock. New staging files have mode 0600; file metadata preservation is W8.5.
The receiver watches control-stream cancellation while collecting chunks;
sender task ownership uses `JoinSet` so cancellation aborts its async children.
Blocking disk jobs can finish their current operation and drain their bounded
queue; fully interruptible scans and absolute budgets remain W1.9/W2.5.

File and parent sync occur before reporting commit. A parent-sync error after
rename reports uncertainty and keeps recovery state; the caller receives no
false `Done`. The process-crash test targets committed parts and lock recovery;
it does not simulate loss of OS caches or certify macOS power-loss durability.
Unknown orphan staging entries are preserved for future owned-state GC (W8).

Dependency rationale: `rustix` supplies thin safe OS calls; `rand` supplies
staging entropy. Both versions were already in `Cargo.lock`. No helper process,
native code block or new dependency version is introduced by the sync runtime.

## Measurement fixture correction

An intermediate workspace run failed the existing 20-second desktop soak:
frame age p95 1319 ms, p99 1657 ms, 835 received frames. Running the **same
compiled test alone**, without changing its thresholds, passed at p95 32 ms,
p99 63 ms and 1110 frames. This indicates sensitivity to the execution context;
it does not establish a unique root cause or certify production latency.

Inspection also found that the supposed clean in-process desktop lane used
default endpoints with public discovery enabled. That fixture now explicitly
binds loopback and disables discovery, so its direct-path claim is enforced.
Impairment remains on the separately configured controlled socket lane. The
150/250 ms p95/p99 budgets are unchanged. The final check matrix passed after this fixture correction and the journal
namespace-alias check: format; workspace clippy in default, X11 and all-feature
configurations; and `cargo test --workspace` (135 tests in 36 targets). The
sync crate contributes 5 unit, 13 journal and 14 end-to-end tests. Native macOS
and power-loss qualification remain open; no entire wave is closed.

## Name identity verification

Regression `unsigned_directory_name_cannot_select_an_identity` failed before
implementation: the client accepted the endpoint key in an unsigned JSON
answer. The directory now serves a `SignedNameBinding`, created by the registry
issuer for each name. The client requires an independently configured verifying
key, checks the signature's domain/version/name/lifetime, and the resolver then
binds the endpoint record to the verified key. No whole inventory or issuer
secret is returned to a lookup client.

The directory verifies all per-name proofs agree with the uploaded registry,
refuses snapshots without them, and stops serving names at expiry. The old
wire response is intentionally unsupported; the issuer must re-sign snapshots
and clients must provision `--registry-key`. Migration is documented in
`deployment.md`. Maximum binding lifetime is 24 hours with no future timestamp
allowance; bounds are 256 names per snapshot, 256 KiB serialized snapshot,
256-byte binding payload and 1024 remembered names per client. Cache overflow
refuses new names instead of discarding freshness evidence.

The timestamp cache is shared across client clones and rejects older or
conflicting same-issued-at responses. It is not durable and cannot discover a
withheld newer revocation or name removal. Revision/epoch persistence,
authority rotation and outage policy are W1.4.
Pinned ticket/key paths remain independent of name resolution.

Local validation: format, default/X11/all-feature workspace clippy and
`cargo test --workspace` passed (141 tests in 38 targets). Six new name-trust
tests exercise unsigned answers, wrong names/issuers/records, signature-domain
confusion, malformed/oversized proofs, redirects, future/expired/excessive
lifetimes, rollback/equivocation and server expiry. Native macOS remains open.

## Directory HTTPS and DNS

`Client::from_endpoint` accepts a validated HTTP(S) origin or legacy plaintext
socket address. TLS checks public WebPKI roots and hostname/IP SAN; explicit
private CA bundles replace the roots. URL credentials, paths, queries,
fragments, malformed origins and CA configuration on plaintext are refused.
Requests send the correct Host authority. There is no TLS-verification bypass,
HTTP downgrade or redirect following. Signed identity checks still run over TLS.

One deadline covers DNS, bounded staggered TCP attempts (16 maximum), handshake
and response. Dropping that future aborts its pending dial tasks; native OS
resolver work can complete later without extending the caller's deadline.
The TLS-only server listener reserves capacity before spawning and bounds the
entire handshake/request. Its owned JoinSet bounds active/completed handles and
aborts async request children on service drop. Blocking stores remain W1.5;
this is not a claim of complete W2.5
task ownership throughout the agent.

Six TLS tests passed locally: DNS lookup and signed record/name roundtrip;
missing/wrong name authority despite valid TLS; untrusted CA, wrong hostname
and expired certificate rejection; strict origin/PEM configuration and Host
header; stalled handshake timeout without plaintext retry; capacity recovery
and pending request teardown. The full Linux matrix passed: format;
default/X11/all-feature workspace clippy with warnings denied; and
`cargo test --workspace` (147 tests in 39 targets). Native macOS, edge proxy
compatibility and certificate renewal automation remain open.
All three binaries (`rds`, `rds-agent`, `rds-server`) also built and executed
`--help` successfully with their new directory TLS/CA options present.

Dependency rationale: rustls/tokio-rustls provide standard TLS, rustls-pki-types
parses PEM, webpki-roots supplies trust data, and url validates/normalizes DNS/IP
origins. rcgen generates synthetic test certificates only. All versions already
existed in Cargo.lock. No helper binary is added. The selected existing ring
crypto provider contains native code/assembly; this is not a pure-Rust crypto
implementation claim. TLS and protocol policy stay in RDS Rust code.

## Next sequence

While validating W1.4, the workspace run exposed a measurement race in
`bounded_queue_newest_wins`: draining a live queue returned 65 headers over
time even though the instantaneous capacity is 64. Receiver depth now has the
same atomic diagnostic access as sender depth, and the test checks occupancy
before and during a bounded batch. Frame order, freshness and the 64-entry
limit remain asserted; no capacity or latency budget was relaxed. The focused
regression passed after this W0.1 fixture correction. Full matrix results are
recorded with the policy change that triggered the run.

Continue W1.5 directory transactions and W1.9 wire
binding/accounting. Each change retains
its own failing-before/passing-after evidence. No general CI or long soak is
made a blanket barrier to independent development; actual invariant failures
are repaired and platform evidence is recorded separately.

## Durable policy acceptance and managed revocation

W1.4 replaces second-resolution timestamp ordering with explicit authority
epochs and revisions. Name proofs bind to the signed registry digest; the CLI
persists one global revision floor across names/processes. Policy snapshots and
lease metadata commit together through protected, exclusively owned staging
before publication. Failed writes poison the store and close managed access;
corruption, missing initialized state or unknown bootstrap keys never select an
empty fallback. A lower-revision bootstrap cannot replace the directory's
committed state. Unchanged cache hits do not rewrite/sync the marker file.

The managed agent observes revoked IDs and freshness in one value. Startup can
use a verified same-boot cache only until its original absolute lease ends;
replies and restarts cannot rearm that lease. OS reboot needs a newer revision.
Admission, admission commit, new service requests and the live watchdog all
check policy. A feed has one owner; shutdown/fatal persistence error prevents
late publication, and obsolete callbacks cannot overwrite a successor.
Authority rotation requires signatures by both the current and next key and
advances the epoch by exactly one.

Five real-QUIC managed-feed cases passed, covering fresh/missing policy,
revocation, outage, cached restart with older network replies, feed drop and
failed disk commit. Nine policy-state cases cover ordering, boot/clock changes,
rotation, linked/corrupt/missing state and bounded wire inputs. Fault and abrupt
child-exit tests cover five persistence boundaries. Directory/client integration
tests cover actual restart and cross-process-style cache reuse. The worker
budget test confirms a timed-out HTTP request cannot free a still-running disk
job's permit. Its initial test assertion was corrected to use the client's
typed `RateLimited` error for HTTP 429; no production limit changed.

Linux validation: `cargo fmt --check`; default, X11 and all-feature workspace
Clippy with warnings denied; `cargo test --workspace` (**170 tests, 43 targets**).
The owned transport/relay feature suite also passed (**52 tests, 17 targets**).
The five managed-feed cases were additionally rerun with their endpoints
explicitly selecting noq, and all passed. `rds`, `rds-agent` and `rds-server`
built and executed `--help` with the new state/epoch/rotation flags.
The first workspace failure and the separate desktop fixture correction are
recorded above. `cargo-deny` is not installed. Native macOS, physical suspend,
power loss and real estate deployment were not run. No wave is closed and no
performance qualification is claimed. See the [policy receipt](reports/rds-policy-20260924.md)
and [migration/operational contract](policy-state.md).

## Record transaction foundation

W1.5 now keeps delete tombstones in both stores, serializes mutations and stores
record/tombstone/catalog updates in an embedded Rust transaction. The file
store's separately synced generation anchor refuses database-only rollback of
acknowledged history. Corrupt/missing initialized files and legacy directories
are refused; unexpected I/O closes the instance until recovery. Eight new
integration tests and six unit tests cover replay, concurrency, confinement,
bounded storage, partial writes, sync faults and process exits. No runtime
subprocess or database service is added. Migration is deliberately not inferred
from old files; deployment remains pending the versioned publisher work.

Formatting and all three Clippy lanes passed. The default workspace run failed
the existing parallel desktop soak latency gate (p95 176 ms/p99 1447 ms); a
single isolated diagnostic rerun passed at 10/16 ms. The complete sequential
test run passed **184 tests, 44 targets**. This does not erase the default-run
failure or close W0/W6 performance evidence. No thresholds changed. See the
[record transaction receipt](reports/rds-records-20260924.md) for exact scope.
That storage patch left signed revisions and durable publisher allocation for
the following change, then expiry/quotas, enrollment fairness and HTTP framing.

## Versioned endpoint publication

The next W1.5 change replaces timestamp ordering with one positive revision
sequence for record updates and deletion. New signature domains bind version,
identity, revision and bounded validity. Verification precedes variable-field
decoding; exact signed retries are idempotent, while equal-revision conflicting
content and older mutations are refused. Both stores enforce current lifetime
on writes and reads; resolver checks remain independent of the server.

`RecordIssuer` persists the counter and pending signed bytes before publication.
The CLI owns durable state, while fixtures explicitly select volatile issuers.
Same-second address changes receive different revisions. Retries preserve signed
bytes until actual renewal/change; restart resumes the pending operation. Fatal
history/disk failures and permanent protocol refusals reach the agent supervisor
instead of silently stopping the announce loop. Existing connections close on
that fatal CLI path. A lost successful HTTP reply is exercised through a real
directory and proxy, with an identical second request accepted successfully.

Ten revision/issuer cases and three announce lifecycle cases cover retry,
conflicts, delete ordering, expiry, wire bounds, restart, missing history and
permanent rejection. Publisher fault tests cover five write/rename/sync phases,
including abrupt child exits. Old fixtures that advanced `issued_at` into the
future were updated to use revisions. The expiry test now checks both a 410
from the real store and independent client rejection of an expired signed
record returned by a hostile HTTP 200 response.

Record validation and the owned transport share a typed `rds-relay://` locator
parser, preserving relay identity and IPv4/IPv6 sockets. The real owned relay
test now goes through announce, directory storage and resolution before forcing
the handshake, datagrams and stream exchange onto the relay. This caught a
compatibility gap in an HTTP-only validation rule; the previous IPv6 bracket
parse failure is also corrected. No IPv6 network qualification is implied.

The first workspace run exposed a state-lock lifetime issue: a descriptor alias
can retain flock after its owner drops. A deterministic reproducer failed;
explicit guard release now passes across atomic state, database and sync root
locks. See the [separate lock receipt](reports/rds-locks-20260924.md), committed
as `8a18064`. This correction does not add sleep/retry to hide a failed invariant.
The final default `cargo test --workspace` run passed **204 tests across
46 targets**, including the parallel desktop smoke test and crash/reopen cases.
Formatting and default, X11 and all-feature workspace Clippy also passed with
warnings denied. This does not qualify sustained production latency or erase
the preceding failed-run evidence.

`cargo test -p rds-net -p rds-agent -p rds-relay --all-features` passed
**58 tests across 18 targets**, including the real owned-relay feature (not just
the noq endpoint feature). The agent binary built and executed `--help` with
`--record-state`. See the [publisher receipt](reports/rds-publisher-20260924.md).
Native macOS, physical failures, migration and real estate deployment remain
unqualified. This receipt precedes the expiry-retention change below.

## Record expiry and retention

W1.5 now persists each record/delete acceptance lease, using the same boot and
suspend-inclusive clock adapter as policy state. Exact retries cannot rearm it.
Expiry observed by a read commits retirement and a clock floor before returning;
expired addresses/signatures are reclaimed while revision/digest/kind history
remains. OS reboot requires a newer signed revision. Backward wall time refuses
operations until it catches up. Whole-state rollback still needs a GDS anchor.

Both stores share expiry/order decisions and retain at most 4096 identities;
memory deployments can choose a lower capacity. Collection keeps those identity
slots. A higher revision for an existing identity remains possible at identity
saturation. One owned maintenance worker visits at most 64 rows per pass,
separately from request-worker capacity, with retirement/failure metrics.
Concurrent renewal and collection serialize through the store transaction.

An HTTP 410 triggers a durable local successor; losing its reply retries the
same new bytes. HTTP 409 remains fatal. An already acknowledged publisher checks
the server again on its normal renewal, so immediate recovery after server reboot
is not claimed. Native platform, physical-failure and latency qualification
remain open. The database is format 3; deployment migration is still on hold.

The Linux matrix passed: formatting; default/X11/all-feature workspace Clippy
with warnings denied; **221 workspace tests across 47 targets**; and **59 tests
across 18 targets** for the network/agent/relay all-feature lane. Targeted checks
also passed: 38 discovery unit tests, two service-collection tests and four
real-HTTP publisher lifecycle tests. See the
[expiry receipt](reports/rds-expiry-20260924.md). No wave gate is closed.

This receipt precedes the admission change below.

## Publisher enrollment and write fairness

Default directory configuration now denies record access. Production explicitly
lists publishers with `--directory-allow`, independent of relay/agent permissions
and registry names. The static list is bounded to 4096 distinct nonweak keys;
removal hides records while preserving floors. GDS live reconciliation remains W4.

The before-fix regressions failed: unconfigured publication succeeded, and one
writer's refused requests spent the shared counter and blocked another device's
renewal. Both now pass. Stores verify and compare before invoking quota admission
under the same compare/commit owner. Exact retries, stale operations and bad
signatures do not spend the owner's quota; concurrent identical copies charge
once. Callback refusal does not change the record or poison storage.

Known identities have one protected mutation in each fixed 60-second window.
New admissions and extra writes have separate shared budgets, with per-identity
limits checked first. Each policy role has its own budget after signature and
revision validation. TTL/3 renewal with TTL >= 180 fits the reserved cadence;
faster renewals need burst capacity. The guarantee concerns write quotas, not
isolation from arbitrary network, CPU or disk saturation.

Validation and limits are recorded in the
[admission receipt](reports/rds-admission-20260924.md). The full Linux matrix
passed: formatting; default/X11/all-feature Clippy with warnings denied;
**234 workspace tests across 48 targets**; **59 all-feature network/agent/relay
tests across 18 targets**; and server build/`--help` with `--directory-allow`.
No wave completion is claimed. Next: strict HTTP framing, explicit
migration/file-capacity qualification, then W1.9 wire/accounting.

## Unambiguous directory HTTP framing

Seven regressions failed against admission commit `e46125f`, including a real
signed PUT that modified storage despite conflicting Content-Length fields.
Both directions now share strict header validation and reject ambiguous lengths,
transfer encoding and malformed fields/start lines. Exact head/body bounds and
outgoing validation apply before a body is consumed or bytes are emitted.
Buffered head reads retain prefetched body data; the connection always closes
after one exchange. Parsed HEAD responses omit bodies, including on overload.

The expanded framing target passes eleven cases, including the real client,
maximum binary bodies fragmented through a tiny stream, truncated messages,
bodyless statuses and pipelined-request closure. The
[HTTP receipt](reports/rds-http-20260924.md) preserves the before/after evidence
and the [private API profile](record-state.md#directory-http-profile) describes
compatibility limits. No helper program or dependency was added. Particular
proxy/tunnel routes and native macOS execution remain unqualified.

The Linux matrix passed: formatting; default/X11/all-feature Clippy with warnings
denied; **245 workspace tests across 49 targets**; and **59 all-feature network/
agent/relay tests across 18 targets**. The first extra feature run exposed a
one-second expiry-fixture race; the receipt records its correction and the
successful default and all-feature reruns, without changing production expiry
semantics. Next: file-capacity qualification and explicit migration, then W1.9.
All waves remain open.

## Full directory capacity measurement

The Rust `rds-bench directory-capacity` scenario uses shared production bounds,
fresh synthetic state and 32 IPv6 addresses/eight maximum raw relay origins per
large record. It passed **16,384 signed mutations and four full reopens** at
4096 retained identities. The extra identity was refused without charging an
admission callback; existing records continued to renew and deleted identities
reactivated at higher revisions. Maximum sampled database length was **35.004
MiB** under the unchanged 256 MiB cap. See the
[capacity receipt and raw artifacts](reports/rds-capacity-20260924.md).

This is one bounded Linux run in a dev build with optimized cryptographic
dependencies, not release throughput or physical-failure qualification. Complete
catalog validation during reopen took **1.54–15.98 seconds** across four samples;
startup/recovery profiling remains open alongside sustained renewal load. The
first unoptimized attempt was deliberately stopped after filling the catalog
to replace its undersized overall harness deadline; it is retained as partial
evidence, not counted as a pass.

At that checkpoint the next implementation step was the
[offline migration checklist](record-migration-plan.md): preserve format-2 signed
revision floors without inventing new leases; treat timestamp/legacy formats as
a separate authenticated cutover. That plan alone did not provide a migration
command or deployment approval. The implementation update follows below.

Final Linux checks passed: formatting; default/X11/all-feature workspace Clippy
with warnings denied; **245 workspace tests across 49 targets**; both harness
build profiles and the CLI state-preservation guards. No dependency or external
runtime helper was added. All waves remain open.

## Explicit format-2 offline migration

`rds-server migrate-v2` now verifies a read-only source copy and imports all signed
revisions/deletions as retired format-3 floors. It retains the original bytes,
rejects existing or ambiguous destinations and publishes the complete new sibling
only after catalog validation, a durable receipt and exclusive rename. Imported
addresses require a newer signed publication. Intent and receipt files remain in
protected sibling audit state; there is no automatic live cutover, retry or cleanup.
See [operation and remaining qualification](record-migration-plan.md) and the
[2026-09-25 receipt](reports/rds-migration-20260925.md).

Linux validation passed returned-error and abrupt-exit cases at 12 migration
checkpoints, malformed/history/ownership cases and real HTTP 410 → durable
successor → unread reply → issuer restart/exact retry. The separately invoked
4096-identity capacity migration preserved every floor and source byte. Import
took 67.658 seconds in an unoptimized test build; it is an offline conversion
measurement, not connection latency. Final formatting, three workspace Clippy
lanes, **256 workspace tests** and **59 all-feature network/agent/relay tests**
passed. No dependencies or external runtime helpers were added.

W1.5 remains open for timestamp/per-key legacy cutover, external GDS anchoring,
release load/startup, physical-failure and native macOS qualification. Next local
implementation work is W1.9: bind sync requests and acknowledgments to their
intended file and enforce unique chunk accounting and bounded protocol progress.

## Sync request, completion and progress integrity

W1.9 now binds a pull Offer to its requested normalized path before receive state
is opened, and both sender roles verify the Done root. The shared frame decoder
requires an exact postcard payload. Need bitmaps, manifest parts and chunk sets
have enforced dimensions and padding; empty streams/batches and duplicate or
unrequested indices fail. Headers agree with the manifest before allocation,
and the journal must verify every requested index before assembly and Done.
The receive route is registered before Need. Wire-byte statistics count actual
requested payload, so identical reuse reports zero chunks and zero bytes.

Every role has a one-hour default absolute session budget, with explicit library
overrides and five-minute I/O stall bounds. Progress never extends that deadline.
The blocking writer signals every exit through an owned one-shot channel, waking
collection immediately on verification/storage failure or panic, including when
the peer becomes silent. No detached network wait can hide that worker exit.
Read [the exact contract](sync-protocol.md) and
[regression evidence](reports/rds-sync-protocol-20260925.md).

Four transport regressions and the shared-frame regression failed before their
fixes. An additional corrupt-body/silent-peer case exposed the missing worker
notification during validation; it now fails promptly while retaining prior
destination contents and releasing journal ownership. The ten protocol scenarios
cover both transports for path/root binding and owned transport for malformed
input, timeout, reuse and cleanup cases.

Final Linux validation after the worker-exit correction passed formatting,
default/X11/all-feature workspace Clippy with warnings denied, **267 workspace
tests across 50 targets**, and **59 all-feature network/agent/relay tests**. Existing
impaired transfers, resume and concurrent desktop/sync behavior remain covered.
The separate 4096-row migration qualification is unchanged. No dependency or
runtime helper program was added; `cargo-deny` remains unavailable locally.

W1.9 remains partial for explicit transfer IDs/negotiation and native macOS
qualification. Blocking syscalls and a started complete file replacement can
outlive async cancellation; stronger publication barriers remain W2.5/W8. Next
local work is W1.10's remaining journal commit/failure/collection evidence, then
W2's unified session configuration, negotiation and lifecycle ownership. No
existing wave is closed by these changes.

## Journal commit recovery and overlapping receive roots

W1.10 now holds a common destination-parent lock in addition to the configured
root lock. A new regression failed before this fix: roots `root` and
`root/nested` could concurrently receive the same destination. Nested private
namespace access also failed its new regression; `.rds-sync` is now reserved at
every depth, including handle-level alias checks.

Assembly lives under its parent's locked private state and syncs both directory
parents after rename. Metadata and part transactions use reserved private
temporary names. Recovery removes only known regular single-link temporary
files, verifies committed parts and preserves unknown data. Cleanup after
durable publication is synced and best effort without changing a committed
success into failure. Existing journal parts remain compatible.

The [recovery contract](sync-journal.md) and
[receipt](reports/rds-sync-journal-20260925.md) distinguish 23 process-exit cases,
46 injected-error cases and 12 temporary-object cases from physical-failure
qualification. Final formatting, three Clippy lanes, **273 workspace tests** and
**59 all-feature network/agent/relay tests** passed. One historical test allowing
nested private paths was updated to enforce the corrected namespace contract.

W1.10 stays partial for physical power loss, native macOS, actual nested mounts,
the large-file kill campaign and W8's inactive/legacy journal collection/quotas.
Async disk cancellation and concurrent destination edits remain separate work.
No new dependency, unsafe block, external runtime helper or deployment was added.
Next local implementation is W2.1 endpoint configuration validation and explicit
backend/relay selection, followed by negotiation and lifecycle ownership. No
wave is closed by this local increment.

## Versioned endpoint configuration

CLI and agent now share a bounded JSON endpoint schema with explicit defaults,
file/flag precedence and mutually exclusive relay modes. Unknown fields, unused
backend-specific relay settings and invalid dimensions fail before endpoint
identity creation. Iroh refuses extra bind addresses; noq refuses iroh relay
URLs instead of ignoring them. Its key-pinned owned relay is configurable through
both binaries. An outer relay socket now uses an independent ephemeral port,
preserving a fixed primary bind. Endpoint secrets and policy authorities remain
separately provisioned. See [configuration](endpoint-configuration.md) and the
[receipt](reports/rds-endpoint-config-20260925.md).

Two new baseline tests failed for ignored settings. A fixed-port owned relay
fixture failed during integration and now passes against a real local relay.
The strict-schema tests caught an ignored-field serde corner case. The original
socket-mux test also caught an overly strict duplicate rule: repeated port 0
requests now correctly allocate independent sockets, while fixed duplicates fail.

Final formatting and three Clippy lanes passed, along with **285 workspace tests**,
**72 all-feature network/agent/CLI/relay tests**, four feature-isolated schema tests
and five isolated binary tests. An initial disk-full build was recovered by
clearing only this workspace's generated incremental cache and limiting compiler
parallelism; it is not runtime durability evidence. Existing dependency versions
are unchanged; `rustix` and `thiserror` become direct network dependencies and
`serde_json` moves from test to runtime use. No runtime helper program was added.

W2.1 remains partial for unified role/service/authority configuration and timeout
policy. The next local correction is common SSH/TCP destination parsing and
preflight, followed by negotiated sessions, scopes and structured lifecycle work.
Native macOS, real reachability/failover and deployed integration remain open.

## Canonical SSH/TCP destination preflight

W2.1 now shares `rds-core::TcpTarget` between both command-line frontends,
the client library and agent wire handling. Canonical IP/hostname representation
is used by both policy matching and actual TCP dialing. IPv6 flags use brackets;
invalid syntax, oversized names, non-unicast literal addresses and zero ports
fail before network operations. Actual binary tests verify invalid flags leave
the identity file absent. Development `allow_any_tcp` still validates targets.

Two baseline argument regressions and two policy regressions failed before the
fix. A real IPv6 TCP service is reached through both QUIC backends while an
off-policy address stays denied; malformed raw requests are rejected and do not
prevent a subsequent valid request. See the
[contract](endpoint-configuration.md#tcp-service-destinations) and
[receipt](reports/rds-tcp-target-20260925.md). No dependency, wire version or runtime
helper changed. Syntax normalization does not replace DNS address pinning or
SSH host-key/authentication policy. Role configuration, timeout classes and
negotiated session ownership remain open. The next local correction is the
owned path driver's terminal lifecycle (audit T05 / W2.5).

The all-feature matrix exposed a W1.2 ordering race: a successful Authz ACK can
reach the peer before the authorization task resumes to commit, so an immediate
service was incorrectly rejected as pending. A deterministic real-QUIC test
reproduced it. Services now wait for an in-progress admission with a deadline,
subscribing before inspecting state, then recheck the committed grant and policy.
Failed commit and closure wake the waiters without granting access. Tests cover
commit, abort and revocation; the original managed-policy integration fixture
remains unchanged. Requests arriving before authorization starts still fail.

Final formatting and three Clippy lanes passed, as did **296 workspace tests**,
**81 all-feature network/agent/CLI/relay tests** and **7 isolated binary tests**.
The initial full-feature failure and its deterministic reproduction are retained
in private evidence; only the corrected final matrix supports these totals.
No deployment or remediation wave closure occurred.

## Owned connection-driver teardown

Audit T05 / W2.5 now uses a weak QUIC closure notification to terminate the
path-policy loop, including when closed handles remain alive or the last I/O
handle drops. Endpoint-owned tracking seals admission before closing connections
and waits for policy-task cleanup. Completed tasks are removed immediately.
Pending QNT history and closed weak path handles are pruned.

Both lifecycle regressions failed before the correction. Five focused tests
also cover stream-only ownership, 32 connection cycles returning to zero tasks,
and concurrent endpoint close with eight retained connection pairs. Final
formatting, three Clippy lanes, **301 workspace tests** and **86 all-feature
network/agent/CLI/relay tests** passed. See the
[receipt](reports/rds-driver-lifecycle-20260925.md). Existing locked `tokio-util`
becomes an optional direct dependency for task tracking; no package version,
wire protocol, unsafe code or runtime helper program was added.

W2.5 remains partial for uni-stream routing, agent/service task groups, global
and per-session budgets, relay queues and disk cancellation. Validated path
selection and larger churn/load qualification remain W3.6. R08 concerns relay
Drain framing and stays open, as does R09 candidate fallback. The next local
lifecycle work is the uni-stream router's ownership and pending-task bounds.
No wave or deployed release is closed by this increment.

## Uni-stream router ownership and limits

Two real-QUIC regressions failed on iroh and owned noq: dropping the facade and
inbox left the connection retained by its own router task. The private router
now uses weak owner references; live inboxes can still own I/O independently
of facade handles. Its task group caps tag/queue handoff work at 64 workers.
Closure cancels and joins them before registered producers are cleared; each
inbox keeps at most 128 buffered streams for explicit draining.

Tests cover last-owner teardown, facade clones, surviving inboxes, 80 partial
tags, ready routing behind a stalled tag, reclaim and a full 128-slot inbox
with 64 pending handoffs. Existing fixtures now retain the peer handle and
actually send a partial tag instead of only opening a local stream. Final
formatting, three Clippy lanes, **307 workspace tests**, **92 all-feature
network/agent/CLI/relay tests** and **7 feature-isolated routing tests** passed.
See [contract](uni-routing.md) and [receipt](reports/rds-uni-routing-20260925.md).
No dependency, unsafe code, wire version or runtime helper changed.

W2.5 remains partial for agent/service ownership, admission and other resource
budgets, relay pumps and disk cancellation. Kind-based routing still needs
session IDs, negotiated limits and per-service fairness; local task bounds are
not RSS/FD or latency-percentile acceptance. The next local lifecycle work is
owned agent connection/service tasks and bounded admission. No wave is closed.

## Agent service ownership and admission

Two before-fix regressions showed runner cancellation left child sessions alive
on both transports. The agent now owns connection/service groups and inline
metrics sampling. Normal closure joins its authorization watchdog and service
workers. Admission slots cover pending handshakes and live connections, while
per-connection stream budgets apply backpressure. The positive CLI limits
validate before identity creation. See [contract](agent-lifecycle.md) and
[receipt](reports/rds-agent-lifecycle-20260925.md).

Real TCP forwarding, runner/direct-serve cancellation, saturated partial hellos,
refusal/readmission, watchdog cleanup and invalid binary limits passed. Final
formatting, three Clippy lanes, **314 workspace tests** and **100 all-feature
network/agent/CLI/relay tests** passed. No dependency or wire version changed.

W2.5 and W2.6 remain partial: cancellation destructors request nested aborts;
only the normal path joins all these children. Global resource bounds, client
forwarding, relay pumps, media/disk cancellation, service fairness and complete
timeout/retry policy remain open. Next: complete client request deadlines and
owned bounded local forwarding. No remediation wave or deployment is closed.

## Complete client preludes and owned TCP forwarding

Two real-QUIC regressions reproduced unbounded stream-credit/write waits before
the old ACK timeout. Requests now have one 15-second prelude budget, and failed
or canceled streams reset/stop. Authz cancellation closes its connection;
successful one-shot responses end at FIN. TCP/Sync bodies retain their own
service policy. Local forwarding uses a positive configurable worker limit and
owned cleanup, with normal joins and cancellation-triggered socket closure.

Final formatting, three Clippy lanes, **323 workspace tests**, **109 all-feature
network/agent/CLI/relay tests** and **13 isolated CLI tests** passed. See
[contract](client-lifecycle.md) and [receipt](reports/rds-client-lifecycle-20260925.md).
Existing iroh is a new test-only direct dependency; no package version or runtime
helper changed. W2.5/W2.6 remain partial for global budgets, startup, relay/media
ownership and full timeout/retry classes. Next: shared bounded owned-relay
control framing, actual Drain/PeerGone receipt and preserved grace traffic.
No remediation wave or deployed release is closed.

## Cancellation of queued sync stores

The relay-control full matrix exposed a G6 immediate-retry refusal while prior
disk work still held its exclusive receive lock. Two deterministic regressions
confirmed a canceled sink continued storing queued chunks. Its guard now aborts
unstarted blocking work and stops running workers between stores, including
when finish is canceled. Normal finish still drains and verifies every chunk.

Formatting, sync Clippy, **3 focused unit tests** and **14 sync e2e tests** passed.
See [contract](sync-journal.md#receive-cancellation) and
[receipt](reports/rds-sync-cancel-20260925.md). G6's final phase now tests bounded
byte-identical convergence after transient refusal; a fixed 30 ms wait does not
guarantee remote cleanup. Executing syscalls and commits remain protected by
their journal lock. Open/scan and assembly cancellation barriers stay open.
No dependency, wire type or wave status changed. The final relay-control matrix
will revalidate this correction together with its pending network changes.

## Shared relay control framing and grace

Before-fix tests reproduced the missing Drain notice and socket-drop attachment
leak. One bounded exact codec now covers both ends and every control variant.
Notices use owned bounded groups and lock/write deadlines; partial failure
closes its stream's connection. Control reading shares the server connection
future. Socket destruction cancels pumps, while explicit close joins them.

Actual Drain receipt preserves grace-period datagrams. Replacement ownership
protects flow history and subsequent PeerGone/Pong framing. The first full run
exposed the independently corrected queued-sync-store issue above; its failure
is retained in evidence. The final rerun passed formatting, three Clippy lanes,
**329 workspace tests**, **118 all-feature network/agent/CLI/relay tests**
and **3 isolated codec tests**. See [contract](relay-control.md) and
[receipt](reports/rds-relay-control-20260925.md). Only existing noq was added as a
test dependency; no new package or runtime helper was introduced.

R08 notice receipt is corrected; W3.3 remains partial for warm failover and
SSH/video/sync interruption acceptance. Global relay admission/queues and
server task ownership remain W2.5. R09 initial candidate racing is next; native
macOS and real network qualification stay open. No remediation wave is closed.

## Initial candidate race and consistent socket families

Two before-fix failures confirmed that the first silent address blocked a healthy
second direct candidate or attached relay. The owned dialer now races bounded
authenticated attempts under one deadline and owns loser cancellation. IPv6
testing additionally exposed first-socket family inference and fatal unroutable
QNT probes; the mux now maintains mapped/native address consistency, while
unsupported candidates are filtered before the cap and policy path opens.

Focused validation passed 14 tests. Final formatting, three Clippy lanes,
**337 workspace tests**, **128 all-feature network/agent/CLI/relay tests**
and **22 isolated owned-network tests** passed. A 20/20 loopback handshake
smoke run is included, without WAN/parity claims. See [contract](candidate-dialing.md)
and [receipt](reports/rds-candidate-dial-20260925.md). No dependency or runtime
helper changed.

W3.1/W3.6 remain partial for relay bootstrap, scoped interface discovery, validated
path selection and transport-failure isolation. Requested ALPN narrowing is the
next bounded protocol correction. No remediation wave or deployment is closed.

## Exact requested transport protocol

Three before-fix real-QUIC failures confirmed that the owned backend could ignore
the requested ALPN. Immutable exact-protocol configurations now serve each dial
and its candidate attempts, while preserving the endpoint-wide TLS cache bound.
Repeated alternating and concurrent protocol requests, refusal of unsupported
protocols and an immediately invalid first address are tested.

Focused validation passed 16 tests, including existing backend interoperability.
Final formatting, three Clippy lanes, **341 workspace tests**, **132
all-feature network/agent/CLI/relay tests** and **26 isolated owned-network
tests** passed. See [contract](protocol-negotiation.md) and
[receipt](reports/rds-alpn-selection-20260925.md). No dependency or wire version
changed. W2.2 remains partial for capability/limit/version negotiation and
session/transfer IDs. Validated path selection is next. No wave is closed.

## Validated application path eligibility

The before-fix silent-path test observed premature Available status. The policy
now seeds only the authenticated handshake, subscribes before explicit path/QNT
work, and admits extra paths to selection on Established. A real validated
secondary carries a datagram after logical primary close. A deterministic slow
primary remains preferred over a silent candidate with a lower default RTT.
The QUIC engine's existing prohibition on unvalidated payload is unchanged.

Four focused real/simulated cases passed. Final formatting, three Clippy lanes,
**344 workspace tests**, **135 all-feature network/agent/CLI/relay tests**
and **25 isolated owned-network tests** passed. See [contract](path-selection.md)
and [receipt](reports/rds-path-selection-20260925.md). No dependency or wire
version changed.

The next bounded correction is owned retry when extra-path connection IDs have
not yet arrived. Event loss, complete metrics, physical failover, native macOS
and service recovery remain open. No remediation wave is closed.

## Bounded retry for delayed path credits

A failing-before simulation proved that the automatic secondary attempt was
lost while a later manual attempt on the same listener validated. The policy
now owns bounded credit retry, candidate-address reconciliation and source-aware
withdrawal without retaining strong I/O handles across awaits. Five queue unit
cases and two delayed-link regressions cover backoff, bounds, replacement data
and cancellation. The focused run passed 32 tests.

Final formatting, three Clippy lanes, **351 workspace tests**, **142
all-feature network/agent/CLI/relay tests** and **32 isolated owned-network
tests** passed. See [contract](path-selection.md) and
[receipt](reports/rds-path-credit-20260925.md). No dependency or wire version
changed. Initial family fairness is next; lost path events, metrics, transport
failure isolation and physical/native-platform qualification remain open.
No remediation wave is closed.

## Fair direct-candidate family budget

A failing-before real regression confirmed that eight supported silent IPv4
addresses excluded a healthy IPv6 listener. Canonical per-family selection now
alternates under the existing cap and fills spare slots, while retaining native
IPv6 scope IDs. Four unit cases cover selection, aliases and filtering. The
focused library/candidate/simulation run passed 36 tests.

Final formatting, three Clippy lanes, **356 workspace tests**, **147
all-feature network/agent/CLI/relay tests** and **43 isolated owned-network
tests** passed. See [contract](candidate-dialing.md) and
[receipt](reports/rds-candidate-families-20260925.md). No dependency or wire version
changed. Relay failure isolation is next; global/progressive dialing, lost path
events, complete metrics and physical/native-platform qualification stay open.
No remediation wave is closed.

## Relay route loss preserves direct traffic

Two failing-before real cases showed that a closed relay tunnel or unknown relay
peer mapping could disrupt a connection with a healthy direct path. RelaySender
now treats these conditions as loss on the relay route; a weak diagnostic handle
reports local tunnel availability. The failure regression checks 25 direct
roundtrips and bounded shutdown after actual relay closure. A second test adds
a missing mapping later and validates that same pending path. The focused relay
suite passed 16 tests.

Final formatting, three Clippy lanes, **356 workspace tests**, **149
all-feature network/agent/CLI/relay tests** and **35 isolated network/relay
tests** passed. See [contract](relay-control.md) and
[receipt](reports/rds-relay-failure-20260925.md). No dependency or wire version
changed. Generic child I/O failure/shutdown, peer-table and queue bounds, warm
replacement, complete metrics and physical/native-platform qualification remain
open. No remediation wave is closed.

## Explicit shutdown after failed protocol I/O

A terminal UDP send failure reproduced endpoint-close stalling when Connection
and Path handles remained alive. The endpoint now signals tracked policy tasks
to directly close weakly referenced connection state before exiting. Admission
is sealed first, subscriptions still precede spawn and no strong I/O handle
crosses an await. The focused lifecycle/simulation run passed 15 tests.

Final formatting, three Clippy lanes, **357 workspace tests**, **150
all-feature network/agent/CLI/relay tests** and **40 isolated owned-network
tests** passed. See [contract](path-selection.md) and
[receipt](reports/rds-failed-driver-shutdown-20260925.md). Generic transport
recovery, relay queue/peer ownership, global resource bounds and physical/native
platform qualification remain open. No remediation wave is closed.

## Bounded client relay state

A real before-fix synthetic alias collision redirected a raw transport fixture
to the second identity. Registration now refuses collision and preserves the
first owner. Positive configurable peer/queue limits, metadata leases through
last-stream lifetime and whole-packet drops bound client relay state. Full peer
tables preserve direct dialing and acceptance; relay-only admission fails fast.
The focused run passed 52 tests across seven targets.

Final formatting, three Clippy lanes, **364 workspace tests**, **161
all-feature network/agent/CLI/relay tests** and **52 isolated network/relay
tests** passed. See [contract](relay-control.md) and
[receipt](reports/rds-relay-bounds-20260925.md). Server task ownership/admission,
global resource qualification, warm replacement and physical/native-platform
acceptance remain open. No remediation wave is closed.

## Owned relay lifecycle and tunnel health

Real before-fix cases showed live tunnels after Relay Drop and an indefinitely
draining server after caller cancellation. The owned runner now bounds admission,
owns and joins session workers and keeps drain cleanup independent of callers.
Registration guards clean canceled sessions and history; notice futures are inline.
The shutdown campaign also reproduced stale RTT selecting a dead relay over a
working direct path. A shared availability watch now retires known failed relay
paths and withdraws failed advertisements/candidates without retaining I/O.

The focused run passed 73 tests. Final formatting, three Clippy lanes,
**364 workspace tests**, **167 all-feature network/agent/CLI/relay tests**,
**73 isolated network/relay tests** and **30 failure-isolation repetitions
(60 test executions)** passed. See [contract](relay-control.md) and
[receipt](reports/rds-relay-lifecycle-20260925.md). Owned server CLI integration,
global resource qualification, full path recovery and real service/platform
acceptance remain open. No remediation wave is closed.

## Nonzero virtual relay ports

A public deterministic identity reproduced Noq refusing a relay-only dial because
its synthetic hash port was zero. Zero now maps to virtual port one; existing
nonzero aliases and endpoint identities are unchanged. Real relay-only sockets
without direct children exchange pinned reliable streams in both directions.
The focused run passed 59 tests.

Formatting, three Clippy lanes, **364 workspace tests**, **111 all-feature
network/relay tests** and **59 isolated network/relay tests** passed. See
[receipt](reports/rds-relay-zero-port-20260925.md). Broader alias design,
mixed-version/physical/native-platform qualification and owned server runtime
integration remain open. No remediation wave is closed.

## Persistent identity transactions

The shared key loader now publishes complete private seeds atomically, without
replacement, under a stable directory lock. It bounds reads and lock wait, rejects
unsafe existing files, and recovers its exact pending state after process exit.
The raw format and default path remain; strict permissions and the typed error
return are documented compatibility changes. CLI/agent use blocking workers.

Real old-code tests reproduced symlink/nonprivate/hardlink acceptance. New tests
cover concurrency, Busy retry, seven returned-error/process-exit boundaries, strict-umask
initialization recovery, explicit directory unlock and path/publication races. Formatting, three Clippy lanes, **390 workspace**,
**194 all-feature net/relay/agent/CLI** and **36 isolated tests** passed.
See [receipt](reports/rds-identity-storage-20260925.md) and
[contract](identity-storage.md). This is a W2.4/W3.2 prerequisite; local IPC,
platform/physical qualification, directory shutdown and owned server runtime
integration remain open. No remediation wave is closed.

## Directory task ownership and shutdown

Directory close now seals admission and joins bounded connection, request-worker
and maintenance groups, including fallback after runner failure. Canceling one
close waiter preserves handles and the runner result. HTTP timeout retains
started disk work and its budget. Drop requests cleanup; explicit close awaits it.
The server awaits directory and relay shutdown together and closes the directory
on relay startup failure. Unix signal registration precedes final readiness.

New tests cover real HTTP/TLS, blocked storage and maintenance, canceled and
concurrent close, queued job cancellation, worker/runner panic and real server
SIGTERM/restart/error paths. Formatting, three Clippy lanes, **400 workspace**
and **194 all-feature net/relay/agent/CLI tests** passed. See
[receipt](reports/rds-directory-lifecycle-20260925.md) and
[contract](directory-lifecycle.md). Started filesystem calls remain nonpreemptible;
startup cancellation, full server preflight, owned CLI runtime and platform
qualification remain open. No remediation wave is closed.

## Checked owned relay shutdown

Owned Relay close/drain now return a typed retained runner failure after cleanup,
rather than only logging it. The Relay shares its connection task set with the
runner, so abnormal exit preserves join ownership. Saved outcomes and serialized
fallback remain valid after canceled, concurrent and repeated close calls.
The existing admission and five-second connection cleanup grace are unchanged.

A real attached-tunnel fixture checks runner failure, two fallback cancellation
points, child release, identical retained failure and zero occupied cleanup
state. Existing owned-relay tests now inspect shutdown results. Formatting,
three Clippy lanes, **400 workspace** and **195 all-feature
net/relay/agent/CLI tests** passed. See the
[receipt](reports/rds-relay-checked-shutdown-20260925.md). This is an explicit
library API change and a runtime-composition prerequisite. Owned binary
integration, platform and operational qualification remain open. No wave closes.

## Shared relay binary runtime

Both relay/server binaries now select the owned QUIC relay with a separate
persistent identity, explicit production allowlist and positive connection cap.
Open owned mode requires a development flag; iroh remains the default. Shared
preflight rejects unused/backend-incompatible flags and bounded malformed PEM
before state creation; host registry/rotation/TLS input parsing also precedes
key/catalog initialization. Both services join shutdown and propagate failure.

Actual-process fixtures check authenticated relay-only stream/datagram exchange,
unknown-peer denial, capacity recovery, explicit development drain, identity
persistence and signed-directory reuse across restart. **17 owned and 13 default
focused tests**, formatting, three Clippy lanes, **413 workspace** and
**216 all-feature net/relay/agent/CLI/server tests** passed. See the
[contract](relay-runtime.md) and [receipt](reports/rds-relay-runtime-20260925.md).
CI includes owned-server tests on both OSes; local results are Linux only.
Malformed-frame work accounting, service supervision, native macOS and network/
service qualification remain open. No wave closes.

## Relay datagram work admission

A real QUIC regression failed because empty, short and invalid-key frames did
not touch the source bucket. Every datagram now pays at least 1024 units before
validation/routing. Larger frames retain byte charging under the existing
numeric steady/burst limits. Raw-byte lookup against the authenticated table
removes per-packet curve decompression without admitting unknown identities.
The existing cooperative yield and synchronized source/history ownership stay.

The formerly failing case and **39 focused relay tests** pass. Fixed-time
unit tests verify finite small-frame bursts, refill/cap and larger-frame cost.
Formatting, three Clippy lanes, **413 workspace** and **219 expanded
all-feature tests** passed. See [contract](relay-control.md#datagram-work-admission)
and [receipt](reports/rds-relay-accounting-20260925.md). Global ingress/decryption
limits, reattachment/control budgets, service supervision, performance and
platform/network qualification remain open. No wave closes.

## Relay and directory runner supervision

Both binaries observe service termination as well as signals; the composed host
stops both services and exits unsuccessfully on unexpected completion, even a
clean return. Observers preserve runner handles/results across cancellation and
repetition. Directory failure observation precedes fallback disk joins, and the
iroh adapter retains the consumed supervisor result for safe later shutdown.
Combined startup/cleanup failures retain both component causes.

**74 focused tests**, formatting, three Clippy lanes, **417 workspace**
and **224 expanded all-feature tests** passed. Real fixtures check healthy
listener survival, concurrent close, actual runner abort/panic and the upstream
iroh supervisor-error path. See [contract](relay-runtime.md) and
[receipt](reports/rds-runtime-supervision-20260925.md). Hung-task detection,
automatic recovery, kernel-I/O shutdown deadlines and platform/network/service
qualification remain open. No wave closes.


## Observed path telemetry

Real regressions reproduced stale path-zero selection after migration and direct
labels on relay-only traffic. Noq policy now shares validated weak path metadata
with the facade, without guessed ID scans. The API and CLI expose observation
coverage, sticky event loss and observer liveness; unknown selection does not
fall back to historical traffic. Metrics keep RTT/cwnd/validity together, expose
coverage, and retire path baselines with observed paths.

A 70-path actual transport campaign covers high IDs, migration and real bounded
broadcast overflow. Both owned binary restart fixtures check relay-only byte
buckets. Formatting, three Clippy lanes, **419 workspace** and
**226 expanded all-feature tests** passed using fresh isolated build
artifacts after a build-directory interruption. See [contract](path-telemetry.md)
and [receipt](reports/rds-path-telemetry-20260925.md). Full validated inventory
reconciliation, lossless retirement metrics, weak sampler ownership, generic
socket failure isolation and platform/network qualification remain open. No
wave closes and no throughput/latency improvement is claimed.

## Local mux child failure isolation

Real QUIC regressions reproduced shared-driver failure after one child socket
error. Shared monotonic child health now isolates that failure, wakes independent
send/receive/policy waiters, withdraws failed advertisements and excludes failed
observed paths. Packet-scoped UDP errors preserve the socket; bounded retry
prevents receive spinning. A further failing test fixed reselection of a failed
last path that the engine refused to close. All-child loss closes held
connections and releases policy tasks without requiring explicit endpoint close.

Five poll-level tests and three real transport cases cover ownership, source
routing, open-stream continuity, datagrams, QNT withdrawal, fresh connection
admission and terminal cleanup. The transport cases passed 30 repetitions.
Formatting, three Clippy lanes, **427 workspace** and **234 expanded
all-feature tests** passed. See [contract](socket-failure-isolation.md) and
[receipt](reports/rds-mux-isolation-20260925.md). Full validated inventory,
socket/interface recreation, relay bootstrap/replacement, native macOS and
physical-network qualification remain open. No wave closes.


## Weak metric sampler ownership

Regressions on both transport backends reproduced idle samplers retaining
connection I/O after the application dropped its last facade. Samplers now own
weak backend observations and metadata only. Closure events wake their loop
independently of the interval; actual streams remain I/O owners. Closed iroh
handles expose no live paths. A last-selected sample is invalidated only by
its own sampler drop, preserving a newer connection's observation. The bench
keeps its one-shot observer through the scrape.

**15 focused tests**, formatting, three Clippy lanes, **431 workspace**,
**238 expanded all-feature** and **2 isolated iroh-only lifecycle tests**
passed. See [contract](path-telemetry.md) and
[receipt](reports/rds-sampler-lifecycle-20260925.md). Full path reconciliation,
lossless metrics, receiver-confirmed throughput measurement, native macOS and
physical-network qualification remain open. No wave closes.


## Shared process telemetry and real collector qualification

The Rust `rds-observe` leaf owns all five binary entrypoints: bounded stderr,
text/private or schema-1/allowlisted JSON, process IDs, numeric session context,
heartbeat, readiness and typed connect/handler outcomes. Arbitrary diagnostic
fields and terminal errors are never formatted in JSON mode. Invalid logging
configuration fails before product initialization. Stdout remains command data.

Producers never wait for queue capacity or network delivery. The output adapter
bounds records to 4096 bytes and its queue to 1024 entries; shutdown seals and
drains admission with a 500 ms wait. Tests cover saturation, write/flush errors,
blocked output/destruction, concurrent shutdown admission and secret canaries.
Loss is observable but delivery is best effort, including final shutdown.

Vector independently projects the schema and forwards JSON plus low-cardinality
Prometheus metrics to OpenObserve. Exact image digests, bounded buffers, private
secret files and three disabled alert definitions accompany a disposable Compose
fixture. Its real test checks projection/rejection, queried logs, controlled
counter/histogram values, alert predicates, local webhook delivery and recovery
after a collector restart during backend downtime. A password with quotes and
backslashes exposed Vector's raw substitution hazard; the checked configuration
uses a pre-encoded Basic header from the secret backend. No credentials or human
notification destinations are committed or contacted by the fixture.

Formatting, default/X11/all-feature workspace Clippy, **444 workspace tests**,
**239 expanded all-feature tests** and the **real pipeline regression** passed.
The latter also runs Vector validation and five projection tests. Two 100-probe
same-host direct ping runs completed with JSON telemetry enabled and zero loss
counters in the collected records. They are debug smoke measurements, not an
A/B overhead study, connection-establishment timing or physical network evidence.
See the [contract and O1–O6 sequence](observability.md) and
[dated receipt](reports/rds-observability-20260925.md).

At the O1 receipt, W10.1/W10.2 remained partial. Secured admin/source metrics
were the next increment (recorded below); detailed phase/reason correlation,
exact-build diagnostics and redacted bundles, private estate rollout,
expected-instance monitoring, independent failure probes, long-run
resource/latency measurement and native macOS checks remain open.
Existing SSH/desktop/sync/transport remediation gates are unchanged. No wave
checkpoint, remote CI run, production activation or release readiness is claimed.


## Authenticated source metrics and local agent readiness

O2 adds a disabled-by-default Rust loopback admin listener to agent, relay and
composed server. Paired address/token flags are validated before product state
creation. `rds admin-token` creates a private credential without endpoint
identity or secret output. Only authenticated bounded `GET /metrics` requests
invoke aggregate source snapshots; proxy headers do not authorize. The old
public `/v1/metrics` returns 404 even behind a loopback proxy, and stable writer
hash labels are removed.

Agent, directory and owned relay export admission/task, transport, grant/lease,
request/publication/GC and forwarding/history observations. Weak references or
counter-only handles preserve I/O teardown. Busy/unavailable values are explicit;
upstream-relay forwarding coverage is absent. Transport snapshot export no
longer waits for its selected-sample lock, demonstrated by a failing-before
contention regression. The corresponding stored observation survives a busy
scrape.

Default-backend daemon testing also found an unbounded iroh `online()` wait in
agent startup with relays disabled. Local service/admin supervision now starts
after endpoint bind independently of relay availability. A real process with a
nonresponsive relay serves an allowed direct ping, exports its accepted counter
and joins SIGTERM shutdown. Directory announcements follow address changes;
initial tickets and local readiness are not remote reachability guarantees.

The optional Vector fragment loads the private bearer credential from its
secret directory, disables source proxying and projects only opaque labels.
Its real fixture queries exact source counters in OpenObserve and verifies an
owned proxy receives no scrape. Existing logs/metrics/alert/restart checks also
run. A fixture storage-capacity mismatch was diagnosed and repaired without
reducing the production buffer limit.

Formatting, default/X11/all-feature workspace Clippy, **466 workspace**,
**251 expanded all-feature** tests and the **real Vector/OpenObserve pipeline**
passed. A final workspace rerun and focused observer Clippy followed the
platform-specific test fixture split. `rds-observe` all targets/features also
pass an **Apple Silicon cross-check**, without claiming native execution.
Two 100-probe debug ping measurements completed with JSON telemetry and zero
observed local loss/error counters. Admin is inactive in those bench worlds;
these results do not measure admin overhead, setup latency or WAN behavior.

One workspace run timed out in the existing path replacement test, while the
same binary passed isolated/paired repetitions. The fixture now establishes
application policy's observed replacement path before closing the primary;
post-fault budgets are unchanged. No unique root cause or production failover
fix is claimed. See the [dated receipt](reports/rds-admin-metrics-20260925.md)
and [contract/remaining sequence](observability.md#next-reviewable-increments).

Detailed durable inventory/policy metrics, finer queue coverage, upstream relay
observations, phase/reason correlation, support bundles/dashboards, private
rollout and independent liveness remain O2–O6. Native macOS execution, physical
network/resource/failure campaigns and all open product/release gates remain.
No wave closes and no production service was activated.


## 2026-09-25 — durable catalog and policy observations

W10 / O2 increment; no wave closure. Rust transaction owners now publish
metadata-only catalog counts/capacity/generation and policy epoch/revision,
aggregate entries and bounded lease freshness. Memory/custom backends, busy
owners, expired leases and failed commits have distinct observations. Exporters
retain no store/file locks and perform no disk reads or transaction locking.
Agent revocation revision and TTL follow its effective watched admission value.

The [receipt](reports/rds-durable-metrics-20260925.md) records verification and
limits; [the contract](observability.md#durable-catalog-and-policy-observations)
explains each gauge, failure/recovery and clock semantics. Existing cryptography,
QUIC/TLS, redb and Vector/OpenObserve boundaries remain; no new dependency or
persistence format is introduced.

Linux verification passed: formatting, default/X11/all-feature workspace Clippy,
472 workspace tests (2 ignored), and 211 expanded tests (1 ignored, overlapping).
The Apple Silicon discovery cross-check stopped in `ring` C compilation: default
Linux compiler rejected Apple flags; explicit Clang lacked a usable Apple SDK.
This is retained as failed environment qualification, not a macOS pass.

Remaining: finer queue/task and upstream relay coverage (O2), phase/reason
correlation (O3), bounded diagnostic bundles/dashboards (O4), private rollout and
independent delivered alerts (O5), platform/resource/latency campaigns (O6).
Native SSH/PTY, desktop capture/viewer, sync completion, transport recovery and
release qualification retain their W0–W10 gates. This increment does not establish
complete product readiness or connection latency improvement.

## 2026-09-25 — agent-owned local device sessions

W2.4 partial: a new shared Rust client crate hosts an opt-in local manager on the
agent's existing endpoint. Keyless CLI commands connect/list/select/disconnect,
Ping/Info and forward SSH/TCP without another identity or UDP bind. Same-UID IPC,
private socket ownership, bounded reservations/workers, credential-aware reuse,
pinned forwarding and authenticated aggregate metrics have regression coverage.

Two failure cases drove explicit TCP direction-FIN framing: abandoned silent
bodies exhausted all slots, and an OS-readiness workaround lost a late upload
after reverse half-close. The final adapter preserves both half-close directions,
queued data and drop cancellation with bounded buffers and owned tasks.

Final Linux checks: formatting; default/X11/all-feature Clippy; 484 workspace
tests (2 ignored); 96 expanded tests (0 ignored, overlapping).
See [contract and remaining sequence](local-sessions.md) and
[receipt](reports/rds-local-sessions-20260925.md). No wave is closed. Default
identity ownership/migration, native SSH/PTY, desktop rendering/input switching,
GDS lifecycle and real device/network/operational qualification remain open.

## 2026-09-25 — default managed connectivity and runtime identity ownership

Ordinary CLI ticket/ping/info/SSH-forward commands now use the running local
agent. Agent control defaults to a dedicated directory beside its key; explicit
paths and server-only `--no-control` remain available. Managed commands never
fall back to an independently bound endpoint. Existing sessions stay pinned and
are reused by concurrent CLI processes. This increment introduced local IPC v2;
the later [grant renewal increment](grant-leases.md) requires v3. The iroh default
remains unchanged.

Agent, explicit direct CLI and owned relay acquire the validated seed inode's
exclusive runtime owner before network bind. Pure identity readers still work.
The guard survives awaited shutdown, operation failures and owned-relay canceled
drain cleanup; SIGKILL recovery uses OS lock release and existing stale-socket
recovery. This is a cooperative inode guarantee, not fencing of copied keys or
old binaries. Direct mode remains necessary for unfinished viewer/sync APIs.

Shared grant-file loading also now uses nonblocking no-follow regular-file
validation, closing a reproduced FIFO startup hang. The CLI adds a direct
dependency on the existing locked rustix library; no package/version was added.

See the [receipt and final source/check evidence](reports/rds-identity-migration-20260925.md).
W2.4 remains partial. Next: viewer/sync manager APIs, maintained Rust SSH/PTY,
interactive desktop/input switching, installed-device migration and platform/
network qualification. GDS lifecycle, recursive sync and O2–O6 observability
work retain their separate gates. No deployed service or release gate is closed
by these local code and regression checks.

## 2026-09-25 — native Rust SSH client and standard OS PTY

W5.1/W5.3/W5.5 partial: `rds-ssh` reuses russh 0.63.3 for SSH, while the
remote server/OS owns account isolation and PTY allocation. Both CLI SSH
commands now open a native shell/exec session over a pinned managed or explicit
direct stream. Existing TCP listener behavior is available through `forward`.
Explicit host pins, a named account and one key/agent identity are required;
there is no trust-on-first-use, implicit credential fallback or agent forwarding.

Positive channel/PTY/exec acknowledgements, bounded setup and cancellation,
concurrent input/output, complete exit/output draining, resize and local terminal
restoration have regression coverage. A tiny-buffer bidirectional deadlock
reproducer led to a bounded transport bridge sized above the advertised receive
window. The bridge owns the actual stream independently of upstream task drop.
No command replay or reconnectable PTY semantics are claimed.

Fixed SSH operation outcomes/timings enter the Rust/Vector schema. Remote stderr
cannot be trusted as a log envelope: JSON SSH now requires a separate exclusive
private file, with an 8 MiB process cap. File telemetry remains live during a
terminal session; console logging uses an acknowledged pause/restoration barrier.
Tests inject a forged telemetry event and verify it stays only in command output.

See the [contract and migration](ssh.md) and the
[final validation receipt](reports/rds-ssh-20260925.md), including actual OpenSSH
interoperability, default/feature-expanded checks and frozen source hashes.
Final Linux checks pass: formatting; default/X11/all-feature Clippy; 513 default
and 560 feature-expanded tests (3 ignored each, overlapping); the separately
invoked OpenSSH test; cargo-deny; Vector validation and seven projection tests.
All 225 source/configuration hashes remain unchanged across the final sequence.
The existing remote SSH server is an explicit dependency. The new library reuses
the existing ring crypto boundary; no all-Rust native-object claim is made.

Next W5 steps, in dependency order: verified GDS host-key provisioning/rotation
and account/command capabilities; a Rust OS account broker with proven privilege
separation; authorized PTY survival/reattachment with bounded history; native
macOS, lease/revocation, impaired-network and mixed SSH/video/sync qualification.
Viewer/sync manager APIs and the other W0–W10/O2–O6 work retain their gates.
No wave, deployment or complete-system readiness is closed by this increment.


## Destination-bound grants and explicit live renewal

W2.3 remains partial. Two negative tests first reproduced missing signature
protocol separation and acceptance of an inverted signed interval within clock
skew. Grant v2 now validates the domain, version, exact bounded payload, interval,
subject and actual serving endpoint audience. A random issuer nonce identifies
one authorization; its stable ID survives positive, increasing lease revisions.

The connection owns one replay reservation and watchdog. Same-scope renewal
extends its wall/continuous-clock lease without reconnecting or interrupting
TCP bodies; duplicate requests retain the original deadline. Existing expiry
and revocation remain enforced while a candidate reply stalls. Revocation of the
initial ID closes the renewed connection. Invalid scope/identity/revision changes
close only the affected connection, with an independent control peer kept live.

A two-slot agent fixture proves that a held TCP body does not consume renewal
capacity: grant mode reserves one of the existing task slots for control.
Invalid one-slot grant configuration fails before identity creation or bind.
Response FIN follows authorization commit; local control EOF follows manager
publication. These barriers prevent immediate successive renewals from racing a
pending predecessor. Cancellation before completion releases the pinned session.

`rds session renew` requires an explicit session and grant file, updates the
credential digest, and preserves device selection and open bodies. IPC v3 and
grant v2 require coordinated migration. Two fixed authorization operation names
join the existing telemetry/Vector schema without exporting grant contents.
The [contract](grant-leases.md) and [dated receipt](reports/rds-grant-leases-20260925.md)
record final evidence and remaining qualification. Final Linux checks passed:
formatting, default/X11/all-feature Clippy, 529 default and 578 expanded tests
(three ignored per overlapping run), separate OpenSSH interoperability,
cargo-deny, Vector validation and nine projection tests. All 226 executable
source/configuration hashes stayed fixed. The expanded run also exposed a
relay-process fixture race; the corrected test waits for observed client closure
under a deadline while retaining the Drain assertion. Both relay host variants
and the final full matrix passed.

Next, in dependency order: tenant/policy and per-resource authorization scopes;
the GDS issuer/client API and automatic renewal/reconciliation; SSH host/account
provisioning; viewer/sync manager operations; native macOS, suspend, physical
network and mixed-load acceptance. Existing W0–W10 and O2–O6 tasks remain open.
Scope reductions require explicit revocation and new authorization today. No
wave-close, benchmark latency result, installed-agent update or deployment is
claimed by this increment.

## Directional grants and truthful input acknowledgment — 2026-09-26

Baseline `a3cdbd036b54d4cd2d4c1d17fd3ca59a38f5288c`. The
[scope increment](reports/rds-service-scopes-20260926.md) implements `SyncRead`
and `SyncWrite` before filesystem access, and `DesktopView` plus the optional
`DesktopControl` input modifier. Legacy broad grants retain their semantics.
Tags 6–9 are appended; grant v2 and IPC v3 stay unchanged. Old decoders reject
unsupported scopes, and renewal cannot change the signed service list.

Sync admission now owns its exclusive slot across the hello ACK and body;
early errors and cancellation cannot leave a live connection permanently busy.
View-only never invokes an input sink. Controlling sessions create one lazy,
bounded blocking worker, reuse its backend and acknowledge only successful
injection results. Dropping it discards queued input and releases the sink after
any in-progress native call returns; it cannot undo that call. Native
screen/seat/focus isolation, key mapping and held-key release remain W6.4.

Linux validation passed fmt, default/X11/all-feature Clippy, **535 default** and
**585 all-feature** tests, an explicit OpenSSH interoperability test and
cargo-deny. Both workspace runs have three opt-in cases ignored; OpenSSH was
run separately. The 228 source/configuration hashes stayed fixed. No Vector
configuration changed and the full ingestion/alert pipeline was not rerun.

Next: tenant/policy revision and per-path/account scope binding; authenticated
GDS issuance, durable renewal and desired-policy reconciliation; managed
viewer/sync integration; native platform and real network/resource acceptance.
This is a partial W2.3/W4.4, W2.5 and W6.4 increment. All waves remain open;
no installed process, issuer or deployment was changed.

## Native preview packaging — 2026-09-26

Partial W10.3/W10.6 increment. Release assembly now builds four Rust binaries
on Ubuntu 24.04 x86_64 and macOS 15 arm64 in PRs and on `main`, before the tag
publication path. Compiler `1.98.1`, Cargo.lock, features, source commit, target
and binary digests are bound; tracked source includes `ops/observability`.
Twelve synthetic positive/negative contract tests cover drift, wrong tags,
missing/extra assets, special files, tampered bytes and empty source SBOMs.
The CLI identifies itself as `rds` in `--version`, matching its installed name.

Real macOS CI found an unguarded Linux-only FIFO fixture in the SSH test.
Special-file refusal now runs with a Unix socket on both platforms and a FIFO
on Linux. The next macOS run exposed XNU's kernel-managed `PENDIN` flag on
canonical-mode restoration: the test now compares all configurable modes and
control characters, excluding only that transient macOS queue-state bit.
See [XNU ttioctl](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/tty.c).
XNU also exposes read-only `FWASWRITTEN` through `F_GETFL` after output; only
that kernel-history bit is excluded on macOS, with `O_NONBLOCK` and every other
descriptor flag still checked. See [XNU flags](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/fcntl.h).
The local six active SSH integration tests pass. A new native CI run
is required to establish macOS acceptance, rather than assuming portability.

The [release contract](releases.md) declares an engineering preview with no
desktop features in the packaged executables. Publication requires exact-commit
default-branch checks, complete packages and official artifact attestations;
source SPDX coverage is not presented as a complete native binary/OS SBOM.
No installed process or consumer pin is changed. W10.3 remains partial until
actual published-asset verification and clean-machine qualification; automatic
activation/rollback and Apple signing/notarization remain open.

The canonical GDS anchor now points to existing Rust workspace entrypoints and
declares numeric GitHub releases with actual provider check names. Its pinned
0.9.15 projection was regenerated and verified through journaled GDS operations;
generated policy content did not change. GDS `complete` still cannot integrate
GitHub PRs, so that route remains `NOT_PROVEN`; ordinary GitHub PR integration
and the checked-in publication workflow retain their separate evidence.
Estate CI-feedback reconciliation and the rest of W10.8 remain open.

The native macOS default workspace subsequently passed. Its additional owned
transport lane exposed a missing wake in the socket-fault fixture: changing a
pending receive into an error did not notify its task, and a QUIC ping could
travel on the healthy sibling instead. The fixture now registers before checking
fault state and wakes when enabling receive failure; the test no longer relies
on a packet or incidental OS wake. All three mux-isolation tests pass locally
with unchanged deadlines. CI uses `--no-fail-fast` within each test invocation
to report all failing targets while still returning failure. The final local
default workspace run passed 535 tests with three opt-in cases ignored.

Both native bundles and complete PR asset assembly subsequently passed. A later
full macOS test run found two additional issues: wall-clock-only fixture names
could collide across concurrent tests, and closing an admin TCP socket with
unread over-limit input could reset the peer before it received the HTTP error.
Parallel scratch factories now include an atomic sequence and sync tests refuse
an existing directory. Admin response teardown now half-closes then discards a
bounded tail (8 KiB / 100 ms within the existing two-second request budget), as
specified by RFC 9112 §9.6. A regression keeps the client write half open and
sends another request after response EOF, requiring bounded slot release and
exactly one source invocation. No extra request is parsed and header/body
admission limits are unchanged. Final native CI remains required after this
runtime change.

The subsequent macOS run passed the HTTP teardown checks, but exposed another
shared CLI scratch factory without a sequence and an over-specific mux fixture
assumption. The shared CLI factory now uses an atomic sequence too. The mux
fixture waits for both policies to observe a validated sibling between the
actual IPv6 socket addresses, allowing QNT's concurrent path creation; it also
holds the actual selected sibling for the final-socket fault. Selection, stream
continuity, advertisement withdrawal and shutdown assertions retain their
deadlines. The source archive now includes root `tests/` and `examples/`, required
by the crates' compile-time fixture includes; the archive contract checks this.

PR integration exposed a separate release-gate issue: CodeQL workflow success
can coexist with unresolved scanner findings. The reviewed PR findings were
synthetic test grants, non-cryptographic Ping echo values, test-only diagnostics
and variable uses inside assertion macros; each disposition is recorded in
GitHub without disabling a query. Publication now additionally requires the
latest Rust/Actions analyses on main to bind the exact source revision, have no
extraction warnings/errors, and leave no unresolved code-scanning alerts. API
pagination is exhaustive. Negative tests reject green workflows with open alerts,
foreign/stale/duplicate/incomplete scans, later-main evidence and a late-page
alert. This strengthens release qualification without changing Rust runtime code.

## 2026-09-26 — managed single-file transfers (W2.4 / W2.2 partial)

Ordinary `send`/`recv` and selected/explicit `session send`/`session recv` now
reuse the agent endpoint and session identity. The manager delegates to the
existing Rust sync engine, with one transfer per session and eight active
transfers across its outgoing sessions. Local IPC v4 requires a coordinated
CLI/agent upgrade. Local paths are caller-resolved, absolute UTF-8 paths within
the same-UID filesystem trust boundary.

The additive `SyncTransfer` greeting/tag carries a fresh 128-bit ID. Legacy
wire tags stay fixed; old peers reject this new service before file mutation.
Canceled/late chunk streams cannot enter a replacement transfer, and the uni
router bounds and retires its route registrations. Cancellation resets the
transfer's control streams while preserving other TCP/SSH streams. Successful
completion waits for the verified data exchange and control completion. There
is no automatic retry; started filesystem commits may outlive cancellation.
Safe `sync_send`/`sync_recv` duration/outcome records pass both application and
Vector allowlists without paths or file contents.

See [the scope and validation receipt](reports/rds-managed-sync-20260926.md).
This is a reviewed increment, not closure of W2/W8 or product acceptance. Viewer
manager APIs, directory synchronization, platform/installed migration, disk-job
qualification and the remaining remediation gates stay open.

## 2026-09-26 — receiver-verified transfer measurement (W0.2 partial)

The `transfer` scenario no longer stops its timer at sender finish. The local
TCP target now streams the body through BLAKE3 and answers only after payload
EOF with a fixed 40-byte receipt — big-endian byte count plus digest — followed
by response EOF. The sender verifies all three under one operation deadline
that covers connect, OpenTcp, upload and receipt; world startup and endpoint
cleanup carry separate deadlines. Bounded, owned receiver tasks (eight live)
replace the unbounded discard sink, and payload is deterministic
position-dependent BLAKE3 XOF data so duplicated/reordered bytes change the
digest. The report names the scenario `transfer-receiver-ack-v1`, adds
`transfer_verified_bytes` and `transfer_completion_ns`, and keeps historical
sender-finish results incomparable by name. See
[the contract](benchmark-transfer.md).

Regression evidence: `large_send_window_cannot_complete_before_receiver_ack`
failed before the fix (`UnexpectedEof` — the old target never acknowledged);
invalid/missing/truncated/trailing receipts, corrupt/short/reordered bodies and
a receipt without EOF all fail the measurement. End-to-end runs cross real iroh
and noq connections over forwarded TCP.

Local validation: `cargo fmt --check`; workspace clippy with default features
and the owned-transport feature set (`-D warnings`); `cargo test -p rds-bench`
(14 tests including both real-transport receipts). Fixed measurement series on
this tree, debug profile, 32 MiB each: iroh 4.4 MiB/s verified
([iroh](reports/bench-transfer-20260926-iroh.md)), noq 3.7 MiB/s verified
([noq](reports/bench-transfer-20260926-noq.md)). The debug-profile series
needed `--timeout-s 120`; the default 15 s deadline expires mid-transfer under
unoptimized crypto — a harness constraint, not a product defect, and the run
fails closed rather than reporting a partial number. Report metadata does not
yet record the build profile; that is W0.6 scope. W0.2 stays partial:
known-rate calibration, per-phase connect/auth/service timings and
topology/load qualification remain open.

## 2026-09-26 — comparator strictness and r0-evidence gate (W0.4 partial)

`rds-bench compare` refused only missing scenarios and numeric drift before;
a metric present on one side but absent on the other, a failed scenario,
a backend or impairment-profile mismatch, a nonfinite value or a single-sample
percentile all compared silently. Comparison now returns a `fault:*` refusal
for each of those, matched before any number is weighed; the impairment seed
is deliberately outside the profile so different drop schedules of the same
conditions still compare. `--min-samples` bounds percentile claims (default 3;
fewer cannot support one). Nonfinite values are additionally refused at JSON
parse — serde_json rejects `NaN`, and the comparator check protects library
callers. Unit tests cover every refusal plus the equal-profile clean pair;
CLI negative fixtures exercise the same paths end to end.

`scripts/checkpoint.sh` gains the registered `r0-evidence` gate (the first
remediation gate): fmt/clippy/test, the rds-bench suite and a generated
negative-fixture battery whose every entry must make `compare` exit 1. The
c1 noq suite line also gained the `--features transport-noq` flag it always
needed — `--backend noq` fails closed without the compiled backend, so the
gate previously could not produce its noq suite at all. Other `r*` gates stay
unregistered until their waves introduce their checks. W0.4 remains partial:
the comparator now enforces its contract, but gate invocation and historical
report re-qualification under the strict rules are open, and the remaining
W0 tasks (relay bench world, capability matrix, receipt schema) are unstarted.

Local validation: `cargo fmt --check`, `cargo clippy -p rds-bench
--all-targets -- -D warnings`, `cargo test -p rds-bench` (10 tests), the
fixture battery through the built CLI (7 refused, 1 accepted) and
`bash -n scripts/checkpoint.sh`.

## 2026-09-26 — versioned capability matrix (W0.5 partial)

`docs/capability-matrix.md` v1 is now the single source of truth for what
this build claims: every service kind, transport, desktop backend, codec,
platform, policy/discovery surface and bench lane carries an explicit state
(`implemented`/`experimental`/`stub`/`unavailable`) and a separate runtime-
prerequisites column. The header states `matrix_version`, and the doc states
that placeholder states must name what would make them real.

Agreement is enforced rather than asserted: a shared checker under
`tests/support/` parses the matrix and is included by `rds-cli` tests
(every `ServiceKind` variant must have a `service:<kebab>` row; `audio`
must stay `stub` until a codec exists) and `rds-bench` tests (every lane in
`scenario::LANES` must have a `measure:<name>` row, so an emitted report
name always resolves to a declared capability). README links the matrix and
the checker fails if the link is removed. The lane list is now a single
`LANES` constant in `rds-bench`, so `run --scenario all` and the matrix test
share one ordering source.

Remaining W0.5 scope: live `rds info` advertisement ↔ matrix agreement
beyond enum coverage, per-report capability tagging, release-gate
enforcement, and qualification of the experimental rows themselves.

## 2026-09-26 — append-only receipt log (W0.6 partial)

`rds-bench` gains a receipt writer and validator. `rds-bench receipt`
appends one JSON object per line to `docs/receipts/rds-receipts.jsonl`
recording full commit SHA, dirty flag, digests of the bench binary and
`Cargo.lock`, the pinned toolchain channel, enabled features, OS/arch,
topology class, repetitions/failures/skips, explicit budgets, and the
blake3 digest of every cited report. No hostnames, addresses or user
paths are recorded — receipts are reproducible without private host
identifiers.

Every line carries `prev_hash` (the previous line's `hash`, `genesis`
for the first) and `hash` over the canonical JSON. `rds-bench
validate-receipts` replays the whole log: a tampered field, a swapped
line order or a mid-log deletion fails with the offending line number.
Unit tests cover the roundtrip, tamper, reorder, bad-status and
bad-budgets refusals; the writer only ever appends, so history is
linked rather than overwritten.

`write_checkpoint` now appends a `kind:gate` receipt — citing the
checkpoint markdown by digest — immediately after writing it, so every
future gate run leaves machine-checkable evidence. Receipts only record
executed gates; a failed gate appends nothing and validates nothing.

Remaining W0.6 scope: citing each gate's bench JSONs individually,
backfilling receipts for historical reports where their inputs are
still known, per-report capability tagging (W0.5 link) and release-gate
consumption of the log.

## 2026-09-26 — measured-path containment (W0.3 partial)

Every bench world now bounds the peer-path kinds its ticket advertises
instead of trusting the ticket alone. `EndpointConfig::transports` is the
hard bound: `DirectOnly`/`RelayOnly` on iroh physically remove the other
transport (`clear_relay_transports`/`clear_ip_transports`) — required
because iroh's in-band NAT-traversal exchange cannot be switched off
(`max_remote_nat_traversal_addresses` floors at 8) and `observed_address
_reports=false` does not cover candidate exchange. On noq, `RelayOnly`
suppresses direct addrs in `addr()`, dial candidates, in-band QNT
advertisement and learned-candidate path opens while keeping the UDP
socket the owned-relay attachment rides on.

Two real escapes were found and closed. iroh relay-only had opened a
direct path mid-run (integrity check caught 9146B direct under a relay
ticket). iroh direct-impaired was worse: QNT migrated path 0 to the
agent's unproxied address, so the "impaired" suite measured loopback RTT
(~5 ms under configured 50 ms) while counters still showed residual
proxy traffic — the old `forwarded > 0` check could not see it. Integrity
now additionally requires the impairment devices' offered count to cover
every datagram the client sent on the claimed kind, and a configured
one-way delay to surface as a measured path-RTT floor (0 = unobserved,
skipped). Both checks independently detect the fixed escapes.

`max_multipath_paths=1` finally pins on iroh: the transport config floors
the cap at 14, so the backend installs a `PathSelector` (iroh's
visibility-only `unstable-custom-transports` feature) that keeps the
path the handshake established per remote — the first `select` call can
only see the dialed path, and a later clean path is never selected. On
noq the same flag suppresses advertisement and traversal initiation; the
relay-socket helper endpoint is pinned to its bootstrap path, which is
what keeps relay-impaired traffic on the impaired attachment legs.

noq bench worlds now run the owned `rds-relay` server, with a UDP impair
proxy on each endpoint↔relay attachment leg for `relay-impaired`, and
socket-level impairment (`ImpairingSocket` under the whole endpoint) for
`direct-impaired`, which is immune to path migration by construction.
`resolve-connect` runs on noq through the owned relay (directory +
announce + resolve are transport-agnostic). On iroh, `impaired` emits an
explicit `SCENARIO SKIPPED` row for `relay-impaired` — its relay leg is
TCP, outside the UDP impairment model — instead of silently omitting the
lane, and without `transport-noq` the lane is skipped as unavailable
rather than absent.

Verification on Linux, debug profile: `run --scenario all` passes on
iroh (handshake/ping/transfer direct; multiconnect and impaired on
proxied direct; relay-fallback on pure `via="relay"` counters;
resolve-connect 10/10) and on noq (all eight lanes, including
relay-impaired at ~280 ms RTT with ~380 proxied datagrams and
resolve-connect 10/10 through the owned relay). Failed worlds close
endpoints before integrity is enforced, so a rejected measurement no
longer logs ungraceful endpoint drops; `resolve-connect` also closes its
agent endpoint.

Remaining W0.3 scope: recovery/migration scenarios (path loss → relay,
relay → direct upgrade) are not yet lanes; iroh cannot run
relay-impaired at all (TCP relay leg); the iroh pin is selection-level —
an opened-but-unused learned path still exists and is disclosed via
`paths_seen`; real-network and macOS qualification remain open.
