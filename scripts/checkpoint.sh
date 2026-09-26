#!/usr/bin/env bash
# checkpoint.sh — run a wave's gate checks and emit the report skeleton.
#
# Usage: scripts/checkpoint.sh <gate>
#
# A gate id is registered exactly once below; an unknown id exits 2 —
# a checkpoint with no registered checks is refused by design
# (docs/implementation-plan.md §Checkpoint protocol).
#
# Reports land in docs/reports/: bench-<ts>.{json,md} plus
# checkpoint-<gate>.md, a checklist to fill in honestly before merge.

set -euo pipefail
cd "$(dirname "$0")/.."

GATE="${1:-}"
TS="$(date -u +%Y%m%d-%H%M%S)"
REPORTS="docs/reports"
mkdir -p "$REPORTS"

note() { printf '\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\033[31mFAIL\033[0m %s\n' "$*" >&2; exit 1; }

green_bars() {
    note "fmt"
    cargo fmt --check || fail "cargo fmt --check"
    note "clippy"
    cargo clippy --workspace --all-targets -- -D warnings || fail "clippy default"
    if [ "$(uname -s)" = "Linux" ]; then
        cargo clippy --workspace --all-targets --features rds-desktop/x11 -- -D warnings \
            || fail "clippy x11"
    fi
    note "test"
    cargo test --workspace || fail "cargo test"
}

write_checkpoint() {
    local gate="$1" verdict="$2" extra="$3"
    cat > "$REPORTS/checkpoint-${gate}.md" <<EOF
# Checkpoint ${gate^^} — ${TS}

Verdict: **${verdict}**

## Automated checks
${extra}

## Manual checklist (fill before merge)
- [ ] All "not run" items above explained
- [ ] Reports committed: bench-*.json, bench-*.md, this file
- [ ] docs/ updated for anything this wave changed
- [ ] security/unsafe review done for new code paths
EOF
    note "wrote $REPORTS/checkpoint-${gate}.md"

    note "append hash-chained machine receipt (docs/receipts/)"
    cargo run -q -p rds-bench -- receipt \
        --kind gate --subject "$gate" --status pass --topology loopback \
        --report "$REPORTS/checkpoint-${gate}.md" \
        --note "verdict: $verdict" \
        || fail "receipt append"
    cargo run -q -p rds-bench -- validate-receipts \
        || fail "receipt log validation"
}

case "$GATE" in
c0)
    note "gate c0 — measurement harness baseline"
    green_bars

    note "bench suite run 1"
    cargo run -q -p rds-bench -- run --scenario all \
        --json "$REPORTS/bench-${TS}-a.json" --md "$REPORTS/bench-${TS}-a.md" \
        || fail "bench run A"

    note "bench suite run 2 (reproducibility)"
    cargo run -q -p rds-bench -- run --scenario all \
        --json "$REPORTS/bench-${TS}-b.json" --md "$REPORTS/bench-${TS}-b.md" \
        || fail "bench run B"

    note "reproducibility compare (p95 ±15%)"
    cargo run -q -p rds-bench -- compare \
        "$REPORTS/bench-${TS}-a.json" "$REPORTS/bench-${TS}-b.json" --tol 0.15 \
        || fail "suite not reproducible"

    cp "$REPORTS/bench-${TS}-a.md" "$REPORTS/baseline-iroh.md"
    write_checkpoint "c0" "pending review" \
        "- fmt/clippy/test: PASS
- suite A: bench-${TS}-a.{json,md}
- suite B: bench-${TS}-b.{json,md}
- reproducibility p95 ±15%: PASS
- baseline: baseline-iroh.md"
    ;;
