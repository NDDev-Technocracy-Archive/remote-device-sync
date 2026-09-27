//! Scenario runners — each produces one `BenchReport`.
//!
//! The case set follows the QUIC interop runner taxonomy
//! (`handshake`, `transfer`, `multiconnect`) plus rds-specific cases
//! (`ping`, `relay-fallback`, impaired-path runs). `rebind-*` and
//! `migration` need socket-level control and arrive with the noq
//! backend (WS1).

use std::time::{Duration, Instant};

use anyhow::Context;

use crate::impair::Impairment;
use crate::report::{BenchMeta, BenchReport, Percentiles, git_sha, unix_ts};
use crate::world::{Path, World, WorldRelay};

/// Knobs every scenario reads; CLI fills it.
#[derive(Debug, Clone)]
pub struct Params {
    /// Probes per latency scenario / connections per handshake scenario.
    pub iterations: usize,
    /// Payload size for `transfer`, MiB.
    pub transfer_mib: u64,
    /// Per-attempt timeout; an attempt exceeding it counts as failed.
    pub timeout: Duration,
    /// Impairment applied to `*-impaired` scenarios.
    pub impairment: Impairment,
    /// Backend under test: `iroh` (default) or `noq` (with the
    /// `transport-noq` feature).
    pub backend: String,
}

impl Params {
    /// Parse the backend label into the facade selector.
    pub fn transport_backend(&self) -> anyhow::Result<rds_net::Backend> {
        match self.backend.as_str() {
            "iroh" => Ok(rds_net::Backend::Iroh),
            #[cfg(feature = "transport-noq")]
            "noq" => Ok(rds_net::Backend::Noq),
            other => anyhow::bail!(
                "unknown or unavailable backend {other:?} (build with --features transport-noq for `noq`)"
            ),
        }
    }
}

impl Default for Params {
    fn default() -> Self {
        Self {
            iterations: 100,
            transfer_mib: 32,
            timeout: Duration::from_secs(15),
            impairment: Impairment::lossy(),
            backend: "iroh".into(),
        }
    }
}

/// Every case `run` knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scenario {
    /// Cold connect → close, repeated. Measures resolve→established.
    Handshake,
    /// Established connection, N ping probes, RTT percentiles.
    Ping,
    /// Bulk stream goodput through receiver byte/digest acknowledgement.
    Transfer,
    /// Handshake under 5% loss (interop `multiconnect` analogue).
    Multiconnect,
    /// Connect with only the relay address advertised.
    RelayFallback,
    /// Ping over the impaired direct path (loss/jitter/rate as given).
    Impaired,
    /// Cold `rds ssh <name>`: registry name → record → connect →
    /// first byte, fresh client endpoint per iteration (G3).
    ResolveConnect,
    /// Mid-connection relay failover on a two-attachment world: drain
    /// and hard-kill lanes, recovery latency onto the surviving slot.
    /// noq-only.
    Migration,
    /// Verified transfer over a rate-capped impaired path; measured
    /// goodput must land inside the declared band of the cap, proving
    /// the measurement reports the imposed ceiling rather than a
    /// fabricated or unbounded figure (W0.2).
    Calibration,
    /// Path-loss mid-transfer: verified upload starts clean, then 30%
    /// loss + delay are imposed on the client's live socket; the receipt must
    /// still verify and the drop counter must prove loss engaged
    /// (W0.3). noq only — needs runtime-tunable socket impairment.
    Recovery,
    /// All of the above.
    All,
}

/// Lanes `Scenario::All` expands to, in run order. `All` itself is a
/// CLI convenience and is never a report name.
pub const LANES: &[Scenario] = &[
    Scenario::Handshake,
    Scenario::Ping,
    Scenario::Transfer,
    Scenario::Multiconnect,
    Scenario::RelayFallback,
    Scenario::Impaired,
    Scenario::ResolveConnect,
    Scenario::Migration,
    Scenario::Calibration,
    Scenario::Recovery,
];

impl Scenario {
    /// Scenario name emitted into report metadata.
    pub fn name(&self) -> &'static str {
        match self {
            Scenario::Handshake => "handshake",
            Scenario::Ping => "ping",
            Scenario::Transfer => "transfer-receiver-ack-v1",
            Scenario::Multiconnect => "multiconnect",
            Scenario::RelayFallback => "relay-fallback",
            Scenario::Impaired => "impaired",
            Scenario::ResolveConnect => "resolve-connect",
            Scenario::Migration => "migration",
            Scenario::Calibration => "calibration",
            Scenario::Recovery => "recovery",
            Scenario::All => "all",
        }
    }
}

/// Run one scenario (or the whole suite for `All`).
pub async fn run(s: Scenario, p: &Params) -> anyhow::Result<Vec<BenchReport>> {
    if s == Scenario::All {
        let mut out = Vec::new();
        for &s in LANES {
            match run_one(s, p).await {
                Ok(reports) => out.extend(reports),
                Err(e) => out.push(failed(s.name(), p, e)),
            }
        }
        // `failed` rows never passed through run_one's tagger.
        return Ok(tag_reports(out));
    }
    run_one(s, p).await
}

