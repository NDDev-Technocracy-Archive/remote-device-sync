# Changelog

## [Unreleased]

## [0.1.3] - 2026-11-08

**Engineering preview.** The standalone viewer now honors an explicitly selected
private directory for its live diagnostics and incident snapshots. Omitting the
option retains the existing automatic private directory. File ownership,
permissions, size and rotation protections remain enforced by the same storage
owner. Controlled visual-marker diagnostics now also initialize for each native
workspace tab through the same validated descriptor path as a single view. Native
capture and wire formats retain the 0.1.2 implementation.

## [0.1.2] - 2026-11-08

**Engineering preview.** This patch corrects the first image of a newly opened
or restored native screen, including a read-only view after capture was idle.

- PipeWire and ScreenCaptureKit retain the last complete native update even
  without an encoder or viewer. A final static desktop change no longer waits
  for pointer input or unrelated later damage before a new viewer can see it.
- Both native callbacks use one bounded latest-image publisher. Obsolete pixels
  are released on replacement/close; a late native completion cannot revive a
  closed owner. Encoding remains demand-driven, with the existing native rate,
  pixel, byte and worker budgets.
- Keys, wire formats, saved workspaces and supported native entrypoints are
  unchanged. Mac hosting still requires normal local Screen Recording and
  Accessibility grants for the installed code signature. Packages retain the
  unchanged-byte promotion and provenance contract from 0.1.1.

## [0.1.1] - 2026-11-07

**Engineering preview.** NDDev Remote is the standalone OpenNetwork remote-screen
product, retaining the RDS engine and history. Numeric releases promote the exact
attested desktop packages built and checked on `main`; publication never rebuilds
the executables or signs the macOS applications again.

- Linux x86_64 packages contain `nddev-remote`, `rds` and `rds-agent` with X11,
  the consented Wayland/PipeWire/EIS host and the native wgpu viewer enabled.
  A Wayland session never silently falls back to capturing Xwayland.
- macOS arm64 packages add ScreenCaptureKit capture and CGEvent control, and
  separate viewer/host applications with stable bundle identifiers. Explicit
  local Screen Recording and Accessibility grants are required. The resident
  manager remains usable while hosting waits for local consent.
- Native capture owners retain bounded buffers, input leases and callback
  drains. Primary displays use protocol slot zero on both platforms. The same
  software producer, scaling and H.264 implementation serves both native hosts.
- NDS integrates through an adapter; standalone remote screens require no NDS
  account, telemetry, synchronization or package-cache service.
- Desktop artifacts bind source, toolchain, features, producer and binary/file
  hashes. Releases include both native archives, macOS apps, exact source,
  source SPDX SBOM, release notes, manifest and checksums. Public macOS packages
  use ad hoc signatures and are not notarized distribution.
- Upgrade the CLI, viewer and resident agent together. Existing endpoint keys,
  local consent and saved workspaces remain owned by each device. Native
  acceptance, WAN recovery and physical presentation are distinct from a green
  build; see [the release contract](docs/releases.md).

- File send/receive now reuse the local agent's identity and selected or explicit
  sessions. Per-transfer uni-stream IDs isolate cancellation and late data;
  limits reserve control capacity and preserve unrelated TCP/SSH streams.
- Local IPC v5 requires upgrading CLI and agent together. Managed transfers
  require the new remote `SyncTransfer` extension; no legacy downgrade occurs.
- Safe transfer duration/outcome telemetry is collected by the Vector pipeline.

## [0.1.0] - 2026-09-26

**Engineering preview.** This first published workspace release is for explicit
evaluation and integration, not production readiness or an in-place upgrade of
earlier development state. Native Linux x86_64 and macOS arm64 bundles contain
`rds`, `rds-agent`, `rds-relay` and `rds-server`, with both transport backends and
the owned relay. Desktop capture/rendering is not enabled in these binaries.
See [release instructions](docs/releases.md) for verification and limitations.

- Native SSH uses the standard SSH protocol through `russh`, OS PTYs, explicit
  host-key pins and private-key/SSH-agent authentication. A shared Rust session
  manager owns connections and supports explicit device/session selection.
- Grant v2 binds audience, session identity, monotonic renewal and directional
  sync/view/control scopes. Local IPC is version 3. Renewals currently require
  an explicit caller; GDS automatic issuance/renewal is not shipped.