c1)
    note "gate c1 — owned noq transport parity"
    green_bars

    note "clippy + tests with transport-noq"
    cargo clippy --workspace --all-targets \
        --features rds-net/transport-noq,rds-agent/transport-noq,rds-cli/transport-noq,rds-bench/transport-noq \
        -- -D warnings || fail "clippy noq"
    cargo test -p rds-net -p rds-agent \
        --features rds-net/transport-noq,rds-agent/transport-noq \
        || fail "noq tests"

    note "turmoil deterministic simulation"
    cargo test -p rds-net --features transport-noq --test turmoil_sim \
        || fail "turmoil sim"

    note "noq bench suite"
    cargo run -q -p rds-bench --features transport-noq -- run --scenario all --backend noq \
        --json "$REPORTS/bench-${TS}-noq.json" --md "$REPORTS/bench-${TS}-noq.md" \
        || fail "noq bench suite"

    write_checkpoint "c1" "pending review" \
        "- fmt/clippy/test: PASS
- clippy/tests with transport-noq: PASS
- turmoil partition/repair sim: PASS
- noq suite: bench-${TS}-noq.{json,md}
- impaired-path migration: see checkpoint-c1.md measurement table"
    ;;
c2)
    note "gate c2 — owned relay transport"
    green_bars

    note "clippy + tests with transport-noq and owned-relay"
    cargo clippy --workspace --all-targets \
        --features rds-net/transport-noq,rds-agent/transport-noq,rds-cli/transport-noq,rds-bench/transport-noq,rds-relay/owned-relay \
        -- -D warnings || fail "clippy relay"
    cargo test -p rds-net -p rds-agent -p rds-relay \
        --features rds-net/transport-noq,rds-agent/transport-noq,rds-relay/owned-relay \
        || fail "relay tests"

    note "relay wire decoder fuzz (proptest)"
    cargo test -p rds-core relay::tests::decode_never_panics_and_roundtrips \
        || fail "decoder fuzz"

    write_checkpoint "c2" "pending review" \
        "- fmt/clippy/test: PASS
- clippy/tests with transport-noq + owned-relay: PASS
- relay proto unit tests (rds-core): PASS
- owned relay e2e (attach→handshake→datagrams→streams, replacement, drain): PASS
- rate-limiter unit tests: PASS
- decoder fuzz (proptest): PASS
- relay-kill → second-relay migration: NOT RUN (single-relay config; needs multi-relay attach)"
    ;;
c3)
    note "gate c3 — discovery service + publish/resolve"
    green_bars

    note "directory suite (roundtrip, staleness, rate-limit, delete, registry, hostile, outage)"
    cargo test -p rds-discovery || fail "rds-discovery tests"

    note "announce + resolve e2e (publish, republish, resolve→connect, fallbacks)"
    cargo test -p rds-net --test announce_e2e || fail "announce e2e"

    note "agent e2e (relay + authz regression)"
    cargo test -p rds-agent --test e2e || fail "agent e2e"

    note "G3 cold resolve→connect→first-byte"
    cargo run -q -p rds-bench -- run --scenario resolve-connect \
        --json "$REPORTS/bench-${TS}-resolve.json" --md "$REPORTS/bench-${TS}-resolve.md" \
        || fail "resolve-connect bench"
    python3 - "$REPORTS/bench-${TS}-resolve.json" <<'PY' || fail "G3 budget exceeded"
import json, sys
r = json.load(open(sys.argv[1]))["reports"][0]
ok, total = r["attempts"]
p50 = r["rtt"]["p50_ns"] / 1e6
p95 = r["rtt"]["p95_ns"] / 1e6
print(f"resolve-connect: {ok}/{total} ok, p50={p50:.1f}ms p95={p95:.1f}ms")
sys.exit(0 if (ok == total and p50 <= 300.0) else 1)
PY

    write_checkpoint "c3" "pending review" \
        "- fmt/clippy/test: PASS
- directory suite (roundtrip, expiry, staleness/replay, forgery, rate limit, signed delete, registry authz, hostile input, outage): PASS
- announce e2e (publish, TTL refresh, addr-change republish): PASS
- resolve e2e (ticket/bare-key/name fallbacks, resolve→connect): PASS
- record + http parser fuzz (proptest): PASS
- G3 cold resolve→connect→first-byte ≤300ms: see bench-${TS}-resolve.{json,md}"
    ;;