async fn run_one(s: Scenario, p: &Params) -> anyhow::Result<Vec<BenchReport>> {
    // Fail fast on an unselectable backend instead of per-scenario noise.
    p.transport_backend()?;
    Ok(tag_reports(run_lane(s, p).await?))
}

async fn run_lane(s: Scenario, p: &Params) -> anyhow::Result<Vec<BenchReport>> {
    match s {
        Scenario::Handshake => handshake(p, Path::Direct).await.map(|r| vec![r]),
        Scenario::Ping => ping(p, Path::Direct, None).await.map(|r| vec![r]),
        Scenario::Transfer => transfer(p).await.map(|r| vec![r]),
        Scenario::Multiconnect => {
            handshake(p, Path::DirectImpaired(p.impairment))
                .await
                .map(|mut r| {
                    r.meta.scenario = "multiconnect".into();
                    vec![r]
                })
        }
        Scenario::RelayFallback => ping(p, Path::RelayOnly, None).await.map(|mut r| {
            r.meta.scenario = "relay-fallback".into();
            vec![r]
        }),
        Scenario::Impaired => {
            let mut reports = Vec::new();
            let mut direct =
                ping(p, Path::DirectImpaired(p.impairment), Some(p.impairment)).await?;
            direct.meta.scenario = "impaired".into();
            reports.push(direct);
            // noq worlds also impair the endpoint↔relay attachment legs
            // and measure the relay path under impairment; iroh's TCP
            // relay leg cannot take a UDP impairment device — an explicit
            // SCENARIO SKIPPED row, not silent absence.
            #[cfg(feature = "transport-noq")]
            if p.transport_backend()? == rds_net::Backend::Noq {
                match ping(p, Path::RelayImpaired(p.impairment), Some(p.impairment)).await {
                    Ok(mut r) => {
                        r.meta.scenario = "impaired".into();
                        reports.push(r);
                    }
                    Err(e) => {
                        let mut r = failed("impaired", p, e);
                        r.meta.path = "relay-impaired".into();
                        reports.push(r);
                    }
                }
            } else {
                reports.push(skipped(
                    "impaired",
                    p,
                    "relay-impaired",
                    "iroh relay attachment is TCP; UDP impairment cannot sit below it",
                ));
            }
            #[cfg(not(feature = "transport-noq"))]
            reports.push(skipped(
                "impaired",
                p,
                "relay-impaired",
                "built without transport-noq; the owned relay backend is unavailable",
            ));
            Ok(reports)
        }
        Scenario::ResolveConnect => resolve_connect(p).await.map(|r| vec![r]),
        Scenario::Migration => migration(p).await,
        Scenario::Calibration => calibration(p).await.map(|r| vec![r]),
        Scenario::Recovery => recovery(p).await.map(|r| vec![r]),
        Scenario::All => unreachable!("handled in run"),
    }
}

/// The capability tag tracks the emitted scenario name — composite
/// lanes relabel `meta.scenario` (multiconnect runs the handshake body,
/// relay-fallback/impaired run ping), so the tag is normalized here at
/// the single point every report exits through.
fn tag_reports(mut reports: Vec<BenchReport>) -> Vec<BenchReport> {
    for r in &mut reports {
        if r.meta.capability.is_empty() {
            r.meta.capability = format!("measure:{}", r.meta.scenario);
        }
    }
    reports
}

fn meta(scenario: &str, p: &Params, path: &str, impairment: Option<Impairment>) -> BenchMeta {
    BenchMeta {
        capability: String::new(),
        scenario: scenario.into(),
        backend: p.backend.clone(),
        path: path.into(),
        impairment,
        unix_ts: unix_ts(),
        git: git_sha(),
    }
}

fn proxy_note(world: &World) -> Vec<String> {
    let t = world.impair_totals();
    if t.probes == 0 {
        return Vec::new();
    }
    vec![format!(
        "impairment probes={} forwarded={} dropped={} bytes={}",
        t.probes, t.forwarded, t.dropped, t.bytes
    )]
}

fn failed(scenario: &str, p: &Params, e: anyhow::Error) -> BenchReport {
    BenchReport {
        meta: meta(scenario, p, "n/a", None),
        rtt: None,
        throughput_mib_s: None,
        attempts: None,
        metrics: Default::default(),
        notes: vec![format!("SCENARIO FAILED: {e:#}")],
    }
}

/// A lane the selected backend/build cannot run — declared in the
/// suite (path named, reason given), never silently absent and never
/// counted as a failure.
fn skipped(scenario: &str, p: &Params, path: &str, reason: &str) -> BenchReport {
    BenchReport {
        meta: meta(scenario, p, path, Some(p.impairment)),
        rtt: None,
        throughput_mib_s: None,
        attempts: None,
        metrics: Default::default(),
        notes: vec![format!("SCENARIO SKIPPED: {reason}")],
    }
}

