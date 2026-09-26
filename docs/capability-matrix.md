# Capability and requirement matrix

matrix_version: 1

Single source of truth for what this build claims to do. States:

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

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| service:ping | implemented | peer on `--allow` list | `rds-net` tests, CI |
| service:info | implemented | peer on `--allow` list | `rds-cli` tests, CI |
| service:tcp | implemented | `--allow` + grant scope for tcp | `rds-ssh` e2e, CI |
| service:desktop | experimental | `desktop` build flag; usable capture backend; grant scope | capture→encode→decode→stats contract tests; no viewer |
| service:sync | implemented | `--sync-dir` configured | `rds-sync` tests, resumable transfer tests |
| service:audio | stub | no codec; wire shape reserved in v2 | `ServiceKind::Audio` variant only |
| service:sync-read | implemented | grant scope on sync root | grant/scope tests |
| service:sync-write | implemented | grant scope on sync root | grant/scope tests |
| service:desktop-view | implemented | grant scope; usable capture backend | scope tests; headless path only |
| service:desktop-control | experimental | desktop-view + control scope; X11 input sink | input contract tests; real-session injection unqualified |

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
| desktop-backend:pipewire | stub | XDG portal consent; backend unwritten | `probe()` returns `Ok(None)` |
| desktop-backend:screencapturekit | stub | macOS 12.3+, screen-record permission; backend unwritten | `probe()` returns `Ok(None)` |
| desktop-backend:dxgi | unavailable | Windows host; no module | roadmap only |

## Codecs and presentation

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| codec:openh264 | implemented | none (software) | encode/decode tests |
| codec:vaapi | stub | libva driver; unwritten | doc-only module |
| codec:vulkan-video | stub | ash `vk::Video*`; unwritten | doc-only module |
| codec:videotoolbox | stub | macOS; unwritten | doc-only module |
| render:wgpu | stub | GPU surface; unwritten | `available()` returns `false` |

## Platforms

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| platform:linux-x86_64 | implemented | — | CI ubuntu lane |
| platform:macos-arm64 | experimental | macOS runner/hardware; desktop stubbed; signing/notarization open | CI macos lane compiles+tests |

## Discovery, policy, observability

| Capability | State | Runtime prerequisites | Evidence |
|---|---|---|---|
| discovery:directory | experimental | `--directory`, `--directory-allow` publishers; HTTPS or explicit http | discovery tests; WAN unqualified |
| policy:grants-v2 | implemented | `--issuer` + grant files; `--revocations-key` for managed mode | grant/lease tests |
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

Notes:

- `service:desktop` advertised by `Info` means the flag is on; a session
  still fails closed when no capture backend probes usable. X11 init
  failure is an explicit `Err`, and the x11 CI lane fails when the
  feature cannot initialize.
- Historical `transfer` reports predate receiver-verified timing; the
  verified lane is renamed `transfer-receiver-ack-v1` on
  `fix/verified-transfer-benchmark` — update this row when it merges
  (old `transfer` reports stay incomparable, per benchmark doc).
