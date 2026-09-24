# remote-device-sync architecture

Read this design together with the [2026-09-24 implementation audit](reports/rds-audit-20260924.md)
and [remediation plan](remediation-plan.md). They distinguish implemented
behavior, known defects and planned capabilities; design claims below are
not a substitute for the current acceptance evidence.

Remote access for GDS devices: SSH reachability and remote desktop sessions
between any two enrolled devices, assisted by a GDS-operated server.
Design goals, in order: **minimum interactive latency**, **maximum
connection stability**, defense in depth, no inbound firewall changes.

Status: v0.1 foundation. This document records the protocol and stack
research and the decisions that fall out of it. The deeper second-pass
research — iroh 1.2/noq internals (multipath, path selectors, hooks),
capture/codec/input crate matrix, GDS server composition, and the
updated build order — lives in [research.md](research.md).

CLI and agent share [versioned endpoint configuration](endpoint-configuration.md)
with explicit file/flag precedence, backend/relay validation and preflight before
identity creation. The owned relay uses a separately pinned public identity.
TCP service flags, client requests and agent policy use the same canonical
`rds-core::TcpTarget`; IPv6 spelling and IPv4-mapped addresses normalize before
policy comparison and dialing. Parsing has no DNS or socket side effects.
Role-level authority/service policy, negotiated session limits and lifecycle
configuration remain W2 work; the endpoint schema does not claim to cover them.

Owned noq endpoints now track path-policy tasks. A weak QUIC closure notification
ends each driver without retaining the connection; endpoint close seals task
admission, closes QUIC connections and waits for policy-task cleanup. Streams
may legitimately outlive a Connection wrapper. Completed drivers are removed
immediately, and `active_path_drivers()` exposes the owned endpoint's count.
This boundary does not yet own every agent, relay or disk task; see the
[lifecycle receipt](reports/rds-driver-lifecycle-20260925.md) for its tested scope.

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
- **SSH**: do not implement SSH. Forward TCP to the host `sshd` over a QUIC
  stream — the user's ssh client keeps its own auth, keys, agent.

## Architecture

```text
┌──────────┐   direct QUIC (hole-punched)   ┌──────────┐
│  rds cli │◄──────────────────────────────►│ rds-agent│
│ (viewer, │   or relayed via the relay     │ (daemon, │
│  ssh -L) │◄────────────►┌──────────┐◄────►│  host)   │
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

```text
                     rds-core           types, framing, tokens — leaf
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
   proto+     capture/codec/ Opus paths   FastCDC+BLAKE3
   iroh shim  input/render   (scaffold)   manifests/delta
   └──────────┴──────────────┴───────────┘
                       ▼
        ┌──────────────────────────────┐
        ▼                              ▼
   rds-agent (daemon)            rds-cli (operator)
                       ▼
   rds-server — GDS services host: relay + discovery + registry