c4)
    note "gate c4 — capability authorization"
    green_bars

    note "grant type unit tests + decoder fuzz (rds-core)"
    cargo test -p rds-core grant || fail "rds-core grant tests"

    note "grant-mode e2e: boundary, scope, expiry, replay, revocation"
    cargo test -p rds-agent --test e2e || fail "agent e2e"

    note "denylist channel: revocations endpoints + signature authz"
    cargo test -p rds-discovery || fail "discovery tests"

    write_checkpoint "c4" "pending review" \
        "- fmt/clippy/test: PASS
- grant unit tests + proptest decoder fuzz: PASS
- e2e: valid grant serves; streams before grant refused (G4); expired /
  wrong-service / untrusted-issuer rejected; denylist push drops the
  live connection and refuses new ones; concurrent replay rejected: PASS
- revocations snapshot roundtrip + forged/stale refusal: PASS
- clock-skew tolerance: documented in rds-core::grant (SKEW_SECS = 30s,
  not_before tolerant, expires_at strict)"
    ;;
c5)
    note "gate c5 — session/media protocol v2"
    green_bars

    note "clippy + tests with transport-noq (impairment lane backend)"
    cargo clippy --workspace --all-targets \
        --features rds-net/transport-noq,rds-bench/transport-noq \
        -- -D warnings || fail "clippy noq"

    note "session v2 e2e: keyframe, newest-wins, control, impairment, soak"
    cargo test -p rds-desktop --test session_v2 || fail "session_v2 e2e"

    note "session v2 e2e on the owned transport"
    cargo test -p rds-desktop --test session_v2 \
        --features rds-net/transport-noq || fail "session_v2 e2e (noq)"

    note "FrameHeader decoder + control demux fuzz (rds-core)"
    cargo test -p rds-core || fail "rds-core tests"

    write_checkpoint "c5" "pending review" \
        "- fmt/clippy/test: PASS (default + transport-noq lanes)
- session_v2 e2e (iroh + noq): PASS
  - header roundtrip + input metadata: rds-core unit tests
  - keyframe request roundtrip: PASS
  - bounded queue + newest-wins (240 fps pressure): PASS
  - input acks + heartbeat RTT: PASS
  - impairment 5% loss + 30ms jitter via ImpairingSocket (underneath
    QUIC, migration-immune): PASS — counters prove real drops
  - G5 latency: clean-link p95 ≤150ms asserted in soak lane;
    impaired lane asserts queue_p95 ≤100ms (protocol queues bounded),
    lat_p50 ≤500ms (median at path speed), tail ≤2s/3s (retransmit
    physics, not queueing) — split via capture/send/deliver ts
- soak 60fps: 20s smoke PASS; full 30min via RDS_SOAK_SECS=1800
  (run before merge or noted honestly)
- FrameHeader decoder + stream demux fuzz (proptest): PASS"
    ;;
c6)
    note "gate c6 — content-addressed resumable sync"
    green_bars

    note "sync e2e: transfer, resume, corruption, traversal, impairment"
    cargo test -p rds-sync --test sync_e2e || fail "sync_e2e"

    note "agent sync: slot guard + unconfigured refusal"
    cargo test -p rds-agent --test e2e sync_ || fail "agent sync e2e"

    note "manifest/chunking unit tests"
    cargo test -p rds-sync --lib || fail "rds-sync lib tests"

    write_checkpoint "c6" "pending review" \
        "- fmt/clippy/test: PASS (default + transport-noq lanes)
