# Checkpoint C0 — 20260926-161247

Verdict: **PASS** — the iroh suite is the first run where every row is
provably on its advertised path (W0.3 measured-path containment).

## Automated checks
- fmt/clippy/test: PASS
- suite A: bench-20260926-161247-a.{json,md}
- suite B: bench-20260926-161247-b.{json,md}
- reproducibility p95 ±15%: PASS
- baseline: baseline-iroh.md

## What changed since the last baseline

Every bench world now bounds path kinds (`EndpointConfig::transports`)
and pins single-path endpoints (`max_multipath_paths=1` + iroh
`PathSelector` over `unstable-custom-transports`). Impairment rows are
enforced, not trusted: offered ≥ client-sent datagrams on the claimed
kind, plus a delay-configured RTT floor.

| scenario | path | evidence |
| --- | --- | --- |
| handshake | direct | 100/100, p50 ≈ 25 ms |
| ping | direct | p50 ≈ 2 ms, via=direct only |
| transfer | direct | ≈ 11 MiB/s (debug), verified receipt |
| multiconnect | direct-impaired | 100/100, p50 ≈ 159 ms — proxied |
| relay-fallback | relay | via=relay counters only, zero direct |
| impaired | direct-impaired | p50 ≈ 140 ms; offered 947 ≥ sent 612 |
| impaired | relay-impaired | **explicit skip** — iroh relay leg is TCP |
| resolve-connect | discovered | 100/100 via directory + relay ticket |

## Manual checklist
- [x] All "not run" items above explained — relay-impaired is an
  explicit `SCENARIO SKIPPED` row (UDP impairment cannot sit below
  iroh's TCP relay leg), not an omission.
- [x] Reports committed: bench-*.json, bench-*.md, this file
- [x] docs/ updated — docs/remediation-progress.md W0.3 entry
- [x] security/unsafe review — no `unsafe`; `PathSelector` feature is
  documented as an intentional unstable-API pin in lib.rs/iroh.rs