/// Cold connect → immediate close, repeated `iterations` times.
async fn handshake(p: &Params, path: Path) -> anyhow::Result<BenchReport> {
    let world = World::spawn(path, p.transport_backend()?)
        .await
        .context("spawn world")?;
    let mut samples = Vec::with_capacity(p.iterations);
    let mut ok = 0u64;
    for _ in 0..p.iterations {
        let t0 = Instant::now();
        if let Ok(Ok(conn)) = tokio::time::timeout(
            p.timeout,
            rds_cli::connect(&world.client, world.target.clone()),
        )
        .await
        {
            samples.push(t0.elapsed().as_nanos() as u64);
            ok += 1;
            // Fold this connection's handshake counters before closing —
            // after close the path set is gone.
            world.client.metrics().sampler(conn.clone()).sample();
            conn.close(0u32.into(), b"bench done");
        }
    }
    let mut notes = proxy_note(&world);
    if ok < p.iterations as u64 {
        notes.push(format!(
            "{} connects timed out or failed",
            p.iterations as u64 - ok
        ));
    }
    let metrics = world.metrics_snapshot(None);
    // Close before enforcing so a failed check cannot abort the world
    // into ungraceful endpoint drops.
    world.close().await;
    world
        .enforce_path_integrity(&metrics, 1)
        .context("path integrity")?;
    Ok(BenchReport {
        meta: meta("handshake", p, world.path.label(), path.impairment()),
        rtt: Percentiles::of(&samples),
        throughput_mib_s: None,
        attempts: Some((ok, p.iterations as u64)),
        metrics,
        notes,
    })
}

/// One connection, `iterations` ping probes.
async fn ping(
    p: &Params,
    path: Path,
    impairment: Option<Impairment>,
) -> anyhow::Result<BenchReport> {
    let world = World::spawn(path, p.transport_backend()?)
        .await
        .context("spawn world")?;
    let t_connect = Instant::now();
    let conn = tokio::time::timeout(
        p.timeout,
        rds_cli::connect(&world.client, world.target.clone()),
    )
    .await
    .context("connect timed out")?
    .context("connect failed")?;
    let connect_elapsed = t_connect.elapsed();
    // Warmup probes are excluded: the first streams pay one-time setup
    // that would otherwise pollute the tail.
    for i in 0..5 {
        rds_cli::ping(&conn, i as u64).await?;
    }
    let mut samples = Vec::with_capacity(p.iterations);
    for i in 0..p.iterations {
        let rtt = rds_cli::ping(&conn, 1000 + i as u64).await?;
        samples.push(rtt.as_nanos() as u64);
    }
    let mut metrics = world.metrics_snapshot(Some(&conn));
    metrics.insert("phase_connect_ns".into(), connect_elapsed.as_nanos() as u64);
    world.close().await;
    world
        .enforce_path_integrity(&metrics, p.iterations as u64 * 8)
        .context("path integrity")?;
    Ok(BenchReport {
        meta: meta("ping", p, world.path.label(), impairment),
        rtt: Percentiles::of(&samples),
        throughput_mib_s: None,
        attempts: None,
        metrics,
        notes: proxy_note(&world),
    })
}

/// `transfer_mib` MiB over one forwarded stream, ending at a verified receipt.
async fn transfer(p: &Params) -> anyhow::Result<BenchReport> {
    let (report, _measurement) = transfer_on(p, Path::Direct).await?;
    Ok(report)
}