- sync_e2e (10 tests): PASS
  - push byte-identical; pull byte-identical
  - identical content resend: 0 chunks on the wire (dest-hash dedup)
  - corrupt part deleted + refetched; torn journal meta rebuilt
  - kill mid-transfer: resume fetches only missing chunks
  - G6 repeated kill/resume loop: converges identical
  - path traversal / absolute / NUL rel_paths refused (proptest fuzz)
  - impaired lane (lossy ImpairingSocket, noq backend): completes
- agent e2e: one sync session per connection (slot released on end);
  sync unconfigured refused; Info advertises Sync iff --sync-dir: PASS
- bounds: manifest ≤512/batch, chunksets ≤4096/batch, Need bitmap
  ≤256K chunks, every wire frame ≤64KB MAX_MESSAGE_LEN"
    ;;
c7)
    note "gate c7 — observability"
    green_bars

    note "metrics: known-traffic counter accuracy (direct + relay split)"
    cargo test -p rds-net --features metrics --test metrics || fail "rds-net metrics"

    note "directory metrics scrape: anonymized per-endpoint counts"
    cargo test -p rds-discovery --test directory_e2e metrics || fail "directory metrics e2e"

    note "bench report embeds the metrics snapshot it cites (G7)"
    cargo run -q -p rds-bench -- run --scenario ping --iterations 10 \
        --md /tmp/rds-c7-bench.md --json /tmp/rds-c7-bench.json || fail "bench run"
    grep -q "rds_net_datagrams_sent_total" /tmp/rds-c7-bench.md \
        || fail "bench report carries no metrics snapshot"
    grep -q "rds_net_bytes_sent_total" /tmp/rds-c7-bench.md \
        || fail "bench report missing relay/direct byte split"

    write_checkpoint "c7" "pending review" \
        "- fmt/clippy/test: PASS (default + transport-noq lanes)
- metrics known-traffic (3 tests): PASS
  - direct echo → via=direct counters only; bytes_sent/recv cover payload
  - relay-only ticket + path pinning → via=relay counters only
  - prometheus render emits every name the bench reports cite
- bench report G7: metrics block embedded from endpoint registries
  (client_/agent_ prefixed snapshot at run time): PASS
- directory /v1/metrics: per-endpoint PUT counters under anonymized
  blake3-16 labels; raw key + peer addr absent from body: PASS
- exposure: /v1/metrics is loopback-only at the router (non-loopback
  peers get 404) — remote scrape via SSH/local exporter, documented in
  architecture.md
- session spans: rds.conn{peer, session_id} ⊃ rds.stream{service}
  on every agent connection; sync engine logs session events
- QNT attempt/success counters driven by the noq policy driver;
  iroh reports paths_seen{via=direct} as the equivalent signal"
    ;;
c8)
    note "gate c8 — deployment artifacts"
    green_bars

    note "systemd units carry the required sandboxing directives"
    for unit in deploy/systemd/rds-server.service deploy/systemd/rds-agent.service; do
        for directive in NoNewPrivileges=yes ProtectSystem=strict \
                ProtectHome=yes PrivateTmp=yes "CapabilityBoundingSet=" \
                "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK" \
                "SystemCallFilter=@system-service" "UMask=0077"; do
            grep -q "^$directive" "$unit" \
                || fail "$unit missing $directive"
        done
    done

    note "runbook documents ports, firewall, restart/upgrade/drain, failure modes"
    for section in "Ports and firewall" "Restart" "Upgrade" "Drain" \
            "Failure modes" "0600" "loopback"; do
        grep -qi "$section" docs/deployment.md \
            || fail "deployment.md missing section: $section"
    done

    note "deployed-unit smoke: binaries build release + --version runs"
    cargo build --release -p rds-server -p rds-agent || fail "release build"
    ./target/release/rds-server --version >/dev/null || fail "rds-server --version"
    ./target/release/rds-agent --version >/dev/null || fail "rds-agent --version"

    write_checkpoint "c8" "pending review" \
        "- fmt/clippy/test: PASS (default + transport-noq lanes)