- Bounded file sync, signed discovery, durable policy/record state and protected
  metrics/logging have regression and fault-injection coverage. Vector and
  OpenObserve configurations are included in the source archive under `ops/`.
- Previous development wire formats and policy snapshots are not automatically
  migrated. Re-enroll/re-sign and provision fresh isolated state for evaluation;
  preserve old keys/data until a reviewed migration exists.
- Interactive desktop presentation, native macOS/Wayland desktop backends,
  platform packaging/notarization, automatic updates/rollback, broad NAT/WAN and
  physical-device qualification remain open. W0–W10 are not closed by this tag.

Development history included in this release:

- Add a Rust directory-capacity scenario to `rds-bench`, using shared production
  bounds and fresh synthetic state. Measure full-catalog renewal, deletion,
  reactivation and reopen; retain machine-readable evidence for 4096 identities.
  Production limits and storage behavior are unchanged. Document the pending
  offline migration procedure and measured startup-validation cost. See the
  [capacity receipt](docs/reports/rds-capacity-20260924.md).

- Harden directory HTTP framing in both directions: reject duplicate lengths,
  transfer encoding, malformed fields/start lines and unsupported encodings;
  enforce exact head/body limits and validate outgoing fields before sending.
  Buffer header reads while preserving the body, close after one exchange, and
  omit bodies for parsed HEAD requests even on worker overload. No new dependency
  or runtime helper. This is a restricted private HTTP profile; proxy/tunnel
  compatibility remains unqualified. See [record-state.md](docs/record-state.md).

- Default directory enrollment to deny; provision publishers separately with
  repeated `rds-server --directory-allow`. Unknown identities cannot publish,
  fetch or delete retained records. Admit only verified higher revisions under
  the store's compare/commit lock; duplicate, stale and forged requests spend
  no write quota. Separate new identity, known renewal, extra-write and policy
  budgets so one writer cannot exhaust every admitted device's renewal quota.
  Custom stores implement `put_admitted`/`remove_admitted`; synthetic fixtures
  explicitly opt into open enrollment. See [record-state.md](docs/record-state.md)
  for limits and the remaining GDS/migration/throughput qualification.

- Persist suspend-inclusive record leases and clock floors; refuse rollback,
  require a successor after OS reboot, and collect expired content in bounded
  owned jobs while retaining revision/digest history. Bound memory-store
  identities and preserve renewal at identity saturation. Add collection metrics
  and exact successor retry after HTTP 410. The database becomes format 3;
  earlier formats require the still-pending migration procedure. No production
  dependency or external helper is added. See [record-state.md](docs/record-state.md).
  Custom `RecordStore` implementations must provide bounded `collect_expired`.

- Give endpoint updates and deletions one explicit revision sequence, separate
  signature domains, bounded fields and exact expiry checks. Exact signed retries
  are idempotent; equal-revision conflicts are refused. Persist publisher counters
  and pending bytes before announce, supervise fatal publication failures, and
  require durable agent state (`--record-state`). This changes the pre-1.0 wire
  and library APIs: `publish`/delete construction require a revision;
  `Client::remove` takes a signed deletion; `AnnounceConfig` takes a `RecordIssuer`
  and `announce` returns a result. Migration and directory admission remain
  pending; see [record-state.md](docs/record-state.md).
  Preserve owned `rds-relay://` locators through shared typed identity/socket
  parsing, including IPv6 brackets, and exercise announce-to-relay connectivity.

- Release policy, database and sync receive locks when their Rust owner drops,
  including when another thread's fork temporarily inherits a descriptor.
  Persistent lock inodes are retained; successor ownership remains exclusive.

- Keep signed delete tombstones in both record stores and serialize mutations.
  Replace per-key JSON writes with an embedded Rust redb transaction plus a
  durable generation anchor; refuse corrupt/missing state and database-only
  rollback. Bound and exclusively own the protected database file. Legacy
  directories require explicit migration, which remains pending along with
  enrollment quota/fairness work; see [record-state.md](docs/record-state.md).

