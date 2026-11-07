# Capability and requirement matrix

matrix_version: 1

Claim registry for the implemented build, checked against code and tests.
The implementation is the source of truth; a row does not promote a stub or
replace platform/product acceptance. States:

- `implemented` — code landed and covered by tests/CI on a supported
  platform. Does not imply production or WAN qualification.
- `experimental` — code landed, real but partially qualified; gates,
  features or runtime probes may refuse it.
- `stub` — API/wire shape reserved; probing returns "not available" and
  no bytes move.
- `unavailable` — no code behind the name; roadmap only.

Runtime prerequisites are recorded separately from state: they describe
what a host/session must provide for a non-stub row to actually work.
Tests in `crates/rds-cli` and `crates/rds-bench` enforce that every
`ServiceKind` variant and every bench lane name has exactly one row
below; `README.md` must link this file.

## Services (agent `Info` advertisement and grant scopes)

The data-plane services are gated by deployment policy — `role`, `services`
and `disabled_services` in [agent configuration](agent-configuration.md), or
`--role`/`--service`/`--no-service` flags. A disabled service is refused by
name before grant machinery runs, and `Info`/directory announcements list
only what is actually served. `ping`/`info` are the always-on control plane.

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| service:ping | implemented | peer on `--allow` list | `rds-net` tests, CI |
| service:info | implemented | peer on `--allow` list | `rds-cli` tests, CI |
| service:tcp | implemented | enabled + `--allow` + grant scope for tcp | `rds-ssh` e2e, CI |
| service:desktop | experimental | enabled + `desktop` build flag; usable capture backend; grant scope | capture→encode→decode→stats contract tests |
| service:desktop-managed | implemented | manager-owned remote session relays encoded frames over local IPC v5; viewer decodes via `RelayDecoder` (`desktop` build flag for real decode) | relay e2e + managed-channel tests; `rds desktop` defaults to it, `--direct` kept |
| service:sync | implemented | enabled + `--sync-dir` configured | `rds-sync` tests, resumable transfer tests |
| service:audio | stub | [validated libopus/jitter core](audio.md) exists; native device I/O and service wiring unavailable | `rds-audio` packet/codec/reorder tests; no advertised audio service |
| service:sync-read | implemented | grant scope on sync root; optional grant `sync_paths` subtree | grant/scope tests |
| service:sync-write | implemented | grant scope on sync root; optional grant `sync_paths` subtree | grant/scope tests |
| service:desktop-view | implemented | grant scope; usable capture backend | view/control scope separation and desktop contract tests |
| service:desktop-control | experimental | desktop-view + control scope; X11 or explicitly consented EI sink | native Xvfb input/lifecycle tests; installed causal pixels retain a separate gate |

## Transports

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| transport:iroh | implemented | default backend | CI both platforms |
| transport:noq | experimental | build `transport-noq` feature; `--backend noq` | bench + CI feature lane; WAN unqualified |
| transport:owned-relay | experimental | `rds-relay` deployment; `--owned-relay` pins | relay runtime tests; parity/migration open (W3) |

## Desktop backends (`rds-desktop` probe order)

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| desktop-backend:x11 | experimental | `x11` feature; X11 display socket; x11rb connect ok | xvfb contract tests; CI x11 lane |
| desktop-backend:image-copy | stub | ext-image-copy-capture-v1 compositor; backend unwritten | `probe()` returns `Ok(None)` |
| desktop-backend:kms | stub | DRM/KMS seat; backend unwritten | `probe()` returns `Ok(None)` |
| desktop-backend:pipewire | experimental | Linux `portal` feature; explicitly prepared local RemoteDesktop/ScreenCast permission; mapped BGRA/BGRx floor | [Wayland contract](wayland-portal.md); [native two-monitor capture, restore and EI sync](reports/rds-wayland-native-20261011.md); physical input, revocation and installed acceptance separate |
| desktop-backend:screencapturekit | experimental | macOS 13+, `host` feature, locally prepared Screen Recording and Accessibility grants | bounded native owner and shared software producer; native screen/input/restart qualification required |
| desktop-backend:dxgi | unavailable | Windows host; no module | roadmap only |

