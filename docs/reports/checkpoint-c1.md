# Checkpoint C1 — 20260926-162954

Verdict: **PASS** — the owned transport finally holds its declared paths,
including impairment below every selected path (the escape recorded in
the previous c1 is closed).

## Automated checks
- fmt/clippy/test: PASS
- clippy/tests with transport-noq: PASS
- turmoil partition/repair sim: PASS (4/4)
- noq suite: bench-20260926-162954-noq.{json,md}
- impaired-path migration: closed — see measurement table

## Gate-script fix this run required

The c1 bench line ran `cargo run -p rds-bench` without
`--features rds-bench/transport-noq`, so every noq scenario reported the
backend unavailable. Added the feature flag (same fix the clippy line
already had); a stray wrong-build report from the first attempt was
discarded, not committed.

## Impairment evidence (the wave's reason)

Scenario `impaired` on noq, 5% loss + 50 ms delay + 30 ms jitter:

| lane | p50 | integrity |
| --- | --- | --- |
| direct-impaired | 136.6 ms | socket-level impairment — migration-immune; offered 618 ≥ sent 332 |
| relay-impaired | 293.0 ms | UDP proxies on both attachment legs; offered 2347 ≥ sent 337 |

Compare the previous c1: `noq after driver | 1.80 ms | migrates — proxy
carried only the handshake`. That escape is now impossible for two
independent reasons — the `RelaySocket` helper endpoint pins to its
bootstrap path (`max_multipath_paths=1` suppresses its candidate
exchange), and the enforce step would reject any residual bypass.

`resolve-connect` on noq: 100/100 through the owned `rds-relay` +
directory (cold resolve→connect p50 ≈ 103 ms, mostly direct paths on
loopback — honest, the ticket allows them).

## Manual checklist
- [x] All "not run" items above explained — none omitted; noq covers
  relay-impaired natively.
- [x] Reports committed: bench-*.json, bench-*.md, this file
- [x] docs/ updated — docs/remediation-progress.md W0.3 entry
- [x] security/unsafe review — no `unsafe`; relay-only endpoints never
  advertise or dial direct candidates