- Persist signed policy revisions, authority rotations and absolute freshness
  leases across restart. Managed grant mode now requires a configured revocation
  feed; missing/stale policy, feed shutdown or failed durable commit closes
  admission and live connections. Registry/name signatures move to v2 and
  revocations to domain-separated v1 with positive epoch/revision metadata.
  Re-sign snapshots and upgrade consumers together. Add protected state and
  rotation options to all three binaries; see [policy-state.md](docs/policy-state.md)
  for restart/boot behavior, migration, limits and outstanding qualification.
  Directory disk/signature jobs use a bounded blocking pool. No external runtime
  program added; OS clock/filesystem bindings reuse locked dependencies.

- Add native directory HTTPS and DNS origins to `rds --server` and
  `rds-agent --directory`. Verify certificate chains/hostnames, support explicit
  private CA bundles, and keep one deadline across DNS/TCP/TLS/HTTP. No insecure
  fallback, redirect following or external TLS helper. Server flags
  `--directory-tls-cert/--directory-tls-key` configure the directory separately
  from relay TLS. Pending requests and their connection budget now belong to the
  service lifetime. Library API: `Client::addr()` returns `Option<SocketAddr>`
  because DNS origins have no fixed literal address.

- Verify device names at the client using an independently configured registry
  key and a per-name signature proof. Bind name, endpoint identity and validity;
  reject unsigned replies, redirects, expired/future bindings and in-process
  rollback. Directory name GETs now stop serving expired snapshots. Registry
  snapshots require per-name proofs from the issuer; name clients require
  `--registry-key`. This changes the pre-1.0 name API; re-sign old snapshots.

- Persist verified destination chunks before advertising reuse, including
  edits and size changes. Assemble through uniquely owned staging files;
  preserve unrelated `.rds-part` siblings and clean only owned journal names.
  Pin all journal/destination operations to no-follow directory handles and
  hold the pull source inode across manifest/chunk reads. Serialize receives
  per root across processes, sync data/parents before success, and release
  receives when their control stream is canceled. No external helper added.
  Library API: `Journal::assemble(self)` now consumes the pinned journal;
  the path-returning `proto::resolve_under` helper is removed because it
  cannot provide race-free filesystem confinement.

- Preserve revocations without subscribers and during concurrent updates.
  Make grant admission atomic: failed/canceled Authz responses close the
  connection, release replay reservations and stop watchdogs. Recheck expiry
  and revocation at service admission; observe the initial denylist snapshot.

- Idle-desktop suppression: the X11 capturer subscribes a DAMAGE object
  on the root window (`NON_EMPTY` report level, re-armed via
  `DamageSubtract`); while the screen is still, the producer skips the
  capture→convert→encode path entirely, polling at 25 ms and emitting a
  refresh frame at least once a second so teardown stays prompt and the
  delta chain stays fresh. Backends without damage tracking report
  `changed() == true` and behave as before.
- Sync send path: the per-chunk `vec![0; len]` allocation is now one
  256 KiB scratch buffer per stream — zero allocation per chunk.
- Desktop capture/encode throughput:
  - X11 capture uses MIT-SHM (`CreateSegment` fd-passing, server ≥1.2)
    when available — the pixmap lands in a shared segment instead of an
    ~8 MiB serialized `GetImage` reply per 1080p frame; plain `GetImage`
    remains the fallback for remote/older servers.
  - The encoder recycles its I420 input buffer across frames (~3 MiB
    per frame at 1080p — ~190 MB/s of alloc churn at 60 fps removed).
  - Decode output goes I420→RGBA in one SIMD pass (`write_rgba8`,
    AVX2 on x86-64) plus an in-place R↔B swap — replacing an RGB8
    scratch buffer plus a scalar expand.
- Desktop encode path:
  - OpenH264 now runs `ScreenContentRealTime` — the correct usage type
    for desktop content (text/sharp edges, not camera footage); adaptive
    quantization and background detection are set off explicitly as
    upstream does not support them for screen content.
  - BGRA→I420 conversion now goes through the encoder crate's own
    strided source (`BGRA8Source` on `RawFrame`), which dispatches to
    AVX2 at runtime on x86-64 — ~3.5× faster than the previous scalar
    loop at 1080p (≈1.7 ms vs ≈5.9 ms per frame measured), with
    box-averaged chroma and no extra dependency.