## Desktop session features

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| desktop:clipboard-text-reverse | experimental | `DesktopV5`; X11 or consented Clipboard v1 Wayland serving, AppKit viewer; UTF-8 text only; Linux viewer publication unavailable | bounded offer/request/chunk and portal FD tests; native Xvfb/AppKit probes; installed portal qualification separate |
| desktop:multi-monitor-x11 | experimental | RandR monitor objects or multi-root X server; fresh session after layout changes | native RandR known-pixel/input/removal fixture; physical desktop qualification separate |
| desktop:multi-session | implemented | per-session route IDs; local manager ≤32 sessions; agent connection/service limits | session routing and managed-channel tests |
| desktop:workspace-tabs | implemented | native `viewer` surface; up to eight tabs across actual device/display inventories; each remote needs its own authorization | workspace model, managed native routing and idle-frame restoration tests; physical multi-PC acceptance separate |
| desktop:workspace-profiles | implemented | version-1 `viewer-workspace.json`; per-display geometry/FPS/input/clipboard/receipt settings; existing grant paths only | bounded preference/launch tests; [viewer contract](native-viewer.md#tabbed-workspace) |
| desktop:workspace-panels | implemented | local up/down arrows or Ctrl+Shift+H; window-scoped state and full-window aspect-preserving mapping | layout/HiDPI/pointer ownership tests and native still-image fixture |
| desktop:workspace-spaces | experimental | saved `presentation: spaces` or `--spaces`; one fullscreen window/surface per monitor in one process, up to eight; macOS native Spaces swipe | common profile/model and copy-fence tests; native ordering/closure qualification separate |

## Codecs and presentation

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| codec:openh264 | implemented | none (software) | encode/decode tests |
| codec:vaapi | stub | libva driver; unwritten | doc-only module |
| codec:vulkan-video | stub | ash `vk::Video*`; unwritten | doc-only module |
| codec:videotoolbox | stub | macOS; unwritten | doc-only module |
| render:wgpu | experimental | `viewer` build feature; native window and usable GPU surface | native window, bounded newest-frame presentation; [viewer contract](native-viewer.md) |

## Platforms

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| platform:linux-x86_64 | implemented | — | CI ubuntu lane |
| platform:macos-arm64 | experimental | native viewer; prepared ScreenCaptureKit/CGEvent host requires native qualification; Developer ID distribution/notarization open | CI macos lane compiles+tests, Metal viewer qualification |

## Discovery, policy, observability

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| discovery:directory | experimental | `--directory`, `--directory-allow` publishers; HTTPS or explicit http | discovery tests; WAN unqualified |
| policy:grants-v2 | implemented | `--issuer` + grant files; `--revocations-key` for managed mode | grant/lease tests |
| policy:grants-v3 | implemented | same surface; adds `tenant`/`policy_revision` claims and `sync_paths` scope; agents pin via `--tenant`/`--policy-min-revision` | grant unit tests + e2e binding/scope suite; v2 payloads still verify |
| policy:gds-issuance | unavailable | estate GDS service wiring; not in this module | roadmap; `session renew` is manual-file only |
| observability:export | experimental | `RDS_LOG_FORMAT=json`; Vector/OpenObserve pipeline | fixture pipeline, `docs/observability.md` |

## Measurement lanes (`rds-bench` report scenario names)

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| measure:handshake | implemented | loopback agent | `docs/reports/` |
| measure:ping | implemented | loopback agent | `docs/reports/` |
| measure:transfer-receiver-ack-v1 | implemented | loopback agent + verified-receipt TCP target | `docs/reports/` |
| measure:multiconnect | implemented | impairment profile | `docs/reports/` |
| measure:relay-fallback | implemented | local iroh relay | `docs/reports/` |
| measure:impaired | implemented | impairment profile | `docs/reports/` |
| measure:resolve-connect | implemented | local registry fixture | `docs/reports/` |
| measure:migration | implemented | two local owned relays (noq); iroh lane reports SKIPPED | `docs/reports/` |
| measure:calibration | implemented | rate-cap impairment below the loopback ceiling; noq socket pacing or iroh proxy | `docs/reports/` |
| measure:recovery | implemented | live socket impairment (noq); iroh lane reports SKIPPED | `docs/reports/` |

Notes:

- `service:desktop` advertised by `Info` means the flag is on; a session
  still fails closed when no capture backend probes usable. X11 init
  failure is an explicit `Err`, and the x11 CI lane fails when the
  feature cannot initialize.
- `service:desktop-managed` is the local-IPC path: the manager forwards
  encoded frames and controls without decoding, so it works on headless
  manager builds; the *remote* still needs a capture backend, and the
  viewer build needs `rds-desktop/x11` for real decode.
- Historical `transfer` reports predate receiver-verified timing. The current
  verified lane is `transfer-receiver-ack-v1`; older sender-finish reports remain
  incomparable under [the benchmark contract](benchmark-transfer.md).
