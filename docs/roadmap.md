# Roadmap

The [native clipboard and resource quality plan](quality-plan.md) defines the
current text/image/file clipboard, low-write diagnostics and hardware-media
increment. Its native acceptance remains open under issue210.

Current execution order is in the [2026-10-09 stability plan](stability-plan-20261009.md).
Reopened acceptance gates remain in the [2026-09-24 remediation plan](remediation-plan.md), backed by the
[implementation/session audit](reports/rds-audit-20260924.md).
The milestones below preserve the original direction; historical checkpoint
completion does not establish that the audited defects or product gaps are closed.

The 2026-10-08 completion wave first landed the bounded `rds-audio` packet,
libopus and jitter core. Device capture/playback, agent admission and viewer
playout remain separate until their platform and authorization gates pass; see
the [completion workstreams](completion-plan-20261008.md) and the
[scoped report](reports/rds-audio-core-20261008.md).

The [native viewer](native-viewer.md) now implements W6.3 window/input and
newest-frame GPU presentation, with bounded reconnect and media-gap recovery.
The X11 serving increment removes fixed idle polling and supports explicit
software downscaling. This advances W6.1–W6.4/W6.6/W6.7; it does not close the
v0.3 hardware-codec, geometry, quality or glass-to-glass acceptance gates.

The [borrowed X11 capture increment](reports/rds-desktop-cpu-20261008.md)
removes the native-resolution staging copy before synchronous software encode.
Its isolated conversion measurement advances W6.1 CPU work; it retains the
existing codec quality, FPS and recovery contracts and does not close those gates.

The explicit [validated-payload receipt increment](reports/rds-payload-receipts-20261005.md)
advances W6.8/WS5 delivery: bounded sender admission follows an exact complete-read
proof instead of a potentially delayed FIN acknowledgement. Legacy behavior and
reference/deadline limits remain. Installed mixed-load/idle latency and stability
gates are still required.

The [native SSH client](ssh.md) now uses russh and standard remote PTY requests
over managed/direct TCP streams. W5 remains open for host/account enrollment,
broker/reattachment and macOS/real-network/mixed-load qualification.

[Destination-bound renewable grants](grant-leases.md) now cover explicit
connection renewal and stable-ID revocation. W2.3/W4.2 remain open for policy
and tenant scopes, automatic issuer integration and platform/network acceptance.

The [observability sequence](observability.md) now accompanies remediation:
shared Rust telemetry, Vector/OpenObserve qualification and authenticated
aggregate source metrics now have local Linux evidence. Remaining source
coverage, diagnostic bundles and private operational rollout retain their
acceptance gates; the local pipeline does not close product or release gates.

Each workstream ends at a named **checkpoint** — a reproducible
verification bundle (functional, simulation, impairment, interop,
fuzz, soak, security layers as applicable) whose report is committed
to `docs/reports/` before the next wave starts. Order is chosen so
the riskiest owned layers (transport, relay) are proven early. The
task-level plan, checkpoint protocol and per-gate check tables live
in [implementation-plan.md](implementation-plan.md).

## v0.1 — foundation (done)

Working SSH/TCP forwarding over direct and relayed QUIC (iroh backend),
endpoint allowlists, X11/OpenH264 desktop skeleton, workspace structure.

## v0.2 — owned connectivity core

- `rds-net::backends::noq`: socket mux over `AsyncUdpSocket`, endpoint
  manager, path-open policy, `PathEvents`/`NatTraversalUpdates` wiring.
- `rds-relay::proto`: our relay protocol (REGISTER/FORWARD, datagram
  channel); `rds-server` serves it beside the iroh relay.
- `rds-discovery`: `rds-server` exposes PUT/GET of signed
  `EndpointRecord`s; agent publishes; `rds ssh <device>` resolves
  through GDS instead of pasted tickets.
- Authz: connection-level check (equivalent of `EndpointHooks::
  after_handshake` in our layer).
- **Gates**: same-harness parity vs iroh backend — setup time,
  time-to-direct, RTT, relayed throughput, migration survival; feature
  flag picks backend per run.

## v0.3 — media pipeline, phase 1

- Damage-driven capture (VFR: encode only what changed) — the single
  biggest latency/bandwidth win.
- Linux: `ext-image-copy-capture` backend, then DRM/KMS unattended path.
- macOS: ScreenCaptureKit + VideoToolbox backend (new platform bring-up).
- wgpu renderer (newest-frame-only), Vulkan Video H.264 encode/decode
  behind probe; OpenH264 stays the floor.
- Input: portal EIS (`reis`) on Wayland; `CGEvent` on macOS.
- Text clipboard now has a bounded negotiated reverse path on X11/AppKit;
  RandR-backed X11 monitor selection and per-session concurrent viewers are
  implemented behind their platform qualification gates. The optional explicit
  consented Wayland capture/EI increment has deterministic boundary coverage;
  native acceptance is separate. Optional consented Wayland text clipboard is
  implemented; native Copy/Paste acceptance remains open. Stable hotplug
  identities and rich/file formats remain later capabilities.
- **Gates**: glass-to-glass ≤20 ms LAN / ≤80 ms WAN at 1080p60
  measured end-to-end; input-to-visible-pixel budget tracked.

## v0.4 — sync and resilience

The explicit bounded persistent-Iroh registration increment advances multi-relay
readiness. Isolated same-stream/idle fault evidence and installed three-node
qualification are required before claiming the failover gate.

- `rds-sync` transfer protocol on our streams: manifest exchange,
  `missing_chunks` delta, resume journal; `rds send/recv`.
- Multi-relay map + failover, drain signalling, relay metrics surface.
- Adaptive bitrate from per-path congestion state; FEC behind measured
  loss; audio (`rds-audio` Opus path).
- **Gates**: loss/jitter harness (`netem`) — 5% loss, 30 ms jitter:
  desktop stays interactive; sync resumes mid-transfer; relay failover
  does not drop sessions.

## v0.5+ — interop and breadth

- RDP frontend (`ironrdp-server`) for stock RDP clients.
- Windows support (DXGI + MF/D3D11 + SendInput) behind existing seams.
- Browser client (WebRTC/str0m gap-fill) only if a real need appears.
- HEVC/AV1 negotiated tiers; multi-viewer broadcast (moq-lite) only if
  the product needs it.

## Standing quality bars

- CI green on `ubuntu-latest` + `macos-latest`; fmt/clippy/test +
  `cargo deny` licenses.
- Every milestone ends with a `docs/reports/` entry: what was measured,
  on what network, p50/p99 numbers.
- `docs/research.md` §7 risk register revisited at each gate.