/// Shared transfer body: `path` selects the ticket shape so calibration
/// lanes can impose a known impairment.
async fn transfer_on(
    p: &Params,
    path: Path,
) -> anyhow::Result<(BenchReport, crate::transfer::Measurement)> {
    let total = p
        .transfer_mib
        .checked_mul(1024 * 1024)
        .context("transfer size overflow")?;
    anyhow::ensure!(total > 0, "transfer size must be positive");
    anyhow::ensure!(!p.timeout.is_zero(), "transfer timeout must be positive");
    let world = tokio::time::timeout(p.timeout, World::spawn(path, p.transport_backend()?))
        .await
        .context("world startup timed out")?
        .context("spawn world")?;
    // A single deadline covers connect, OpenTcp, upload, receipt and EOF.
    let outcome = tokio::time::timeout(p.timeout, async {
        let t_connect = Instant::now();
        let conn = rds_cli::connect(&world.client, world.target.clone())
            .await
            .context("connect failed")?;
        let connect_elapsed = t_connect.elapsed();
        let (host, port) = world.transfer_target();
        let t_open = Instant::now();
        let (mut send, mut recv) = rds_cli::open_tcp(&conn, &host, port).await?;
        let open_elapsed = t_open.elapsed();
        let measurement = crate::transfer::send_verified(&mut send, &mut recv, total).await?;
        anyhow::Ok((
            measurement,
            connect_elapsed,
            open_elapsed,
            world.metrics_snapshot(Some(&conn)),
        ))
    })
    .await
    .context("transfer operation timed out")
    .and_then(|result| result);
    let cleanup = tokio::time::timeout(Duration::from_secs(5), world.close()).await;
    let (measurement, connect_elapsed, open_elapsed, mut metrics) = outcome?;
    cleanup.context("world shutdown timed out")?;
    // Phase split (W0.2): connect, service open/authorize, payload
    // completion — measured separately so regression attribution does
    // not need re-derivation from a single aggregate.
    metrics.insert("phase_connect_ns".into(), connect_elapsed.as_nanos() as u64);
    metrics.insert(
        "phase_service_open_ns".into(),
        open_elapsed.as_nanos() as u64,
    );
    let mib_s = measurement.bytes as f64 / (1024.0 * 1024.0) / measurement.elapsed.as_secs_f64();
    metrics.insert("transfer_verified_bytes".into(), measurement.bytes);
    metrics.insert(
        "transfer_completion_ns".into(),
        measurement
            .elapsed
            .as_nanos()
            .try_into()
            .unwrap_or(u64::MAX),
    );
    let mut notes = proxy_note(&world);
    notes.push("receiver-ack-v1: byte count + BLAKE3 digest + EOF; payload generation/hash, upload and receipt are timed; connect/OpenTcp are reported separately as phase_* metrics, not folded into throughput; not comparable to historical sender-finish results".into());
    // The world is already closed above: a failed integrity check cannot
    // leak endpoints, and the snapshot was captured mid-connection.
    world
        .enforce_path_integrity(&metrics, total)
        .context("path integrity")?;
    let scenario_name = match world.path {
        Path::Direct => Scenario::Transfer.name(),
        _ => Scenario::Calibration.name(),
    };
    let impairment = match world.path {
        Path::DirectImpaired(i) => Some(i),
        _ => None,
    };
    Ok((
        BenchReport {
            meta: meta(scenario_name, p, world.path.label(), impairment),
            rtt: None,
            throughput_mib_s: Some(mib_s),
            attempts: None,
            metrics,
            notes,
        },
        measurement,
    ))
}