- Discovery abuse resistance: `Limits::max_conns` (default 1024) caps
  concurrently held connections — a connection flood can no longer
  spend an unbounded number of tasks/FDs; excess sockets close on
  accept.
- Transport tuning and relay redundancy:
  - BBRv3 congestion control on both backends (paced, bufferbloat-
    resistant) instead of the loss-based Cubic default — better
    latency under load for interactive desktop and bulk sync.
  - 4 MiB stream receive window / 32 MiB connection send window
    (upstream defaults target ~100 Mbps × 100 ms): large keyframes and
    sync chunk streams no longer stall on high-BDP links.
  - `--relay` is repeatable on `rds` and `rds-agent`; endpoints probe
    all configured relays, home on the fastest and fail over
    automatically — the iroh-recommended ≥2-relay production topology.
- Relay TLS and lifecycle polish:
  - `rds-server`/`rds-relay` gain native TLS on the relay listener:
    `--tls-cert/--tls-key` for PEM files (rustls `ring` provider), or
    in-process Let's Encrypt via `--tls-acme-domain/--tls-acme-contact/
    --tls-acme-cache` (TLS-ALPN-01, needs :443). HTTPS binds
    `--tls-https-addr` (default 3443); the HTTP port keeps only the
    captive-portal probe and `/healthz` for monitoring.
  - All binaries shut down gracefully: `rds` closes its endpoint on
    exit (no more iroh "ungraceful abort" on `rds id`/`ticket`), and
    `rds-agent`/`rds-server`/`rds-relay` handle SIGINT+SIGTERM —
    peers get `CONNECTION_CLOSE` and relay websockets close cleanly
    under `systemctl stop` instead of dying mid-accept.
  - `rds id` no longer binds a socket: it prints the key's identity
    directly, so it is instant, offline, and refuses clearly when no
    key file can be resolved instead of printing a fresh ephemeral id.
- Stability/latency hardening across the workspace:
  - `rds-net`: the uni demux hands each accepted stream its own
    tag-read task under a 10s bound — a peer that opens a stream and
    never writes its `UniHello` can no longer stall routing of every
    stream behind it, and a failed send into a reclaimed inbox no
    longer removes the *new* route (channel-identity checked).
  - `rds-desktop`: the decoder is session-local (a shared static one
    cross-contaminated reference chains between sessions); decode
    failure auto-requests an IDR, rate-limited at 500ms so corrupt
    stretches can't storm; session ack and frame stream header/body
    reads are bounded at 30s, frame bodies at 32 MiB. On the serving
    side a frame that goes stale *while sending* is reset mid-write
    (MoQ-style) — a stale delta's tail no longer consumes path
    capacity; the reset re-arms the producer's IDR flag and drains the
    undecodable deltas. Frame streams carry an explicit constant
    priority below control (escalating per-frame priorities that
    starved in-flight sends are not reintroduced). Empty encode-failure
    placeholders are skipped instead of sent.
  - `openh264` codec: the `keyframe` flag is read off the emitted NAL
    units instead of assumed from `seq % 240` (which was wrong —
    `intra_frame_period` defaulted to `auto`, so periodic IDRs never
    fired on schedule); the period is now pinned at 240 (~8s at 30fps)
    as a bound on undecodable time. `set_bitrate` actually rebuilds the
    encoder past a 15% deadband instead of being a silent no-op, and
    the rebuild's first frame is a real IDR.
  - `rds-sync`: manifests stream through `StreamCDC` (memory bounded at
    one max-size chunk, proven identical to slice chunking); manifest
    scans, journal open/verify, and assembly run on `spawn_blocking`;
    verified chunks are written by a dedicated blocking-pool sink
    behind a bounded queue; every protocol read and chunk body is
    bounded by a 300s stall; completion counts chunks on the wire —
    not the sink's lagging `present` counter, which used to park the
    receive loop in a 300s stall after the last chunk.
  - `rds-agent`: stream hello bounded at 15s; state/grant/watcher
    mutexes recover from poisoning instead of denying service forever;
    request paths refuse with `HelloAck::Error` instead of
    `unreachable!`.
  - `rds-discovery`: `/v1/registry` PUT verifies freshness and stores
    under one write lock — two racing valid PUTs can no longer leave
    the older snapshot stored (regression test
    `concurrent_registry_puts_cannot_regress`).
  - `rds-relay`: register/control-stream wait bounded at 15s — a
    connection that never registers no longer parks a task.
  - `rds-cli`: connect bounded at 30s; every `HelloAck` wait and the
    ping echo bounded at 15s.
- Protocol v3 + review hardening: every uni-directional stream now
  opens with a `UniHello` tag (`Desktop`/`Sync`/`Audio`), and the
  accepting side routes it through a single per-connection demux
  (`Connection::uni_streams`) — desktop and sync can share a
  connection without racing `accept_uni`. Inboxes end cleanly:
  `UniStreams::recv` returns `None` once the connection dies — the
  demux's exit drops every registered sender instead of leaving
  consumers parked. `PROTOCOL_VERSION` is 3; v2 peers won't interop
  on uni streams.
- Security fixes: sync confinement is now resolved, not just lexical —
  `resolve_under` canonicalizes the deepest existing ancestor of every
  destination and requires it to stay under the canonical sync root,
  so symlinked components can't redirect pulls, the `.rds-sync`
  journal, or assembly outside the root; assembly's temp file is a
  suffixed `*.rds-part` created exclusively after unlinking any stale
  one, and `.rds-sync` as a first path component is refused outright.
  `ChunkHdr.len` is checked against the manifest before it sizes the
  receive buffer (a forged u32 no longer forces a huge allocation).
  X11 scroll injection clamps deltas to 32 clicks per event. Input
  events naming a display other than the session's are dropped
  un-acked. The directory's `/v1/revocations` PUT now verifies and
  stores under one write lock (no check-then-store regression window).
- Correctness fix: `mailbox::Sender` decrements an explicit sender
  count and then notifies on drop — a consumer parked in `recv` now
  observes the last sender leaving instead of sleeping forever, and a
  cross-thread wake can't observe a stale count and re-park
  (regression tests `parked_recv_wakes_when_last_sender_drops`,
  `parked_recv_survives_drop_wake_race`).
- CI/CD: the hand-rolled `ci.yml` is replaced by pinned
  `ci-workflows` reusables (0.1.26): `rust-ci` (locked build, fmt,
  five clippy lanes, ubuntu+macos test matrix), `rust-supply-chain`
  (cargo-deny per `deny.toml`, cargo-audit, cargo-machete — weekly
  advisory sweep), `public-codeql` (rust + actions, build-mode none)
  and `release-supply-chain` (tag `X.Y.Z` → immutable release: source
  archive + SPDX SBOM + SHA256SUMS + SLSA/SBOM attestations, behind a
  `release` environment). `deny.toml` now allows `Unlicense` and
  `CDLA-Permissive-2.0` (iroh transitive deps) and ignores the two
  unfixable unmaintained advisories; 13 unused crate dependencies
  removed; `VERSION` file added for the release contract; dependabot
  tracks cargo + github-actions weekly.
- WS8 deployment: `deploy/systemd/rds-server.service` and
  `rds-agent.service` — hardened units (ProtectSystem=strict,
  NoNewPrivileges, PrivateTmp/Devices, ProtectKernel*/ControlGroups,
  empty CapabilityBoundingSet, @system-service filter,
  AF_INET/6/UNIX/NETLINK only, UMask=0077, StateDirectory-scoped
  writes). `docs/deployment.md` documents install, ports/firewall
  (3340/tcp relay, 3341/tcp directory, metrics loopback-only),
  restart/upgrade/drain procedures and a failure-modes table.
  `rds ping` now prints per-path stats (via/selected/RTT/sent/lost/
  cwnd) for ops evidence; `rds-net/examples/ticket` mints
  restricted-address tickets (e.g. relay-only) for path verification.
  Deployment findings encoded: AF_NETLINK is required for interface
  monitoring; a client transiting the relay must itself be on the
  relay's `--allow`; an agent and a CLI on one host need distinct key
  files or the relay disconnects the duplicate EndpointId.
- WS7 observability: `rds-net::metrics` — a per-endpoint `Registry`
  of atomic counters plus a per-connection `ConnSampler` that folds
  cumulative `path_stats()` deltas into them. The
  `via="direct"`/`via="relay"` split is exact across path migration:
  datagrams and bytes sent/lost, congestion events, paths seen,
  connection totals, and RTT/cwnd/live-paths/active-connections
  gauges. QNT attempt/success counters are driven by the noq policy
  driver (iroh does not expose hole-punch attempts; a `direct` path
  appearing after a relay-only start is the equivalent signal).
  `Registry::render_prometheus` emits text exposition behind the new
  `metrics` feature. `PathStats` gains `sent_bytes`/`recv_bytes`.
  `rds-server`'s `GET /v1/metrics` now reports per-endpoint PUT
  counters under anonymized 16-hex BLAKE3-prefix labels and answers
  loopback peers only — raw keys, peer addresses and content are
  never exposed; remote scraping goes over SSH or a local exporter.
  The agent wraps every connection in an `rds.conn{peer, session_id}`
  span with nested `rds.stream{service}` spans and runs a 1s
  `ConnSampler` per connection; `rds-sync` logs session-boundary
  events with byte/chunk counts. `rds-bench` reports embed the
  `client_`/`agent_` registry snapshot collected at run time, so
  every reported number is backed by the harness's own counters.
- WS6 content-addressed sync: `rds-sync` gains a real protocol —
  FastCDC chunking, BLAKE3 per-chunk + per-file roots, a bounded
  wire format (`Offer`/`Request`/`Refuse`, `ManifestPart` ≤512
  entries, `Need` bitmap ≤256K chunks, `ChunkSet` ≤4096 indices,
  chunk payloads on 4 dedicated uni streams, `SetDone`/`Done`).
  Receives journal under `<dest>/.rds-sync/<root>/`: surviving parts
  are re-verified by content hash (corrupt parts refetched), a torn
  meta is rebuilt from the offer, an existing destination file seeds
  `have` so identical resends move zero bytes, and assembly is an
  atomic rename after root verification. `rel_path` rejects
  traversal, absolute paths, NUL and oversize. `rds send`/`rds recv`
  push/pull through `rds_cli::open_sync`; the agent serves `Sync`
  under `--sync-dir` with a one-session-per-connection guard and
  advertises `Sync` in
  `Info` only when configured. E2E: byte-identical push/pull,
  zero-chunk resend, corrupt-part refetch, torn-journal and
  mid-transfer kill resume, repeated kill/resume convergence,
  traversal fuzz, and a lossy-socket impaired-lane completion.
- WS5 session/media protocol v2: `FrameHeader` gains
  `capture_ts_ms`/`send_ts_ms` (shared-clock latency measurement),
  `DesktopControl` gains heartbeat + `RequestIdr` + `SetBitrate`,
  `DesktopEvent` gains input acks + heartbeat echoes, and
  `InputEvent` carries `seq`/`ts_ms` metadata. The desktop session
  now runs a producer/writer pipeline with bounded collapse
  (keyframe-preserving), serialized sends token-bucket-paced to the
  adaptive `BitrateController` (RTT/loss/deadline-miss driven), and
  a receiver that drops frames below a next-expected-seq watermark,
  auto-requests IDR on delivered-seq gaps, and re-arms IDR when
  backpressure kills a keyframe. Per-frame stream priorities were
  removed after they starved in-flight streams under load; the
  control stream keeps max priority. `rds-net` gains a normalized
  `PathStats` facade on both backends (RTT, cwnd, sent/lost, relay
  flag) plus `EndpointConfig::without_discovery()` /
  `with_path_pinning()` and the feature-gated
  `bind_noq_with_socket` seam. `rds-bench` gains `ImpairingSocket` —
  an `AsyncUdpSocket` decorator applying seeded loss/delay/jitter
  *beneath* QUIC so path migration cannot bypass it. Tests: 5-test
  `session_v2` e2e suite on both transports proving keyframe
  roundtrip, 240fps newest-wins collapse, input-ack/heartbeat RTT,
  socket-verified impairment with split queue/wire latency gates,
  and a 60fps soak (`RDS_SOAK_SECS` for the 30min checkpoint run).
- Post-WS3 consistency + hardening: `rds-desktop` now programs
  against the `rds_net` facade (`Connection` plus the stream/error
  re-exports — the documented "+rds-net when streams abstract"
  direction), so the `desktop` feature on `rds-agent`/`rds-cli`
  compiles and serves on both transport backends instead of failing
  on a raw-`iroh` type mismatch; a new CI lane
  `--features rds-agent/desktop,rds-cli/desktop` (both OSes) keeps it
  compiling. Session teardown fixed: `serve_desktop` aborts the
  frame-writer task and the capture producer exits when its channel
  closes — `JoinHandle::abort` cannot interrupt `spawn_blocking`
  work, so ending a session previously leaked a permanently-spinning
  capture/encode thread; `DesktopSession` drop aborts the
  frame-receiver task so the connection closes with the session
  instead of outliving it. The directory's global per-minute window
  now covers every signature-verifying write — `PUT /v1/records`,
  `PUT /v1/registry`, `DELETE /v1/records/{key}` — checked before
  parsing, exported as `rds_directory_writes_rate_limited`.
  `x11rb` 0.13 → 0.14.
- WS3 discovery service + publish/resolve (`rds-discovery`):
  `EndpointRecord` payloads carry `issued_at`; stores reject
  forged, expired and same-age-or-older replayed records, and honor
  signed `DeleteRequest` tombstones with replay protection. Minimal
  owned HTTP/1.1 codec (`http.rs` — hard head/body caps, one request
  per connection) serving `PUT/GET/DELETE /v1/records`,
  `GET /v1/names/{name}`, `PUT /v1/registry`, `/v1/health`,
  `/v1/metrics` with per-key + global PUT rate limiting
  (`service.rs`), plus a matching timeout-bounded `client.rs`.
  Estate name→key resolution rides a signed `SignedRegistry`
  snapshot (`registry.rs`) verified against a configured registry
  key — the directory can withhold but never invent names.
  `rds-net`: `announce` task publishes on start, refreshes at
  `ttl/3`, and republishes promptly when the advertised address set
  changes; `resolve_target` resolves ticket → bare key → device
  name. `rds` CLI gains `--server`; `rds-agent` gains `--directory` /
  `--record-ttl`; `rds-server` composes relay + directory + registry
  flags. `rds-bench` gains the G3 `resolve-connect` scenario: cold
  name→resolve→connect→first-byte per fresh endpoint. Gate `c3`:
  G3 p50 well under the 300 ms budget (see
  `docs/reports/checkpoint-c3.md`).
- WS2 owned relay transport (`rds-relay` + `rds-net::backends::noq::relay`):
  shared wire protocol in `rds_core::relay` (`rds-relay/0` ALPN, control
  stream, `[32B key][payload]` datagram frames); owned server with
  per-endpoint slots keyed by TLS-verified identity, token-bucket rate
  limiting, `PeerGone` fan-out, drain broadcast, identity-checked
  replacement. Client side: `RelaySocket` joins the socket mux as a
  tunnel child — a same-key helper endpoint registers on the control
  bidi, peers map to synthetic `198.19.0.0/16` addresses, and
  `EndpointConfig::relay_endpoint` attaches it. e2e proves handshake +
  datagrams + streams entirely through the tunnel, stale-slot
  replacement, and drain eviction. Gate `c2` passed; multi-relay
  failover deferred to WS3.
- WS1c connection driver + path migration: per-connection
  `connection_driver` task (`policy.rs`) consumes QNT address
  advertisements (`ADD_ADDRESS`/`REMOVE_ADDRESS`) and path events, and
  applies a biased-RTT path selection — the lowest-RTT path becomes
  `Available`, all others `Backup`, with a 5ms stickiness margin to
  prevent flapping. Holds only `WeakConnectionHandle`/`WeakPathHandle`
  so the task never keeps a connection alive (last-handle-drop stays the
  close trigger). Both endpoints advertise their real socket addrs and
  the client initiates traversal rounds, so peers can upgrade paths
  learned in-band. Result: under the `impaired` scenario (5% loss,
  50+30ms delay via proxy) noq now migrates traffic to a direct path —
  p50 1.80ms vs 136.5ms before the driver, at parity with iroh's 1.68ms.
  Deterministic `turmoil` partition/repair simulation stays green; the
  socket-mux accepts handshakes on non-primary sockets.
- WS1b backend-neutral `rds-net` facade: unified `EndpointConfig`
  (backend, secret key, bind addr, relay, ALPNs) and `Backend` selector;
  `Endpoint`/`Connection`/`Incoming` wrappers dispatch to either
  substrate while sharing the same `noq` stream/error types.
  `rds-agent`, `rds-cli` and `rds-bench` no longer depend on `iroh`
  directly — `--backend iroh|noq` selects at bind time. Owned-backend
  `addr()` now advertises dialable candidates (loopback + kernel egress
  hint) instead of the unspecified bind address. New evidence: agent
  e2e runs on `Backend::Noq`, and `rds-bench --backend noq` produces
  same-harness parity numbers on the direct path (handshake p50 20.5ms
  vs iroh 22.6ms; ping p50 1.31ms vs 1.49ms; suite compare within
  tolerance). Relay paths correctly refuse on `noq` until WS2.
- WS1 owned transport backend (`rds-net::backends::noq`, behind the
  `transport-noq` feature): noq endpoint with our own RFC 7250
  raw-public-key TLS layer (`tls.rs` — Ed25519 SPKI certs, server-name
  `EndpointId` verification, mutual client certs) that is
  wire-compatible with the shipping iroh backend on `rds/0`; BLAKE3
  stateless-reset key; endpoint facade (`bind/connect/accept/addr/close`)
  mirroring the iroh surface; candidate pipeline (`policy.rs` — dedup,
  cap at multipath limit, `open_path_ensure` for extra candidates, QNT
  round trigger); transport config matching iroh defaults (multipath ×8,
  QNT addrs ×32, QUIC datagrams, path keepalive/idle). Integration tests
  prove noq↔noq loopback, QUIC datagrams, and cross-backend interop
  (iroh client ↔ noq server, both directions). `iroh` remains the
  default backend until C1 parity.
- WS0 measurement harness: new dev-only `rds-bench` crate — scenario
  runner (handshake/ping/transfer/multiconnect/relay-fallback/impaired)
  on real QUIC paths, deterministic seeded UDP impairment proxy
  (loss/delay/jitter/rate), JSON+markdown reports and suite-vs-suite
  drift comparison (`max(rel, floor)` rule, separate tail band).
  `scripts/checkpoint.sh` implements the gate machinery from
  `docs/implementation-plan.md`; gate `c0` passed with committed
  evidence under `docs/reports/` (baseline-iroh.md).
- Workspace restructure for the full-ownership architecture
  (`docs/research.md` §9): `rds-transport` renamed `rds-net` with
  `backends::{iroh,noq}`; new crates `rds-discovery` (signed endpoint
  records, memory/file stores), `rds-sync` (FastCDC+BLAKE3 manifests,
  delta computation), `rds-audio` (Opus pipeline scaffold),
  `rds-server` (GDS services host: relay + discovery directory);
  `rds-relay` gains a lib + `proto` owned-protocol scaffold;
  `rds-desktop` reorganized into `capture`/`codec`/`input`/`render`
  backend dirs with per-platform probe order.
- `docs/platforms.md`, `docs/conventions.md`, `docs/roadmap.md`:
  platform matrix (Linux x86_64, macOS arm64), engineering rules,
  milestone gates. CI matrix on both OSes; `rust-toolchain.toml`,
  `deny.toml`, workspace lint floor.
- Core transport foundation: six-crate workspace (`rds-core`,
  `rds-transport`, `rds-relay`, `rds-agent`, `rds-cli`, `rds-desktop`) on
  iroh 1.2 / QUIC with Ed25519 endpoint identity, hole-punched direct
  paths and self-hosted relay fallback.
- `rds` CLI: `id`, `ticket`, `ping`, `info`, `ssh`, `forward`, `desktop`.
- `rds-agent`: peer `EndpointId` allowlist, scoped TCP forwarding
  (default: local sshd only), desktop session service behind `desktop`.
- `rds-relay`: embedded iroh-relay with endpoint allowlist.
- `rds-desktop`: capturer/encoder/decoder/input traits, MoQ-style
  per-frame uni streams with stale-frame drop, X11 capture + XTEST input
  + OpenH264 codec behind the `x11` feature.
- `docs/architecture.md`: transport/desktop research and decisions.
- Initial Rust skeleton: library crate and `remote-device-sync` CLI stub.