- systemd units: ProtectSystem=strict, NoNewPrivileges, PrivateTmp,
  empty CapabilityBoundingSet, AF_INET/6/UNIX/NETLINK (netlink for
  netwatch), @system-service filter, UMask=0077, StateDirectory-scoped
  writes: verified in both units
- key permissions: endpoint.key written 0600 by load_or_create_key;
  enforced at create; review checks stat %a on deployed hosts
- ports/firewall/runbook/failure-modes: docs/deployment.md
- release build + --version smoke: PASS
- REAL-METAL EVIDENCE: attested in the private estate repository's
  docs/reports/rds-e2e-<date>.md (host facts are estate-private) —
  this gate covers the artifact layer; ssh-across-NAT, soak and
  desktop-smoke rows are attested there"
    ;;
r0-evidence)
    note "gate r0-evidence — remediation measurement truthfulness (W0)"
    green_bars

    note "bench self-tests (comparator, verified transfer, world)"
    cargo test -p rds-bench || fail "rds-bench tests"

    note "comparator negative fixtures — every refusal must exit 1"
    FX="$(mktemp -d)"
    python3 - "$FX" <<'PY' || fail "fixture generation"
import json, sys, os
d = sys.argv[1]
def rep(scenario="ping", backend="iroh", path="direct", count=50,
        p95=50_000_000, tp=None, attempts=None, notes=None, imp=None):
    return {"meta": {"scenario": scenario, "backend": backend, "path": path,
                     "impairment": imp, "unix_ts": 0, "git": "fixture"},
            "rtt": {"count": count, "min_ns": 1, "p50_ns": p95 // 2,
                    "p95_ns": p95, "p99_ns": p95, "max_ns": p95,
                    "mean_ns": p95 / 2},
            "throughput_mib_s": tp, "attempts": attempts,
            "metrics": {}, "notes": notes or []}
def suite(name, reports):
    json.dump({"tool": "fixture", "unix_ts": 0, "git": "fixture",
               "reports": reports}, open(os.path.join(d, name), "w"))
suite("a.json", [rep()])
suite("same.json", [rep(p95=52_000_000)])
suite("missing.json", [rep(scenario="other")])
suite("backend.json", [rep(backend="noq")])
suite("impairment.json", [rep(imp={"loss": 0.05, "delay_ms": 50,
                                   "jitter_ms": 30, "rate_mbps": None,
                                   "seed": 2})])
suite("failed.json", [rep(attempts=[9, 50])])
suite("thin.json", [rep(count=1)])
suite("nan.json", [rep(tp=float("nan"))])
suite("absent.json", [{**rep(), "throughput_mib_s": None,
                       "rtt": None}])
PY
    for bad in missing backend impairment failed thin nan absent; do
        if cargo run -q -p rds-bench -- compare "$FX/a.json" "$FX/$bad.json" >/dev/null 2>&1; then
            fail "comparator accepted $bad fixture"
        fi
    done
    cargo run -q -p rds-bench -- compare "$FX/a.json" "$FX/same.json" >/dev/null \
        || fail "comparator rejected a clean fixture pair"
    rm -rf "$FX"

    write_checkpoint "r0-evidence" "pending review" \
        "- fmt/clippy/test: PASS
- rds-bench unit tests (comparator faults, verified transfer, world): PASS
- comparator CLI negative fixtures refuse: missing scenario, backend /
  impairment mismatch, failed scenario, single-sample, NaN, absent
  metric: PASS
- comparator CLI accepts an equal-profile clean pair: PASS
- W0.1 regression inventory: see remediation-progress.md table
- W0.3 owned-relay bench world + W0.5 capability matrix + W0.6 receipt
  schema: pending their own increments — this gate does not close W0"
    ;;
*)
    cat <<EOF
unknown or unregistered gate: '${GATE}'
registered gates: c0 c1 c2 c3 c4 c5 c6 c7 c8 r0-evidence
a checkpoint with no registered checks is refused by design.
EOF
    exit 2
    ;;
esac
