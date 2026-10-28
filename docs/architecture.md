# remote-device-sync architecture

Current behavior is recorded in the [capability matrix](capability-matrix.md)
and the owning code/contracts; unfinished work is ordered by the
[stability plan](stability-plan-20261009.md). The
[September 24 audit](reports/rds-audit-20260924.md) is historical defect evidence,
and the [remediation plan](remediation-plan.md) retains acceptance requirements.
Design goals and dated receipts do not establish current product acceptance.

Remote access for GDS devices: SSH reachability and remote desktop sessions
between any two enrolled devices, assisted by a GDS-operated server.
Design goals, in order: **minimum interactive latency**, **maximum
connection stability**, defense in depth, no inbound firewall changes.

An explicit bounded Iroh registration option maintains up to three initially
configured custom relays independently of peer activity. Completed registrations
contribute advertised addresses; failures, map withdrawal and owned task exit
remove readiness. The home-status API and default policy remain independent.
Registration does not establish a QUIC path or assign directory authority.
Private deployment owns host roles and ordered fallback policy. See
[endpoint settings](endpoint-configuration.md#reachability-presets).

Preferred local dialing retains the pinned endpoint identity. Before fallback,
the manager awaits an explicit Iroh actor acknowledgement discarding only failed
IP hints with no live authenticated path. The small default-inactive vendor
extension preserves live authenticated paths, other peers and
relay/custom candidates; owned Noq has no corresponding shared hint cache.
This prevents a stale address belonging to another key from winning another
handshake against the newly resolved relay, including an address which worked
before being assigned to another computer. Old cache history does not establish
current authentication. TLS and admission remain unchanged.
See [local sessions](local-sessions.md) and [patch provenance](../vendor/iroh/RDS-PATCH.md).

Configured-order relay preference is a separate explicit setting. Only eligible
confirmed relay tiers participate; faster direct paths still compete by RTT.
The order is the existing URL list, with no second authority. Known standby
restoration also works while the surviving carrier is itself a relay. Endpoint
close seals readiness before returning, without shortening QUIC draining.

Explicit latency path preference now observes acknowledgement progress as well
as RTT. A passive adapter delegates the chosen congestion controller unchanged;
each established path owns its pending-work proof and runtime clock. A path
with outstanding ack-eliciting work and no progress for `max(4 RTT, 500ms)` is
no longer selected while a confirmed sibling exists, but it stays open: a relay
path queued behind a desktop stream regains its ACKs, and closing it would strand
the peer's data in flight on it. It may retire only after `max(8 RTT, 2s)`
without progress, and only with an established sibling whose still-fresh ACK
follows the start of that outstanding-work interval and which is not itself
stalled. An older
idle ACK cannot prove replacement progress during a common outage. The existing reliable
stream transfers to that sibling through Noq's ordinary path-abandon machinery;
the last open path is never retired. See [the recovery evidence](reports/rds-path-ack-progress-20261006.md).

Initial multipath validation has its own engine deadline, independent of idle
policy for established paths. The budget uses three times the larger initial
PTO and PTO of live validated paths. An unsuccessful attempt closes through
existing PATH_ABANDON/CID handling; its ID is not reused. Validation success
cancels this timer. RFC 9000 address migration keeps its separate timeout and
previous-route fallback, and last-path protection remains in the engine.

Tagged sync completion now transfers connection exclusion and the data-lane
admission permit into the engine. Filesystem/control work and its reader close before that guard releases,
and only then is the terminal server FIN exposed to the peer. This makes an
immediate next transfer admissible without retrying application work. Dropping
the owning future still releases the guard on cancellation. Failure logs retain
the greeting/transfer stage and ID without changing the IPC error contract.

Latency preference maintains previously known standby paths after abandonment.
Iroh's opt-in refresh restores known relay/custom candidates beside a live IP
path; defaults and pinned selectors retain their opening policy. Noq reoffers
only previously established original-ticket candidates, respecting transport
health, relay drain/unavailability, family support and its bounded retry queue.
Allocation is still not validation; selection/retirement needs established paths
and recent positive acknowledgement evidence. No interface, peer or route is
invented and application input is not replayed.

Iroh selection retains one live IP sibling when a relay/custom path is selected.
The event-updated path map cannot be counted unchanged while closing paths in a
loop: that could close every IP sibling. The retained IP prefers the locally
established selection, otherwise the lowest RTT, so a selected path still being
validated on another connection cannot remove the last local IP alternative.
Pending opening retries deduplicate four-tuples and stale closed handles do not
prevent restoration. See [standby evidence](reports/rds-warm-standby-20261006.md).

Directory publication also isolates a full disk from established transport.
Only ENOSPC/quota failures are retryable. The publisher retains its exclusive
lock and accepts only the pre-commit state or its exact attempted successor;
it synchronizes those verified bytes before resuming signed publication. Clock
rollback, changed history, unsafe files and unrelated I/O remain fatal. No
record is published without durable revision allocation, and authorization
expiry still applies independently. The announcer uses its bounded jittered
retry policy without recreating the endpoint or replaying application input.

Status: version 0.1.0, with implemented connectivity, managed SSH/single-file
transfer and an experimental native X11 desktop path. Current capability states
live in [capability-matrix.md](capability-matrix.md); remaining acceptance and
execution order live in [stability-plan-20261009.md](stability-plan-20261009.md).
[Research](research.md) separates the dated stack survey from current decisions.

Agent local service starts after endpoint bind, independently of external relay
availability. It does not await iroh's unbounded relay-only `online()` predicate.

The explicit DesktopV4 extension releases media admission on a session-bound,
digest-checked encoded-payload receipt instead of waiting for transport FIN
acknowledgement. It retains the three-frame bound, reference ordering, negotiated
geometry/grant policy and QUIC control. Receipts originate in bounded wire readers,
before native decode/IPC waits, and use an independent bounded control queue.
Legacy sessions retain their existing wire and delivery semantics. See
[native delivery receipts](native-viewer.md#validated-payload-delivery-receipts).
The directory announcer republishes evolving addresses; the printed ticket is
only a startup snapshot. Local readiness and remote reachability are distinct.

Endpoint settings can bound usable path kinds and select conservative primary
QUIC packetization for measured tunnel qualification. Defaults preserve adaptive
MTU discovery/offloads and all path kinds. The same policy lowers into Iroh and
owned Noq; OS routing, endpoint identity and service authorization are separate.
An optional local congestion-controller choice similarly lowers into both
backends. BBRv3 remains the default; explicit Cubic enables controlled path
comparison with unchanged admission, priorities, windows and authentication.

CLI and agent share [versioned endpoint configuration](endpoint-configuration.md)
with explicit file/flag precedence, backend/relay validation and preflight before
identity creation. The owned relay uses a separately pinned public identity.
The [directory lifecycle](directory-lifecycle.md) owns bounded connection,
request-worker and maintenance task groups. Explicit close seals admission and
joins all three groups; canceled close waiters and an abnormal runner exit retain
that ownership. The server awaits directory and relay shutdown together.
The shared [identity store](identity-storage.md) preserves the raw seed format
and serializes creation under a directory lock. It publishes only a complete,
synchronized seed with no replacement, refuses unsafe existing files and
recovers its bounded pending state after a process exits.
The default [local session manager](local-sessions.md) now reuses the running
agent's endpoint through same-UID Unix IPC. It owns outgoing connections,
selection, bounded requests and TCP streams. Ordinary connectivity commands use
it without loading a key. Explicit `--direct` commands, the agent and the owned
relay acquire cooperative exclusive ownership of the validated seed inode.
Single-file send/receive also use the manager and an isolated transfer route.
The native viewer also uses that manager through a keyless application entry
point. Installed-binary/platform qualification is recorded separately from the
remaining W2.4 multi-user and grant-issuer integration.
TCP service flags, client requests and agent policy use the same canonical
`rds-core::TcpTarget`; IPv6 spelling and IPv4-mapped addresses normalize before
policy comparison and dialing. Parsing has no DNS or socket side effects.
Role-level authority/service policy, negotiated session limits and lifecycle
configuration remain W2 work; the endpoint schema does not claim to cover them.

Owned noq endpoints now track path-policy tasks. A weak QUIC closure notification
ends each driver without retaining the connection; endpoint close seals task
admission, directly closes weakly referenced connections through a shutdown
signal and waits for policy-task cleanup, including after a stopped protocol
I/O driver. Streams
may legitimately outlive a Connection wrapper. Completed drivers are removed
immediately, and `active_path_drivers()` exposes the owned endpoint's count.
This boundary does not yet own every agent, relay or disk task; see the
[lifecycle receipt](reports/rds-driver-lifecycle-20260925.md) for its tested scope.

The shared [uni-stream router](uni-routing.md) is owned by facade connections
and live inboxes, without a task/connection ownership cycle. A bounded task group
handles tags and inbox handoff; QUIC closure cancels and joins its workers.
Its local budgets do not yet supply negotiated session IDs or per-service QoS.

The [agent task boundary](agent-lifecycle.md) owns connection and bidirectional
service task groups. Positive admission budgets cover pending handshakes and
service workers; normal closure joins services and the authorization watchdog.
Metrics sampling runs within the owning connection future. These application
limits do not establish global media/disk/relay resource bounds.

The [client request boundary](client-lifecycle.md) applies one deadline to stream
opening, request write and response completion. Pending streams reset on
cancellation; incomplete authorization closes its connection. Local forwarding
owns a bounded worker group with connection-close joins and cancellation cleanup.

The [desktop client boundary](desktop-client-lifecycle.md) owns its control,
event and bounded frame-reader tasks. Native decoding uses globally limited
blocking work with retained cancellation budgets. The optional
[native viewer](native-viewer.md) owns a winit event loop on the OS main thread,
a wgpu surface and one newest shared BGRA image per session. Pending measurement
and idle-tab restoration share that allocation; repainting retained pixels does
not admit another frame timing sample. The CLI owns the cancelable network
worker and reopens a desktop session on the same authenticated peer after loss.
Per-session wire routing and broader native media acceptance remain separate.

Managed native input/heartbeat dispatch, incoming control-event observation and
ordered media/decode have independent retained futures. The additive local
separated-desktop API attaches one same-UID event socket using a bounded,
single-claim, process-lifetime route. Video FIFO backpressure cannot block its
input ACK/heartbeat observation. Both sockets share desktop termination, without
closing the manager's unrelated peer streams. Existing combined APIs and remote
wire formats remain; see [the native contract](native-viewer.md).

Native control liveness is independent of decoded-frame progress. Exact bounded
heartbeat confirmations advance per-session monotonic send-time evidence;
late/duplicate replies cannot mask stalled control with a live picture. The
viewer retains clipboard's absolute budget and ends only the owning desktop
attempt, without replay or managed peer teardown. See [native control progress](native-viewer.md#independent-control-progress).

Explicit video repair advances a session-local media epoch and interrupts only
obsolete media work. An independent-picture lease holds admission until receipt;
repeated requests coalesce during that recovery. X11 keyboard holds similarly
own per-key native repeat suppression: the viewer forwards local OS repeats,
so network delay cannot manufacture additional letters. Both native endpoints
need the coordinated update. See [the regression receipt](reports/rds-interactive-repair-20261004.md)
for tested boundaries and remaining installed-device qualification.

Owned path policy now uses [validated eligibility](path-selection.md): only the
handshake path is seeded; application-opened candidates stay Backup until an
Established event. An owned bounded queue retries temporary path-credit
exhaustion and reconciles candidate advertisements. Path-event reconciliation,
physical failover and complete path metrics remain open.

The owned mux now [isolates local child failures](socket-failure-isolation.md).
Terminal child errors wake shared health observers without stopping healthy
siblings; packet-scoped UDP errors do not retire a socket. Policy withdraws
failed advertisements and excludes failed observed paths, including a last
path the engine refuses to close. Loss of every child explicitly closes held
connections. Socket recreation, complete path reconciliation and physical
interface/failover qualification remain open.

Transport metric samplers use weak backend observation handles and never own
connection facades, uni routers or transport I/O. Closure wakes sampling tasks
independently of their interval; real streams remain valid I/O owners after
facade drop. Last-selected RTT/cwnd samples are invalidated when their sampler
is dropped, without clearing a newer sampler's observation. See
[path metrics](path-telemetry.md) for coverage and sampled-accounting limits.

Owned requests also [pin the exact requested ALPN](protocol-negotiation.md) in
an immutable per-protocol TLS configuration. A missing protocol fails before
service use; concurrent requests cannot replace each other's offer.

Owned initial handshakes use a [bounded candidate race](candidate-dialing.md):
eight direct addresses with fair family selection and canonical mapped aliases,
plus an attached relay, one 15-second deadline, common
identity pinning, and one retained winner. This removes first-address blocking;
relay bootstrap, scoped advertisements and complete path-event recovery remain open.

Owned relay client/server share [bounded control framing](relay-control.md).
Drain and PeerGone use the same exact codec as registration and liveness. Drain
receipt preserves usable grace-period traffic, and stale attachment teardown
cannot invalidate a replacement. Relay-link send failures and missing peer mappings
are route loss rather than fatal I/O for the shared QUIC connection; a weak handle
reports local tunnel availability. Warm relay migration remains unqualified.

Client relay queues and peer metadata have positive per-tunnel limits. Active
connection policy tasks own metadata-only peer leases; source learning cannot
overwrite a live synthetic alias and capacity pressure evicts only unpinned
entries. Queue overflow loses whole datagrams with counters. Diagnostic handles
do not retain queued payloads or I/O. See [relay ownership](relay-control.md#client-peer-ownership-and-receive-bounds).
The owned server bounds admitted handshakes/sessions, owns their task group and
joins shutdown; canceled drain callers cannot cancel server cleanup. Tunnel
health wakes attached connection policy so stale relay RTT cannot keep a failed
link selected over a validated direct path. Global resource and physical-network
qualification remain open.

Both server binaries share a validated relay runtime: iroh remains the default;
the opt-in owned QUIC mode loads a separate durable identity and requires an
allowlist unless explicitly opened for development. The host prepares relay,
registry and TLS configuration before creating identity/catalog state, then
awaits both services on shutdown. Unexpected runner termination triggers
shutdown of the composed host and a failure exit, with errors retained across
observation and cleanup. See [runtime composition](relay-runtime.md)
for feature selection, startup boundaries and compatibility.

Durable catalog and policy metrics are metadata projections from their existing
transaction owners. They publish after successful commit/reopen, expose storage
uncertainty and omit unavailable gauges. Scrapes use weak observation handles
and cached clock identity without database/policy locks or disk I/O. Agent
revocation metrics follow the effective watched admission value. These groups
are diagnostic observations, never another authorization or replay authority;
see [observability](observability.md#durable-catalog-and-policy-observations).

## Research summary

### Connectivity models surveyed

| Approach | Latency | Stability | Notes |
| --- | --- | --- | --- |
| Direct SSH to public IP | lowest | poor behind NAT | needs inbound ports; scanner exposure |
| Central relay only (RustDesk `hbbr`, Cloudflare Tunnel) | +1 RTT hop | high | always works, adds hop & server trust |
| P2P w/ relay fallback (Tailscale DERP, RustDesk hbbs+hbbr) | ~direct | high | >90% direct-path success reported by Tailscale |
| Mesh VPN (WireGuard/ Tailscale, Nebula, Netbird) | ~direct | high | solves reachability, not desktop |

Facts that shaped the design:

- **Tailscale** (DERP relays + STUN + in-band disco) reports direct-path
  success "well north of 90%". Connections *start* on the relay and migrate
  to a direct path — the relay is also the signaling channel. This is the
  model to copy: relay-first for instant connectivity, upgrade to direct.
- **RustDesk** splits the broker into `hbbs` (rendezvous/punch coordinator)
  and `hbbr` (relay). Same shape, older transport (TCP/UDP custom).
- **Cloudflare Tunnel** (`cloudflared`) is outbound-only and stable, but
  terminates at the CF edge, adds a permanent extra hop, couples the session
  to a third-party daemon lifecycle, and is a poor fit for interactive
  desktop latency. Decision: not a data path. Optionally documented later
  as a fallback reachability story for SSH.
- **Nebula lighthouses / Netbird** confirm the same pattern: a small public
  rendezvous host + P2P overlay. Our equivalent is the GDS server.

### Desktop streaming surveyed

| Stack | LAN latency | Notes |
| --- | --- | --- |
| Parsec | ~7–10 ms | closed source, managed broker |
| Sunshine+Moonlight | ~8–15 ms | NVENC/AMF/VT hw encode; best-in-class quality; GameStream protocol |
| RustDesk | ~18–30 ms | software encoders by default |
| RDP (AVC420/444) | ~15–25 ms | universal clients; bitmap pipeline |
| VNC | 30–100 ms+ | fallback tier only |

Encoding is the latency budget line item that matters: hardware encoders
(NVENC/VA-API/VideoToolbox/MediaFoundation) add ~5–15 ms; software H.264
(OpenH264, ~8–10 ms per 1080p frame with asm) is acceptable for v1; AV1
(rav1e, pure Rust) is an option where CPU headroom exists.

### Rust crate findings

- **`iroh`** (Apache-2.0/MIT): QUIC endpoints dialed by `EndpointId`
  (Ed25519 public key). Hole punching coordinated over the home relay,
  relay fallback, stream priorities, QUIC datagrams, multipath on the
  `noq` QUIC fork. `iroh-relay` is a self-hostable relay binary/library.
  This is the substrate: it is exactly the rendezvous+relay+P2P pattern
  above, production-maintained, and the endpoint key is a stable identity.
- **Media transport pattern**: iroh maintainers recommend *not* using QUIC
  datagrams for realtime media — datagrams still engage congestion control.
  The MoQ pattern is one uni-directional stream per frame: reset stale
  frame streams and prioritize the newest via `SendStream::set_priority`.
- **`webrtc-rs`**: v0.20 Sans-I/O rewrite is new; media-side gaps remain
  (no jitter buffer interceptor, no FEC, incomplete congestion control).
  Not selected for v1 media; revisit if a browser client appears.
- **`ironrdp-server`**: real RDP-server skeleton (TLS, FastPath input,
  bitmap updates, DVCs). Kept as a future interop frontend so stock RDP
  clients could attach to an agent.
- **Capture**: Wayland mandates `xdg-desktop-portal` ScreenCast + PipeWire
  (DMA-BUF zero-copy where the compositor offers it; `ashpd`/`lamco-*`
  crates, `pipewire` crate). X11: `x11rb` MIT-SHM `GetImage` polling +
  XFixes cursor + XDamage. macOS: ScreenCaptureKit. Windows: DXGI
  Desktop Duplication / Windows.Graphics.Capture.
- **Input**: Wayland RemoteDesktop portal (libei) via `ashpd`; X11 XTEST
  via `x11rb`; `uinput` as privileged fallback; Win `SendInput`,
  macOS `CGEvent`.
- **SSH**: reuse the standard protocol through `russh`, behind `rds-ssh`.
  The native Rust client uses a pinned RDS TCP stream to the remote SSH server;
  that server supplies OS account isolation and PTYs. Host trust, local terminal
  restoration and cancellation are RDS policy. See [SSH](ssh.md).

## Architecture

```text
┌──────────┐   direct QUIC (hole-punched)   ┌──────────┐
│  rds cli │◄──────────────────────────────►│ rds-agent│
│ (viewer, │   or relayed via the relay     │ (daemon, │
│  SSH)    │◄────────────►┌──────────┐◄────►│  host)   │
└──────────┘              │rds-server│      └────┬─────┘
                          │ relay +  │           │
                          │ directory│      ┌────┴─────┐
                          │ (GDS)    │      │ sshd :22 │
                          └──────────┘      │ desktop  │
                                            └──────────┘
```

## Crate map and dependency direction

Dependencies point strictly downward; no cycles, no sideways deps at the
same layer. `docs/conventions.md` holds the enforceable rules.

`rds-observe` is a separate infrastructure leaf with no product-crate
dependencies. Agent/client instrumentation and all executable entrypoints
depend on it; `rds-core` remains free of I/O/runtime dependencies. The logging
adapter never owns a connection or calls Vector/OpenObserve directly.

```text
                     rds-core           types, wire format, tokens — leaf
                       │  │
        ┌──────────────┘  └───────────────┐
        ▼                               ▼
  rds-discovery                     rds-net
  signed EndpointRecord,            transports: backends::iroh (now)
  stores (mem/file), GDS bridge         backends::noq (ours, §9)
        │                               │
        └──────────────┬────────────────┘
                       ▼
   ┌──────────┬──────────────┬───────────┐
   rds-relay  rds-desktop    rds-audio    rds-sync
   proto+     capture/codec/ Opus codec   FastCDC+BLAKE3
   iroh shim  input/render   + jitter     manifests/delta
   └──────────┴──────────────┴───────────┘
                       ▼
        ┌──────────────────────────────┐
        ▼                              ▼
   rds-client — outgoing requests, manager and local IPC
        │                              │
   rds-agent (daemon)            rds-cli (operator)
                       ▼
   rds-server — GDS services host: relay + discovery + registry
```

- **`rds-server`** — runs on the GDS services host: the packet relay and
  the signed-record discovery directory; later the registry bridge into
  estate state, presence and audit. The relay forwards encrypted application
  payloads; the directory handles signed endpoint/policy metadata. Encryption
  of relayed payloads does not hide directory metadata from that server.
- **`rds-agent`** — daemon on each controlled device. Binds the endpoint
  (Ed25519 identity persisted), connects to its home relay, accepts
  `rds/0` connections, serves streams to an allowlist of peers. The default local
  control service hosts outgoing sessions on that same endpoint; `--control-dir`
  overrides its location, and `--no-control` explicitly disables it.
- **`rds-client`** — shared request/forwarding library and local session
  manager/client; depends on core/net/discovery/observe/sync, never on the CLI. The sync
  engine remains below the manager; no new external dependency is introduced.
- **`rds-ssh`** — bounded SSH session adapter over generic async I/O; depends
  on russh/Tokio/typed errors, not on RDS endpoints or the CLI. `rds-cli` owns
  Unix terminal adapters and feeds managed/direct streams into this library.
- **`rds`** — operator CLI. `rds id`, `rds ticket`, `rds ping`, `rds ssh`,
  `rds forward`, `rds desktop` (feature-gated), single-file `rds send/recv`.
  The same crate provides `rds-viewer`, a native application entry point using
  the local manager without reading an endpoint seed.
  `rds session` connects/lists/selects/pings/opens SSH/forwards through the local agent
  without loading a key or binding a network endpoint.

### Identity and authorization

- Network identity: the endpoint's Ed25519 key (`EndpointId`). QUIC-TLS
  authentication is built on it — connections are mutually authenticated
  by key, not by password.
- **Membership**: the agent serves only peers on its `allow` list of
  `EndpointId`s, checked at handshake completion.
- **Capability grants** (WS4): when `policy.issuers` is non-empty, the
  peer must additionally open `StreamHello::Authz` as the connection's
  first stream, presenting a grant signed by a trusted estate issuer.
  Every service stream raced ahead of the grant is refused; after
  verification each stream is scope-checked (`services`, `tcp_ports`,
  `displays`, `max_bps`). Grants are short-lived (`grant_max_ttl`),
  bound to subject and serving-device audience by [grant v2](grant-leases.md),
  non-replayable across concurrent connections (`active_grants`), and
  revocable: the directory serves an estate-signed `SignedRevocations`
  snapshot at `GET /v1/revocations`, agents poll it into their denylist,
  and a revoked or expired grant closes its live connection.
- Grant admission uses one connection-owned state machine. Pending service
  requests cannot proceed until Authz commits; its watchdog and replay
  reservation exist before the reply write. Failure or cancellation closes
  the connection and releases the lease. Denylist values survive without
  watchers; service admission rechecks revocation and expiry directly.
  Renewal preserves the stable revocation ID and exact scope, atomically extends
  a wall/continuous-clock lease before response FIN, and keeps one watchdog.
  One task slot is reserved from service bodies when the configured stream
  budget permits; grant mode retains its renewal lane. `SyncRead` and
  `SyncWrite` are enforced before filesystem access; `DesktopView` needs the
  additional `DesktopControl` capability for input. Legacy `Sync`/`Desktop`
  retain their broad permissions. Grant v3 enforces tenant/policy revision and
  signed sync subtrees when configured. Automatic GDS issuance/renewal and
  account/seat scope integration remain open; see [contract](grant-leases.md).
- GDS names: `GET /v1/names/{name}` returns one `SignedNameBinding`, signed
  by the registry issuer over `rds/name-binding/v2\0` plus the postcard
  payload `{stamp, registry_digest, version, name, key, issued_at, expires_at}`.
  The client requires
  an independently provisioned trust anchor (`rds-agent --registry-key <base32>`
  for managed clients, or `rds --direct --registry-key <base32>`),
  verifies the exact requested name and validity, then verifies the endpoint
  record against that key. No unsigned-name fallback or HTTP redirect is
  followed. The response contains no other inventory entries.
- `SignedRegistry::sign/publish` includes individually signed bindings for
  every entry; the directory validates their agreement with the snapshot and
  rechecks snapshot lifetime on GET. Old snapshots need re-signing, and old
  clients cannot use the new name response. Validity is at most 24 hours,
  `issued_at <= now < expires_at`, with no future clock allowance. A bounded
  durable client cache rejects older revisions, cross-name rollback and
  conflicting same-revision proofs. Directory and managed agents use the same
  acceptance store with sync/rename/sync before runtime publication. Revocation
  freshness is bounded to 300 seconds, checked at admission and during active
  sessions; missing/stale policy closes access. Leases retain absolute
  suspend-inclusive deadlines across process restart, and an OS reboot requires
  a newer signed revision. Dual-signed receipts advance authority epochs.
  See [policy-state.md](policy-state.md) for the complete contract, migration
  and limits, including the external GDS anchor still needed to detect rollback
  of the entire trusted local state. Tickets and
  explicitly pinned endpoint keys do not depend on registry-name trust.
- Directory clients accept HTTP(S) origins with DNS/IP hosts. HTTPS uses
  in-process rustls, verifies the certificate chain and hostname/IP SAN, and
  never falls back to plaintext or follows redirects. Public WebPKI roots are
  the default; an explicit PEM CA bundle replaces them for private estates.
  A single default 3-second deadline covers DNS, staggered TCP candidates
  (at most 16), TLS and the HTTP exchange. Pending dials are owned and canceled
  with the request. Native OS resolver work may finish after the async timeout;
  it cannot extend the caller's deadline. HTTP/1.1 uses an explicit Host header
  and the bounded Content-Length codec. Both directions reject duplicate lengths,
  transfer encoding and malformed headers before consuming a body. This is a
  private one-exchange profile, not a general HTTP implementation; see the
  [exact wire bounds and compatibility limits](record-state.md#directory-http-profile).
- `rds-server --directory-tls-cert/--directory-tls-key` enables a TLS-only
  listener on `--http-addr`; relay TLS is configured separately. The directory
  reserves its connection budget before spawning/handshaking. Its 10-second
  default absolute connection deadline includes TLS and response I/O; dropping
  the directory aborts its owned async requests. Store/signature operations run
  in bounded task groups and remain counted through disk completion even after
  request timeout. Explicit directory close joins connections, requests and
  maintenance, including retained groups after runner failure; see the
  [shutdown contract](directory-lifecycle.md). Endpoint records and delete tombstones share an
  embedded redb transaction. A separately synced generation anchor refuses
  rollback of an acknowledged database generation, including recovery to an
  older root. Both commits finish before readers or HTTP success can observe
  the mutation; uncertain I/O closes the store until reopen. Storage uses a
  protected, single-owner directory and a bounded no-follow file backend.
  Record updates and deletes share a positive publisher revision sequence and
  separate versioned signature domains. The production announce issuer commits
  its counter and exact signed bytes before sending; lost replies can retry
  without a new revision, and local history failures reach the agent supervisor.
  Stores check current lifetime on read/write, including exact retries. A stored
  suspend-inclusive lease survives process restart; OS reboot requires a newer
  signed publication. Bounded collection retires content while preserving its
  revision/digest floor, and observed expiry commits before a read returns.
  HTTP 410 prompts one durable local successor; network retry does not allocate
  another revision. Directory publisher enrollment defaults to deny and is
  configured independently with `--directory-allow`. Stores invoke write-budget
  admission only for verified higher revisions under the compare/commit owner;
  exact retries and stale mutations never debit the publisher's quota. Known identities
  have protected renewal capacity; new identities, extra writes and each policy
  role use separate budgets. Offline format-2 migration preserves signed revisions
  as retired floors in a new, fully verified directory; it requires successor
  publications and never changes the original database. Bounded Linux capacity
  evidence is available; legacy cutover and broader qualification remain W1.5;
  see [record-state.md](record-state.md) for migration and current limits. Directory
  certificate renewal is external provisioning plus restart for now; automatic
  renewal and hot reload are not implied by the relay's separate ACME support.

### Stream protocol (`ALPN = rds/0`)

Every stream opens with a length-prefixed postcard `StreamHello`:

| Service | Direction | Payload |
| --- | --- | --- |
| `Authz` | bi | capability grant v2/v3 (first stream in grant mode) |
| `RenewAuthz` | bi | same-session, same-scope signed lease extension |
| `Ping` | bi | nonce echo for RTT |
| `Info` | bi | agent version, services, displays |
| `TcpConnect { host, port }` | bi | raw byte splice (ssh = `127.0.0.1:22`) |
| `Desktop` | bi + uni | hello/capabilities; input events client→server; one uni stream per video frame server→client |
| `SyncTransfer { id }` | bi + uni | isolated per-transfer routing; same verified file/journal payload, one active transfer per connection |
| `Sync` (legacy direct) | bi + uni | offer/request → manifest parts → `Need` bitmap → chunk pull on 4 dedicated uni streams → `Done` |

Every uni stream leads with a `UniHello` tag frame (protocol v3). The
accepting side runs one per-connection demux (`Connection::uni_streams`)
that routes each stream to the consumer registered for its tag — a
desktop session and a sync pull can share a connection without either
stealing the other's streams off `accept_uni`. The demux accepts, then
hands each stream its own tag-read task under a 10s bound: a peer that
opens a stream and never writes its tag cannot stall the routing of the
streams queued behind it, and a claimed inbox whose consumer dropped is
reclaimable by the next `uni_streams` call.

The X11 producer synchronously borrows a completed MIT-SHM mapping through
conversion/encode, avoiding an owned native-resolution BGRA staging copy.
Owned capture consumers still receive independent snapshots; the next X11
capture cannot reuse the segment until the synchronous callback returns.

Desktop media: capture → BGRA→I420 → H.264 (OpenH264 baseline, no B-frames;
hw encoders behind a trait) → per-frame uni stream with a `FrameHeader`
`{seq, keyframe, capture_ts_ms, send_ts_ms}`. Freshness is enforced at
every stage: the producer→writer channel collapses to the newest queued
frame (a queued keyframe always survives — deltas behind it cannot decode
without it), sends are serialized and token-bucket-paced to the
controller's bitrate so QUIC's own buffer never fills with
stale-on-arrival frames, and a frame that goes stale *while its stream
is still sending* is reset mid-write (MoQ-style) — a stale delta's tail
only consumes path capacity the fresher frame needs. The reset breaks
the client's delta chain, so the producer's IDR flag is re-armed and
queued deltas are drained. The encoder's `keyframe` flag is read off the
emitted NAL units — never assumed from a schedule. Periodic IDRs are disabled;
new sessions, geometry changes and explicit reference recovery request an IDR.
Native bitrate updates preserve references instead of rebuilding the encoder. The receiver drops
anything below a "next expected seq" watermark, decodes with a
session-local decoder (a shared one would cross-contaminate reference
chains). A bounded completion-order queue preserves an unfinished reference
keyframe when a dependent delta completes first, retaining at most three
successors. A 100 ms gap initiates IDR recovery. The client also requests an IDR on a delivered-seq gap or a decode
failure, rate-limited so a corrupt stretch cannot storm. Frame stream
headers and bodies are bounded (32 MiB cap, 30s stall); every stream
leads with its `UniHello` tag under a 10s bound. The control stream
(`DesktopControl`: input events, `RequestIdr`, `SetBitrate`, heartbeats;
`DesktopEvent`: input acks, heartbeat echoes) runs at max stream
priority; frame streams sit at a constant midpoint — above QUIC's
default, strictly below control. Per-frame *escalating* priorities were
tried and removed — under load they starve in-flight sends — but a
constant rank keeps frames ahead of background traffic without frames
fighting each other. This yields decode-what-survives behavior without
a custom UDP stack.

File sync (`rds send`/`rds recv`, agent `--sync-dir`): the file is cut
by FastCDC into BLAKE3-addressed chunks — streamed (`StreamCDC`), so
manifest construction buffers one max-size chunk (the bounded manifest itself
still scales with chunk count). Transfer queues have their own finite bounds, and
disk-bound work (manifest scan, journal open/verify, assembly) runs on
the blocking pool, never an async worker. The control stream carries
`Offer`/`Request` then the manifest in ≤512-entry `ManifestPart`
batches (a 1 GiB manifest exceeds the 64 KiB frame cap). The receiver
opens a destination-bound journal under
`<dest>/.rds-sync/v2-<root>-<destination-hash>/` (verified legacy metadata is
required to resume an older content-only journal), re-verifies every
surviving part by content hash, copies matching chunks from an already-present
destination into verified parts before advertising `have` (identical resend
costs zero wire chunks), and answers
with a `Need` bitmap (≤256K chunks). The sender pushes `ChunkSet`
indices (≤4096/batch) then chunk payloads across 4 dedicated uni
streams; `SetDone`/`Done` close the session. Assembly begins only after all requested
unique indices have reached the verified journal.
Pull Offer paths must match the requested normalized path, and both sender roles
check Done against their offered root. Empty/oversized batches, noncanonical Need
bitmaps and duplicate/unrequested chunks are refused. The default absolute session
budget is one hour, with five-minute I/O stalls; library callers can select an
explicit total budget. Already-running blocking disk work can finish after
cancellation; preparation/assembly drop guards stop abandoned work at cooperative
barriers, while a rename already in progress still has an uncertain commit outcome.
Both profiles watch terminal control messages during chunk transfer without an
independent control-idle timeout; data-I/O and absolute session bounds remain. See
[the precise protocol contract](sync-protocol.md). Assembly concatenates
verified parts, checks the BLAKE3 root, and installs an exclusively created
`assembly` inode from the destination parent's private `.rds-sync` directory
by atomic rename. This keeps assembly on the destination filesystem even for
nested mount points. Parts, metadata, assembled
data and their parent directories are synced before their successful return;
an error after rename is reported as an uncertain commit, not success.
A torn or corrupt part is refetched. `rel_path` is validated lexically
(traversal, absolute, NUL, the `.rds-sync` journal namespace at every depth); every subsequent
read/write uses directory-relative no-follow operations through `rustix`.
Journal descendants, destination parents and pull source inodes are held open,
so changing a path to a symlink after admission cannot redirect I/O. The
configured root's ancestors and the local OS identity are trusted: a directory
capability continues to name the same inode after rename; this is not a sandbox
against a local process moving already-open directories out of the tree.
`Journal::assemble` consumes its journal and takes no new destination root.

The recursive-sync foundation now has four deliberately separate layers:
`rds-sync::directory` defines bounded canonical file/dir/symlink entries,
snapshot roots and identity-based diff/rename candidates, while its scanner
walks only held directory handles. The scanner sorts names by raw bytes,
opens directories and regular files with no-follow flags, records symlink
targets without traversing them, rejects special files, bounds depth/entries/
metadata and rejects mutations observed during reading. This is not an atomic
snapshot of concurrent writers; private journal namespaces are excluded at
every depth and destructive apply requires independent preconditions.
Scanning is blocking and cancellable; callers must place it on a blocking
executor. The directory wire layer splits a verified snapshot into ordered,
frame-sized borrowed parts and rechecks version, order, count, metadata budget and root
before acceptance. It is an additive data model only: no stream route or
service advertises it yet. The directory reconcile model emits a deterministic
one-way plan with an opaque destination revision precondition, explicit delete
policy, durable tombstone records and conflict previews that never choose a
winner. It still does not mutate the filesystem or journal. Recursive transfer,
metadata policy, watch/reconcile and journal GC remain separate gates, so the
single-file protocol is still the only advertised sync capability. The exact
scan, wire and planning limitations live in [directory-sync.md](directory-sync.md).

Each root has one persistent `receive.lock` inode, locked nonblockingly across
processes for a receive's lifetime. A nested destination also holds its parent's
private receive lock, so differently configured overlapping roots cannot write
the same destination concurrently. Competing receives are refused; independent
roots and destination parents remain concurrent. Root-level serialization also covers case/Unicode
aliases on supported filesystems and keeps lock storage bounded. More granular
parallel writes need a proven filesystem alias model first. Cleanup only removes
known part/metadata names through held handles; it never traverses unknown
entries. Metadata and part writes use a reserved `pending` name below private
state. Reopening while holding both locks discards only known regular, single-link
temporary files, then re-verifies committed parts. Assembly syncs its file,
renames across held directories, and syncs both destination and source parents.
Once publication is durable, cleanup failures are logged without revoking a
completed transfer. Historical random temporary files remain untouched; inactive
journal quotas/GC remain W8. See [journal recovery](sync-journal.md).
A canceled control stream ends its receive, and chunk sender tasks
are owned by a `JoinSet`. Verified chunks are written by a dedicated
blocking-pool sink behind a bounded queue. Cancellation aborts an unstarted
writer and stops a running writer between stores; normal finish drains it.
Executing syscalls retain the journal lock, so immediate retry may still be
refused while cleanup completes. Disk latency never parks
the wire pipeline; completion is counted on the wire (the peer sends
exactly the `Need` set), not on the sink's lagging counter. Every
protocol read and chunk body is bounded by a 300s stall — a peer alive
but silent aborts rather than parking the session. One sync session per
connection. Tagged transfers carry negotiated IDs and release admission before
terminal FIN; legacy direct transfers retain their separate route. Physical power-loss,
native macOS and large-file qualification remain W1.10/W8.

The new direct `rustix` dependency is a thin safe OS API for `openat`, no-follow
flags and relative rename/unlink on Linux/macOS; sync does not introduce local
unsafe blocks or external commands. Staging ownership comes from private
reserved names, exclusive creation and held receive locks.
Both versions were already present in the lockfile; no dependency versions
changed with this filesystem adapter.

### Stability measures

- Relay-first connect (works on any egress-only network), in-band
  hole-punch upgrade — both handled by iroh. `--relay` is repeatable:
  custom URL configuration alone does not prove persistent registration or
  peer-known validated standbys. Explicit bounded registration/order settings
  are described in [endpoint configuration](endpoint-configuration.md).
- QUIC connection migration survives NAT rebinding/Wi-Fi↔LTE moves.
- Agent reconnects to relay with backoff; CLI can pin `--relay`.
- QUIC transport tuning on both backends: BBRv3 congestion control
  by default, with an explicit Cubic qualification option,
  4 MiB stream receive window / 32 MiB connection send window so a
  large keyframe or sync chunk stream does not stall on high-BDP
  links (upstream defaults target ~100 Mbps × 100 ms). iroh's own
  multipath keep-alive and path idle-timeout defaults are preserved.
- Serialized frame sends + collapse + mid-send stale reset bound
  media backlog. Network recovery, shared-carrier congestion, control stream
  ordering and native scheduling can still introduce long latency tails;
  bounded queues do not establish an end-to-end latency guarantee.

- Every handshake and stream stage is stall-bounded: agent stream hello
  15s, desktop session ack / frame stream header+body 30s, relay
  register 15s, uni-stream tag 10s, CLI acks 15s and connect 30s,
  sync protocol reads 300s. A peer that opens a stream and goes silent
  costs seconds, not a parked task for the connection's lifetime.
- Agent state mutexes recover from poisoning (`into_inner`) — one
  panicked holder cannot deny service forever, and request paths refuse
  explicitly instead of `unreachable!`.

BBRv3 now replaces its nominal 1ms startup pacing rate once it has an actual
tracked-packet RTT. The first sample uses packet departure/ACK time: Noq updates
the transport RTT estimator after congestion callbacks. This keeps a sparse
flow from retaining a rate derived from the nominal RTT throughout STARTUP.
The shared engine applies the correction to Iroh and owned Noq; ordinary
bandwidth growth, stream ordering and loss-detection thresholds are unchanged.

### Observability

All executable entrypoints now share [bounded process telemetry](observability.md)
through `rds-observe`: stderr text for private local debugging or an allowlisted
schema-1 JSON export. Random process IDs and numeric agent connection IDs
correlate events without raw peer keys. Vector/OpenObserve configuration,
log-derived metrics and disabled alert definitions accompany an opt-in real
pipeline regression. Agent/relay/server now expose aggregate source metrics on
a separate opt-in, authenticated loopback listener. Distributed traces, support
bundles, complete source coverage and estate rollout remain open.

`rds-net::metrics` gives every endpoint a [`Registry`] of atomic
counters; a per-connection `ConnSampler` diffs cumulative
observed path counters into direct/relay buckets. Noq uses shared weak metadata
from the validated-path policy, with explicit event-loss and observer-liveness
coverage; it never scans a guessed numeric ID range. Sampling can miss short
paths and final retired-path increments. Counters: connections opened/accepted,
datagrams and bytes sent/lost per path kind, congestion events, paths
seen, QNT attempts/success (driven by the noq policy driver — iroh
does not expose its hole-punch attempts, where
`paths_seen{via="direct"}` appearing after a relay-only start is the
equivalent signal). Gauges: active connections, selected-path RTT,
cwnd, observed live paths, policy-observed/degraded connections and selection
validity; lost path events are cumulative. Unknown selection does not substitute
a historical or arbitrary path. The last sample's RTT/cwnd/validity are stored
together. See [path observation contract](path-telemetry.md).
`Registry::render_prometheus` emits text exposition
behind the `metrics` feature — no prometheus dependency.

`rds-observe::admin` owns bounded HTTP/1.1 handling, private token-file loading,
constant-time bearer authentication and numeric Prometheus exposition. Daemon
startup validates the optional listener before creating identity/catalog state;
supervision joins its request tasks at shutdown. Component observers retain
counters or weak references only. The directory's public `/v1/metrics` route
was removed after a real loopback-proxy regression reproduced exposure. Stable
writer hash labels are no longer exported. The iroh relay reports unsupported
source coverage explicitly, rather than supplying zero forwarding counters.
See [the admin contract](observability.md#authenticated-admin-metrics-o2).

Session logging is structured `tracing`: every agent connection runs
inside an `rds.conn` span carrying `peer` and a monotonic
`session_id`; each service stream nests an `rds.stream{service}` span
under it, and the sync engine logs accept/complete events with byte
and chunk counts, so `session_id` filters a whole session across
services. Bench reports embed the endpoint registry snapshot they ran
against (`metrics:` block, `client_`/`agent_` prefixed). Each report states its
measurement or regression-test scope; sampled counters do not prove complete
wire accounting. Historical Noq path evidence predating the telemetry fix
requires a new run for qualification.

## Product scope and milestones

[Roadmap](roadmap.md) owns the milestone definitions; the
[stability plan](stability-plan-20261009.md) owns the current execution order.
The code supplies a shared endpoint/session manager, signed discovery and
grants, native russh client access through a scoped TCP adapter, durable
resumable single-file transfer, and an experimental X11/OpenH264/native viewer.
Audio, recursive/two-way synchronization and native macOS serving remain
unfinished. Optional consented Wayland serving has a prepared backend and
deterministic boundary tests, with native acceptance still separate. Hardware codecs, RDP/browser interop and Windows support
retain their separate later requirements.

### Native viewer stability, video profile and text paste increment

The optional native workspace adds an egui layer to the existing winit/wgpu
presenter. `render::workspace` owns bounded tab intent and settings;
`render::viewer::workspace` owns session mailboxes/input focus and the visible
surface; `rds-cli::desktop::workspace` owns managed network workers and private
preferences. Dependencies still point down the crate map. No new service or
wire version is introduced. Each device's tabs retain a shared authenticated
peer binding through settings/reconnect; only the focused tab routes input and
publishes clipboard. The workspace can collapse its panels to one local restore
arrow without reserving viewport height. Pointer ownership uses the same painted
physical geometry; UI and clipboard deadlines share the native event loop.
Presentation selects tear-free Mailbox or portable Fifo, and an existing native
redraw never schedules itself again. See [native viewer](native-viewer.md#tabbed-workspace).

The additive `DesktopV3` remote greeting and `DesktopProfile` local IPC command
carry a per-session video-height choice while keeping prior tags and formats.
See [the native contract](native-viewer.md) for default Full HD, bounds and
coordinated agent migration. Text paste uses bounded, ordered control chunks and
an X11 session-owned CLIPBOARD service with INCR; clipboard contents never enter
observability. `DesktopV5` adds an offer/request/chunk reverse path; the viewer
publishes a fully reassembled value through its native clipboard. Native macOS
access runs on the AppKit main thread and the reverse path is explicitly
negotiated.

Managed desktop IPC has one bounded read worker independent of canceled caller
waits. The renderer acquires a usable surface before queuing GPU writes. Private
rotating startup logs and live snapshots distinguish dispatch, transport, decode,
GPU submission and OS occlusion; stages remain measurements, not physical pixels.

Native diagnostics retain bounded metadata incident windows independently of
ordinary log rotation. Local viewer epochs disambiguate reconnects, canceled
ACK tracking remains counted, and slow ACKs are visible at the default warning
level. Serving capture, pacing, media writers and receipts inherit their owning
tracing span across task boundaries. Opt-in controlled visual response markers
measure native input to submission of the matching response; the ordinary
viewer does not inspect markers. See [the measurement receipt](reports/rds-causal-visual-probe-20261004.md).

### Client transmit observation after a delayed control response

The [passive reply-read observer](reports/rds-passive-control-read-20261006.md)
records framing progress during a control gap. Its independent task never polls
the awaiting I/O future, queries transport locks or changes liveness; matched
newer probes exclude older missing echoes from diagnostic freshness. Progress
records contain lengths/counts and local time only. Qualified installed evidence
remains separate from this observational boundary.

`rds-net::WeakPathObserver` exposes backend-neutral live-path metadata without
retaining the connection facade or transport I/O. Desktop control responses use
a separate session-owned observation worker: one running blocking query and one
newest queued request. Input/receipt writes and event reads never await it. Drop
requests task cancellation; an already-running synchronous backend query may
finish. Diagnostic failure does not terminate the desktop session.

The worker retains at most eight current path baselines, prioritizing selected
paths and reporting truncation/coverage. Slow responses report local transmit
loss/cwnd/bytes and interval deltas, with absent/reset baselines explicitly
unknown. Reply-to-observation age, query work time and evictions qualify those
snapshots. The counters describe this endpoint's TX, not the peer's TX or outer
tunnel losses. Numeric process-local control-instance IDs join write/read/path
metadata; no key, address, route cookie, clipboard or typed body is logged.

The shared postcard frame writer now serializes the four-byte length prefix and
body into one buffer before one `write_all` operation. The receiver's wire bytes,
64 KiB bound, partial-write semantics and error kinds are unchanged. This avoids
a separate prefix-only transport admission/wakeup; it does not guarantee one
network packet, atomic delivery or cancellation-safe writes.

### Optional latency path ranking

EndpointSettings/EndpointConfig PathPreference is lowered in the owning backend.
Iroh latency mode ranks existing candidate RTTs without transport tiers and uses
5ms switching stickiness. A bounded1s optional actor refresh makes the decision
respond to changing RTT while topology is unchanged. The exact published Iroh1.3
source carries this narrow default-None hook under vendor/iroh, excluded from
the RDS workspace; provenance and licenses are retained. Existing/default and
explicitly pinned selectors keep their original callback behavior. Noq already
owns periodic validated-path ranking and needs no policy rewrite for this mode.
This remains a staged qualification increment, not a new default or native pass.


### STREAM work and independent standby acknowledgement

Latency retirement reads unacknowledged STREAM metadata from Noq's existing
sent-packet state. Probe-only debt does not authorize path retirement. This
works for a previously selected path after demotion without selection markers,
payload parsing or application replay. The normal CCA adapter stays passive.
Pending debt observes Noq's ACK-eliciting on_packet_sent callback, rather than
on_sent batches which also include ACK-only transmits. Both delegate unchanged.
When ACK proof grows stale, the existing bounded periodic selector probes again
even if an earlier probe is still outstanding.

Latency preference also enables default-false same-path ACK scheduling in
the vendored published Noq-proto 1.3 source. A validated backup can return its
ACK without depending on the selected data path. ACKs for abandoned/unvalidated
paths can use another route; single-path/handshake scheduling is unchanged.
No wire, crypto, congestion or reliable-retransmission algorithm changes.
Iroh's initial observer subscribes before reading a protocol-owned snapshot of
established path IDs. A delayed accepting task can otherwise miss a standby
that finished validation before actor registration. The small
[Noq adapter extension](../vendor/noq/RDS-PATCH.md) forwards this snapshot;
duplicate snapshot/event observations do not double-count paths. This does not
replace the engine's validation or packet scheduling decisions.

See [patch provenance](../vendor/noq-proto/RDS-PATCH.md) and
[failure/qualification evidence](reports/rds-warm-standby-20261006.md).

### Prepared consented Wayland serving

The optional Linux portal feature adds an agent-owned DesktopSource seam and a
consented multi-monitor PipeWire/EI backend. Native types remain below serving
policy; local preparation precedes remote admission and revocation seals leases.
See [the Wayland contract](wayland-portal.md) for requirements, identity, cleanup,
fixture coverage and separate native acceptance.

Record-store anchor writes now retain an exact pending database successor only
for classified ENOSPC/quota failures after a successful durable database commit.
A later access keeps both exclusive locks, verifies the current database metadata
and the previous-or-successor anchor, then synchronizes that exact anchor before
serving records or allocating another revision. Repeated capacity failures remain
unavailable. Changed state and unrelated I/O permanently close this recovery path
until a validated reopen. Signed lease/tombstone history and admission do not reset.