```

- **`rds-server`** — runs on the GDS services host: the packet relay and
  the signed-record discovery directory; later the registry bridge into
  estate state, presence and audit. Sees only encrypted traffic.
- **`rds-agent`** — daemon on each controlled device. Binds the endpoint
  (Ed25519 identity persisted), connects to its home relay, accepts
  `rds/0` connections, serves streams to an allowlist of peers.
- **`rds`** — operator CLI. `rds id`, `rds ticket`, `rds ping`, `rds ssh`,
  `rds forward`, `rds desktop` (feature-gated); `rds send/recv` planned
  on the sync engine.

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
  non-replayable across concurrent connections (`active_grants`), and
  revocable: the directory serves an estate-signed `SignedRevocations`
  snapshot at `GET /v1/revocations`, agents poll it into their denylist,
  and a revoked or expired grant closes its live connection.
- Grant admission uses one connection-owned state machine. Pending service
  requests remain refused during the Authz reply; its watchdog and replay
  reservation exist before the reply write. Failure or cancellation closes
  the connection and releases the lease. Denylist values survive without
  watchers; service admission rechecks revocation and expiry directly.
- GDS names: `GET /v1/names/{name}` returns one `SignedNameBinding`, signed
  by the registry issuer over `rds/name-binding/v2\0` plus the postcard
  payload `{stamp, registry_digest, version, name, key, issued_at, expires_at}`.
  The client requires
  an independently provisioned trust anchor (`rds --registry-key <base32>`),
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
  in a bounded blocking pool; permits remain held through disk completion even
  after request timeout. Endpoint records and delete tombstones now share an
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
| `Authz` | bi | capability grant (first stream in grant mode) |
| `Ping` | bi | nonce echo for RTT |
| `Info` | bi | agent version, services, displays |
| `TcpConnect { host, port }` | bi | raw byte splice (ssh = `127.0.0.1:22`) |
| `Desktop` | bi + uni | hello/capabilities; input events client→server; one uni stream per video frame server→client |
| `Sync` | bi + uni | offer/request → manifest parts → `Need` bitmap → chunk pull on 4 dedicated uni streams → `Done` |

Every uni stream leads with a `UniHello` tag frame (protocol v3). The
accepting side runs one per-connection demux (`Connection::uni_streams`)
that routes each stream to the consumer registered for its tag — a
desktop session and a sync pull can share a connection without either
stealing the other's streams off `accept_uni`. The demux accepts, then
hands each stream its own tag-read task under a 10s bound: a peer that
opens a stream and never writes its tag cannot stall the routing of the
streams queued behind it, and a claimed inbox whose consumer dropped is
reclaimable by the next `uni_streams` call.

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
emitted NAL units (forced IDRs, periodic IDRs at the configured
~8s `intra_frame_period`, and encoder rebuilds on a >15% bitrate change
all mark real IDRs) — never assumed from a schedule. The receiver drops
anything below a "next expected seq" watermark, decodes with a
session-local decoder (a shared one would cross-contaminate reference
chains), and auto-requests an IDR on a delivered-seq gap or a decode
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
opens a journal under `<dest>/.rds-sync/<root>/`, re-verifies every
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
cancellation, so commit outcome may remain uncertain. See
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
blocking-pool sink behind a bounded queue, so disk latency never parks
the wire pipeline; completion is counted on the wire (the peer sends
exactly the `Need` set), not on the sink's lagging counter. Every
protocol read and chunk body is bounded by a 300s stall — a peer alive
but silent aborts rather than parking the session. One sync session per
connection. Explicit transfer IDs/negotiation remain W1.9/W2; physical power-loss,
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
  multiple custom relays give automatic client-side failover, which is
  the iroh-recommended production topology (≥2 relays).
- QUIC connection migration survives NAT rebinding/Wi-Fi↔LTE moves.
- Agent reconnects to relay with backoff; CLI can pin `--relay`.
- QUIC transport tuning on both backends: BBRv3 congestion control
  (paced, bufferbloat-resistant — vs loss-based Cubic default),
  4 MiB stream receive window / 32 MiB connection send window so a
  large keyframe or sync chunk stream does not stall on high-BDP
  links (upstream defaults target ~100 Mbps × 100 ms). iroh's own
  multipath keep-alive and path idle-timeout defaults are preserved.
- Serialized frame sends + collapse + mid-send stale reset bound
  worst-case latency under loss: queues stay near-empty and the
  residual tail is retransmit physics, not queueing.
- Every handshake and stream stage is stall-bounded: agent stream hello
  15s, desktop session ack / frame stream header+body 30s, relay
  register 15s, uni-stream tag 10s, CLI acks 15s and connect 30s,
  sync protocol reads 300s. A peer that opens a stream and goes silent
  costs seconds, not a parked task for the connection's lifetime.
- Agent state mutexes recover from poisoning (`into_inner`) — one
  panicked holder cannot deny service forever, and request paths refuse
  explicitly instead of `unreachable!`.

### Observability

`rds-net::metrics` gives every endpoint a [`Registry`] of atomic
counters; a per-connection `ConnSampler` diffs cumulative
`path_stats()` into it, so the `via="direct"`/`via="relay"` split stays
exact across path migration. Counters: connections opened/accepted,
datagrams and bytes sent/lost per path kind, congestion events, paths
seen, QNT attempts/success (driven by the noq policy driver — iroh
does not expose its hole-punch attempts, where
`paths_seen{via="direct"}` appearing after a relay-only start is the
equivalent signal). Gauges: active connections, selected-path RTT,
cwnd, live paths. `Registry::render_prometheus` emits text exposition
behind the `metrics` feature — no prometheus dependency.

`rds-server` serves `GET /v1/metrics` on the directory listener with
per-endpoint PUT counters labelled by a 16-hex BLAKE3 prefix of the
writer key — raw keys, peer addresses and content never appear. The
route answers loopback peers only (everyone else gets 404): remote
scraping goes over SSH or a local exporter.

Session logging is structured `tracing`: every agent connection runs
inside an `rds.conn` span carrying `peer` and a monotonic
`session_id`; each service stream nests an `rds.stream{service}` span
under it, and the sync engine logs accept/complete events with byte
and chunk counts, so `session_id` filters a whole session across
services. Bench reports embed the endpoint registry snapshot they ran
against (`metrics:` block, `client_`/`agent_` prefixed) — every number
in `docs/reports/` comes from the harness reading these counters.

## Milestones

1. **v0.1 (this)**: workspace, rendezvous/relay, auth allowlist, `ping`,
   `ssh`/TCP forward E2E, desktop pipeline traits + X11 capture/encode/
   input behind the `desktop` feature, architecture doc.
2. **v0.2**: GDS discovery + authz — `iroh-dns-server` on gds-services,
   `EndpointHooks` allowlist, signed `device_id`↔`EndpointId` registry,
   `rds ssh <device-name>`; damage-driven (VFR) capture replacing the
   fixed-fps loop; wgpu client render.
3. **v0.3**: hardware encode (`cros-codecs` VA-API/V4L2, `gpu-video`
   Vulkan path), `wdotool-core`/portal-EIS input, audio (opus),
   clipboard; `rds send/recv` via iroh-blobs, registry replication via
   iroh-docs.
4. **v0.4**: multi-relay failover + custom `PathSelector`, adaptive
   bitrate from path congestion state, AV1 tier, RDP frontend via
   `ironrdp-server`, browser client via WebRTC if needed.

## Non-goals for v0.1

- No SSH protocol implementation (TCP forward only).
- No unattended access control model beyond the EndpointId allowlist.
- No file transfer, audio, multi-monitor.