/// Cold `rds ssh <name>`: the estate-signed registry maps a device
/// name to the agent's key, the directory serves the agent's announced
/// record, and each iteration resolves → connects → reads the first
/// byte with a *fresh* client endpoint — no warm session resumption.
/// This is the G3 measurement (≤300 ms on LAN).
async fn resolve_connect(p: &Params) -> anyhow::Result<BenchReport> {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use rds_agent::{Agent, AgentPolicy};
    use rds_discovery::registry::SignedRegistry;
    use rds_discovery::{EndpointKey, MemoryStore, client, service};
    use rds_net::{AnnounceConfig, EndpointConfig, bind_endpoint};

    let backend = p.transport_backend()?;

    // Relay per backend: iroh runs its in-process relay, noq attaches
    // endpoints to the owned `rds-relay` — same world shape either way.
    let mut iroh_relay_url: Option<String> = None;
    #[cfg(feature = "transport-noq")]
    let mut owned_relay_addr: Option<rds_net::EndpointAddr> = None;
    let _relay_keepalive: Vec<WorldRelay> = match backend {
        rds_net::Backend::Iroh => {
            let mut relay_config = iroh_relay::server::ServerConfig::default();
            relay_config.relay = Some(iroh_relay::server::RelayConfig::new(
                "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
            ));
            let relay = iroh_relay::server::Server::spawn(relay_config).await?;
            iroh_relay_url = Some(format!("http://{}", relay.http_addr().unwrap()));
            vec![WorldRelay::Iroh(relay)]
        }
        #[cfg(feature = "transport-noq")]
        rds_net::Backend::Noq => {
            let server = rds_relay::server::serve(
                EndpointConfig {
                    backend,
                    bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
                    discovery: false,
                    ..Default::default()
                },
                Vec::new(),
            )
            .await
            .context("spawn owned relay")?;
            owned_relay_addr = Some(server.endpoint_addr());
            vec![WorldRelay::Owned(server)]
        }
        #[allow(unreachable_patterns)]
        _ => Vec::new(),
    };

    // Per-backend endpoint config: iroh attaches by relay URL, noq by
    // owned-relay endpoint address.
    let endpoint_config = |key: rds_net::SecretKey| -> anyhow::Result<EndpointConfig> {
        let mut config = EndpointConfig {
            secret_key: Some(key),
            backend,
            ..Default::default()
        };
        if let Some(url) = &iroh_relay_url {
            config = config.with_relay(url)?;
        }
        #[cfg(feature = "transport-noq")]
        {
            config.relay_endpoints = owned_relay_addr.clone().into_iter().collect();
        }
        Ok(config)
    };

    let agent_key = rds_net::SecretKey::from_bytes(&[42u8; 32]);
    let client_key = rds_net::SecretKey::from_bytes(&[77u8; 32]);

    // Estate-signed registry: "bench-agent" → agent endpoint key.
    let reg_key = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
    let snap = SignedRegistry::publish(
        &reg_key,
        1,
        1,
        BTreeMap::from([(
            "bench-agent".to_string(),
            EndpointKey(*agent_key.public().as_bytes()),
        )]),
        Duration::from_secs(3600),
    )?;
    let dir = service::serve(
        "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
        Arc::new(MemoryStore::default()),
        service::ServiceConfig {
            registry_key: Some(reg_key.verifying_key()),
            registry: Some(snap),
            ..service::ServiceConfig::open_ephemeral()
        },
    )
    .await?;
    let directory = client::Client::new(dir.addr()).with_registry_key(reg_key.verifying_key());

    // Agent endpoint: announce into the directory, then serve.
    let agent_ep = bind_endpoint(endpoint_config(agent_key.clone())?).await?;
    agent_ep.online().await;
    let _announce = rds_net::announce(
        agent_ep.clone(),
        AnnounceConfig {
            issuer: rds_discovery::publisher::RecordIssuer::memory(
                ed25519_dalek::SigningKey::from_bytes(&agent_key.to_bytes()),
            ),
            directory: directory.clone(),
            services: vec![rds_discovery::Service::Ping],
            ttl: Duration::from_secs(120),
            retry: rds_net::RetryPolicy::default(),
        },
    )?;
    let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
    policy.allow.insert(client_key.public());
    let agent = Arc::new(Agent::new(agent_ep, policy));
    let agent_task = tokio::spawn({
        let agent = agent.clone();
        async move {
            let _ = agent.run().await;
        }
    });

    // The first publish is asynchronous; wait for it before timing.
    let ek = EndpointKey(*agent.id().as_bytes());
    let mut published = false;
    for _ in 0..100 {
        if directory.fetch(&ek).await.is_ok() {
            published = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::ensure!(published, "agent record never reached the directory");

    let mut samples = Vec::with_capacity(p.iterations);
    let mut resolve_ns = Vec::with_capacity(p.iterations);
    let mut connect_ns = Vec::with_capacity(p.iterations);
    let mut first_byte_ns = Vec::with_capacity(p.iterations);
    let mut ok = 0u64;
    // Client endpoints are per-iteration; accumulate their registries
    // so the report keeps total connection/path counters (G7).
    let mut client_metrics: BTreeMap<String, u64> = BTreeMap::new();
    for _ in 0..p.iterations {
        // A fresh client endpoint keeps both resolve and handshake
        // cold, matching a new `rds ssh` invocation.
        let client_ep = bind_endpoint(endpoint_config(client_key.clone())?).await?;
        let timed = tokio::time::timeout(p.timeout, async {
            let t_resolve = Instant::now();
            let addr = rds_net::resolve_target(Some(directory.clone()), "bench-agent").await?;
            let d_resolve = t_resolve.elapsed();
            let t_connect = Instant::now();
            let conn = rds_cli::connect(&client_ep, addr).await?;
            let d_connect = t_connect.elapsed();
            let t_first = Instant::now();
            rds_cli::ping(&conn, rand::random::<u64>()).await?;
            let d_first = t_first.elapsed();
            client_ep.metrics().sampler(conn.clone()).sample();
            Ok::<_, anyhow::Error>((d_resolve, d_connect, d_first, t_resolve.elapsed()))
        })
        .await;
        if let Ok(Ok((d_resolve, d_connect, d_first, d_total))) = timed {
            resolve_ns.push(d_resolve.as_nanos() as u64);
            connect_ns.push(d_connect.as_nanos() as u64);
            first_byte_ns.push(d_first.as_nanos() as u64);
            samples.push(d_total.as_nanos() as u64);
            ok += 1;
        }
        for (k, v) in client_ep.metrics().snapshot() {
            *client_metrics.entry(format!("client_{k}")).or_insert(0) += v;
        }
        client_ep.close().await;
    }
    agent_task.abort();
    agent.endpoint.close().await;
    let mut notes = Vec::new();
    if ok < p.iterations as u64 {
        notes.push(format!(
            "{} cold resolve→connect→first-byte attempts failed",
            p.iterations as u64 - ok
        ));
    }
    let mut metrics = client_metrics;
    for (k, v) in agent.endpoint.metrics().snapshot() {
        metrics.insert(format!("agent_{k}"), v);
    }
    // Phase split (W0.2): resolve, connect, first-service-byte as
    // separate percentile series so regression attribution does not
    // need re-derivation from the aggregate `rtt` field.
    for (phase, samples) in [
        ("resolve", &resolve_ns),
        ("connect", &connect_ns),
        ("first_byte", &first_byte_ns),
    ] {
        if let Some(px) = Percentiles::of(samples) {
            metrics.insert(format!("phase_{phase}_p50_ns"), px.p50_ns);
            metrics.insert(format!("phase_{phase}_p95_ns"), px.p95_ns);
        }
    }
    Ok(BenchReport {
        meta: meta("resolve-connect", p, "discovered", None),
        rtt: Percentiles::of(&samples),
        throughput_mib_s: None,
        attempts: Some((ok, p.iterations as u64)),
        metrics,
        notes,
    })
}
/// Mid-connection relay failover, measured on both failure modes:
/// `drain` (graceful notice, relay keeps forwarding through its grace
/// window) and `kill` (tunnel torn down outright). Each lane reports
/// recovery latency onto the surviving slot plus per-relay datagram
/// counters proving which attachment carried traffic.
async fn migration(p: &Params) -> anyhow::Result<Vec<BenchReport>> {
    #[cfg(feature = "transport-noq")]
    if p.transport_backend()? == rds_net::Backend::Noq {
        let mut reports = Vec::new();
        for graceful in [true, false] {
            match migration_one(p, graceful).await {
                Ok(r) => reports.push(r),
                Err(e) => {
                    let mut r = failed(Scenario::Migration.name(), p, e);
                    r.meta.path = failover_mode(graceful).into();
                    reports.push(r);
                }
            }
        }
        return Ok(reports);
    }
    Ok(vec![skipped(
        Scenario::Migration.name(),
        p,
        "relay-failover",
        "multi-relay failover requires the owned noq transport (--backend noq)",
    )])
}

#[cfg(feature = "transport-noq")]
fn failover_mode(graceful: bool) -> &'static str {
    if graceful {
        "relay-failover-drain"
    } else {
        "relay-failover-kill"
    }
}

/// One failover lane: two-attachment relay-only world, warmup probes,
/// identify the forwarding relay by its datagram-counter delta, fail
/// it, then probe until the selected path lands on the surviving slot.
/// Recovery latency is fail-start → first successful probe observed on
/// a different path id.
#[cfg(feature = "transport-noq")]
async fn migration_one(p: &Params, graceful: bool) -> anyhow::Result<BenchReport> {
    /// Upper bound for re-selection on the surviving slot; the relay
    /// drain grace is 2s, so migration must beat it by a wide margin.
    const MIGRATION_BUDGET: Duration = Duration::from_secs(10);

    let world = tokio::time::timeout(
        p.timeout,
        World::spawn(Path::RelayFailover, rds_net::Backend::Noq),
    )
    .await
    .context("world startup timed out")?
    .context("spawn world")?;
    let t_connect = Instant::now();
    let conn = tokio::time::timeout(
        p.timeout,
        rds_cli::connect(&world.client, world.target.clone()),
    )
    .await
    .context("connect timed out")?
    .context("connect failed")?;
    let connect_elapsed = t_connect.elapsed();
    for i in 0..5 {
        rds_cli::ping(&conn, i).await?;
    }

    // Fail whichever relay slot the client's egress path rides. The two
    // directions can pick different slots (server counters cannot identify
    // egress), and `selected` is suppressed while both paths stay
    // Available — so identify the path by its datagram-counter delta.
    let marker_seq = rand::random::<u64>();
    let sent0: std::collections::BTreeMap<u64, u64> = conn
        .path_stats()
        .iter()
        .map(|s| (s.path_id, s.sent))
        .collect();
    for i in 0..3 {
        rds_cli::ping(&conn, marker_seq + i).await?;
    }
    let active = conn
        .path_stats()
        .iter()
        .filter(|s| s.via_relay)
        .max_by_key(|s| s.sent - sent0.get(&s.path_id).copied().unwrap_or(0))
        .and_then(|s| s.relay_slot)
        .context("client has no relay path to fail")?;
    anyhow::ensure!(active < 2, "world attached relays beyond slot 1");
    let survivor = active ^ 1;
    let started = Instant::now();
    let probe = async {
        let mut failed = 0u64;
        let mut seq = rand::random::<u64>();
        loop {
            if started.elapsed() >= MIGRATION_BUDGET {
                anyhow::bail!("selected path never moved off the failed relay slot");
            }
            let ok = matches!(
                tokio::time::timeout(Duration::from_secs(2), rds_cli::ping(&conn, seq)).await,
                Ok(Ok(_))
            );
            seq += 1;
            if ok {
                let now = conn.current_path_stats().and_then(|s| s.relay_slot);
                if now == Some(survivor) {
                    return anyhow::Ok((started.elapsed(), failed, now));
                }
            } else {
                failed += 1;
            }
        }
    };
    // drain() only resolves once the grace window ends — run it
    // concurrently so the probe loop measures in-flight migration.
    let (fail_res, probe_res) =
        tokio::join!(world.fail_relay(usize::from(active), graceful), probe);
    fail_res.context("relay failover op")?;
    let (recovery, failed_probes, after) = probe_res?;

    // Post-migration steady state on the surviving attachment.
    let mut samples = Vec::new();
    for _ in 0..5 {
        let rtt = rds_cli::ping(&conn, rand::random::<u64>()).await?;
        samples.push(rtt.as_nanos() as u64);
    }
    anyhow::ensure!(
        conn.close_kind().is_none(),
        "connection must survive the relay failure"
    );

    let mut metrics = world.metrics_snapshot(Some(&conn));
    world.close().await;
    world
        .enforce_path_integrity(&metrics, 8)
        .context("path integrity")?;

    metrics.insert("migration_recovery_ns".into(), recovery.as_nanos() as u64);
    metrics.insert("migration_failed_probes".into(), failed_probes);
    metrics.insert("migration_slot_before".into(), u64::from(active));
    if let Some(slot) = after {
        metrics.insert("migration_slot_after".into(), u64::from(slot));
    }
    for (slot, (fwd, _, bytes)) in world.owned_relay_stats().iter().enumerate() {
        metrics.insert(format!("relay{slot}_forwarded_datagrams"), *fwd);
        metrics.insert(format!("relay{slot}_forwarded_bytes"), *bytes);
    }
    let mode = failover_mode(graceful);
    metrics.insert("phase_connect_ns".into(), connect_elapsed.as_nanos() as u64);
    let mut meta = meta(Scenario::Migration.name(), p, mode, None);
    meta.impairment = None;
    Ok(BenchReport {
        meta,
        rtt: Percentiles::of(&samples),
        throughput_mib_s: None,
        attempts: None,
        metrics,
        notes: vec![format!(
            "{mode}: slot {active} failed → resumed {recovery:?} on slot \
             {survivor}, {failed_probes} probes lost in flight; \
             post-migration RTT from the surviving slot"
        )],
    })
}

/// Known-rate calibration (W0.2): transfer over a direct path capped
/// at `--rate-mbps` (default 10 Mbps — deliberately below the loopback
/// transport ceiling so the cap is the binding constraint; on hardware
/// too slow to reach it the lane correctly reports miscalibration).
/// Measured verified goodput must land within `[cap*0.4, cap*1.2]`
/// bytes/s: below the floor means the transport under-performs the
/// imposed ceiling, above the cap means the limiter or the measurement
/// is fabricating throughput.
async fn calibration(p: &Params) -> anyhow::Result<BenchReport> {
    let rate_mbps = p.impairment.rate_mbps.unwrap_or(10.0);
    let imp = Impairment {
        rate_mbps: Some(rate_mbps),
        ..Impairment::clean()
    };
    let (mut report, measurement) = transfer_on(p, Path::DirectImpaired(imp)).await?;
    let expected_bps = rate_mbps * 1e6 / 8.0;
    let measured_bps = measurement.bytes as f64 / measurement.elapsed.as_secs_f64();
    let ratio = measured_bps / expected_bps;
    report
        .metrics
        .insert("calibration_expected_bytes_s".into(), expected_bps as u64);
    report
        .metrics
        .insert("calibration_measured_bytes_s".into(), measured_bps as u64);
    report
        .metrics
        .insert("calibration_ratio_milli".into(), (ratio * 1000.0) as u64);
    if p.impairment.rate_mbps.is_none() {
        report
            .notes
            .push("no --rate-mbps given; calibrated at the built-in 10 Mbps".into());
    }
    anyhow::ensure!(
        (0.4..=1.2).contains(&ratio),
        "calibration out of band: measured {measured_bps:.0} B/s vs cap {expected_bps:.0} B/s (ratio {ratio:.3}; expected 0.4..=1.2)"
    );
    Ok(report)
}

/// Path-loss mid-transfer (W0.3): the upload starts on a clean
/// socket-impaired path; once a third of the payload has crossed the
/// impairment device, a ~1.5s loss+delay burst is imposed on the
/// client's live socket, then lifted. The verified receipt must still arrive —
/// QUIC retransmission is the recovery mechanism — and the drop
/// counter must prove the loss actually engaged (not a vacuous pass).
/// noq only: iroh's impairment is a static proxy leg created at spawn.
async fn recovery(p: &Params) -> anyhow::Result<BenchReport> {
    #[cfg(not(feature = "transport-noq"))]
    {
        let _ = p;
        anyhow::bail!("recovery requires --features transport-noq (socket-level live impairment)");
    }
    #[cfg(feature = "transport-noq")]
    {
        if p.backend != "noq" {
            return Ok(skipped(
                "recovery",
                p,
                "direct-impaired",
                "live socket impairment is a noq-path device; iroh impairment is a static spawn-time proxy",
            ));
        }
        // Payload is capped: a transient-loss lane only needs enough
        // bytes in flight to be caught mid-transfer — the full
        // `transfer_mib` (32 MiB default) would blow `all`'s deadline.
        let total = p
            .transfer_mib
            .min(2)
            .checked_mul(1024 * 1024)
            .context("transfer size overflow")?;
        anyhow::ensure!(total > 0, "transfer size must be positive");
        let world = World::spawn(
            Path::DirectImpaired(Impairment::clean()),
            p.transport_backend()?,
        )
        .await
        .context("spawn world")?;
        let outcome = tokio::time::timeout(p.timeout, async {
            let conn = rds_cli::connect(&world.client, world.target.clone())
                .await
                .context("connect failed")?;
            let (host, port) = world.transfer_target();
            let (mut send, mut recv) = rds_cli::open_tcp(&conn, &host, port).await?;
            let upload = tokio::spawn(async move {
                crate::transfer::send_verified(&mut send, &mut recv, total).await
            });
            // Impose loss once the transfer is in flight: trigger at
            // ~1/3 of payload crossing the impairment devices, with a
            // ceiling poll so a slow ramp cannot starve the trigger.
            let trigger = total / 3;
            loop {
                if world.impair_totals().bytes >= trigger {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // A bounded burst lands on the client's egress only — the
            // payload direction. ACKs (agent→client) stay clean so
            // QUIC retransmission recovers inside the deadline; a
            // bidirectional collapse would just stall the conn. After
            // a fixed window the link is restored, proving recovery —
            // not just survival under sustained loss. The imposed
            // profile follows --loss / --delay-ms when given; loss is
            // floored at 15% so a short burst cannot pass vacuously
            // with zero drops.
            let loss = p.impairment.loss.max(0.15);
            let delay_ms = if p.impairment.delay_ms > 0 {
                p.impairment.delay_ms
            } else {
                25
            };
            let imposed = Impairment {
                loss,
                delay_ms,
                ..Impairment::clean()
            };
            let t_imposed = Instant::now();
            world.set_socket_impairment_at(1, imposed);
            tokio::time::sleep(Duration::from_millis(1500)).await;
            world.set_socket_impairment_at(1, Impairment::clean());
            let imposed_window = t_imposed.elapsed();
            let measurement = upload.await.context("upload task")??;
            anyhow::Ok((
                measurement,
                imposed,
                imposed_window,
                world.metrics_snapshot(Some(&conn)),
            ))
        })
        .await
        .context("recovery operation timed out")
        .and_then(|result| result);
        let cleanup = tokio::time::timeout(Duration::from_secs(5), world.close()).await;
        let (measurement, imposed, imposed_window, mut metrics) = outcome?;
        cleanup.context("world shutdown timed out")?;
        let dropped = metrics
            .get("bench_impair_dropped_datagrams")
            .copied()
            .unwrap_or(0);
        anyhow::ensure!(
            dropped > 0,
            "imposed loss never engaged — no datagrams dropped; scenario would pass vacuously"
        );
        metrics.insert("transfer_verified_bytes".into(), measurement.bytes);
        metrics.insert(
            "transfer_completion_ns".into(),
            measurement
                .elapsed
                .as_nanos()
                .try_into()
                .unwrap_or(u64::MAX),
        );
        metrics.insert("recovery_loss_milli".into(), (imposed.loss * 1000.0) as u64);
        metrics.insert("recovery_delay_ms".into(), imposed.delay_ms);
        metrics.insert(
            "recovery_imposed_window_ns".into(),
            imposed_window.as_nanos().try_into().unwrap_or(u64::MAX),
        );
        Ok(BenchReport {
            meta: meta("recovery", p, world.path.label(), Some(imposed)),
            rtt: None,
            throughput_mib_s: Some(
                measurement.bytes as f64 / (1024.0 * 1024.0) / measurement.elapsed.as_secs_f64(),
            ),
            attempts: None,
            metrics,
            notes: vec![format!(
                "path loss imposed mid-transfer at ~1/3 payload \
                 (loss={}, delay={}ms on client egress, restored after \
                 {:?}); verified receipt still arrived; {dropped} \
                 datagrams dropped under the imposed loss",
                imposed.loss, imposed.delay_ms, imposed_window
            )],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn verified_transfer(backend: &str) {
        let p = Params {
            transfer_mib: 1,
            backend: backend.into(),
            ..Params::default()
        };
        let report = tokio::time::timeout(Duration::from_secs(40), transfer(&p))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.meta.scenario, "transfer-receiver-ack-v1");
        assert_eq!(report.metrics["transfer_verified_bytes"], 1024 * 1024);
        assert!(report.metrics["transfer_completion_ns"] > 0);
        let rate = report.throughput_mib_s.unwrap();
        assert!(rate.is_finite() && rate > 0.0);
    }

    #[tokio::test]
    async fn transfer_receipt_crosses_real_iroh_and_forwarded_tcp() {
        verified_transfer("iroh").await;
    }

    #[cfg(feature = "transport-noq")]
    #[tokio::test]
    async fn transfer_receipt_crosses_real_noq_and_forwarded_tcp() {
        verified_transfer("noq").await;
    }

    #[tokio::test]
    async fn invalid_transfer_parameters_fail_before_startup() {
        for p in [
            Params {
                transfer_mib: 0,
                ..Params::default()
            },
            Params {
                transfer_mib: u64::MAX,
                ..Params::default()
            },
            Params {
                timeout: Duration::ZERO,
                ..Params::default()
            },
        ] {
            assert!(transfer(&p).await.is_err());
        }
    }
}
