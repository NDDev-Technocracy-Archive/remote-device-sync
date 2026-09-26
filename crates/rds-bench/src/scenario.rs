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
use crate::world::{Path, World};

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
        return Ok(out);
    }
    run_one(s, p).await
}

async fn run_one(s: Scenario, p: &Params) -> anyhow::Result<Vec<BenchReport>> {
    // Fail fast on an unselectable backend instead of per-scenario noise.
    p.transport_backend()?;
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
        Scenario::Impaired => ping(p, Path::DirectImpaired(p.impairment), Some(p.impairment))
            .await
            .map(|mut r| {
                r.meta.scenario = "impaired".into();
                vec![r]
            }),
        Scenario::ResolveConnect => resolve_connect(p).await.map(|r| vec![r]),
        Scenario::All => unreachable!("handled in run"),
    }
}

fn meta(scenario: &str, p: &Params, path: &str, impairment: Option<Impairment>) -> BenchMeta {
    BenchMeta {
        scenario: scenario.into(),
        backend: p.backend.clone(),
        path: path.into(),
        impairment,
        unix_ts: unix_ts(),
        git: git_sha(),
    }
}

fn proxy_note(world: &World) -> Vec<String> {
    world
        .proxy_stats
        .as_ref()
        .map(|p| {
            let s = p.stats();
            vec![format!(
                "proxy: forwarded={} dropped={} bytes={}",
                s.forwarded, s.dropped, s.bytes
            )]
        })
        .unwrap_or_default()
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
    world.close().await;
    Ok(BenchReport {
        meta: meta("handshake", p, world.path_label(), impairment_of(&path)),
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
    let conn = tokio::time::timeout(
        p.timeout,
        rds_cli::connect(&world.client, world.target.clone()),
    )
    .await
    .context("connect timed out")?
    .context("connect failed")?;
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
    let metrics = world.metrics_snapshot(Some(&conn));
    world.close().await;
    Ok(BenchReport {
        meta: meta("ping", p, world.path_label(), impairment),
        rtt: Percentiles::of(&samples),
        throughput_mib_s: None,
        attempts: None,
        metrics,
        notes: proxy_note(&world),
    })
}

/// `transfer_mib` MiB over one forwarded stream, ending at a verified receipt.
async fn transfer(p: &Params) -> anyhow::Result<BenchReport> {
    let total = p
        .transfer_mib
        .checked_mul(1024 * 1024)
        .context("transfer size overflow")?;
    anyhow::ensure!(total > 0, "transfer size must be positive");
    anyhow::ensure!(!p.timeout.is_zero(), "transfer timeout must be positive");
    let world = tokio::time::timeout(
        p.timeout,
        World::spawn(Path::Direct, p.transport_backend()?),
    )
    .await
    .context("world startup timed out")?
    .context("spawn world")?;
    // A single deadline covers connect, OpenTcp, upload, receipt and EOF.
    let outcome = tokio::time::timeout(p.timeout, async {
        let conn = rds_cli::connect(&world.client, world.target.clone())
            .await
            .context("connect failed")?;
        let (host, port) = world.transfer_target();
        let (mut send, mut recv) = rds_cli::open_tcp(&conn, &host, port).await?;
        let measurement = crate::transfer::send_verified(&mut send, &mut recv, total).await?;
        anyhow::Ok((measurement, world.metrics_snapshot(Some(&conn))))
    })
    .await
    .context("transfer operation timed out")
    .and_then(|result| result);
    let cleanup = tokio::time::timeout(Duration::from_secs(5), world.close()).await;
    let (measurement, mut metrics) = outcome?;
    cleanup.context("world shutdown timed out")?;
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
    notes.push("receiver-ack-v1: byte count + BLAKE3 digest + EOF; payload generation/hash, upload and receipt are timed; connect/OpenTcp excluded; not comparable to historical sender-finish results".into());
    Ok(BenchReport {
        meta: meta(Scenario::Transfer.name(), p, world.path_label(), None),
        rtt: None,
        throughput_mib_s: Some(mib_s),
        attempts: None,
        metrics,
        notes,
    })
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

    let mut relay_config = iroh_relay::server::ServerConfig::default();
    relay_config.relay = Some(iroh_relay::server::RelayConfig::new(
        "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let relay = iroh_relay::server::Server::spawn(relay_config).await?;
    let relay_url = format!("http://{}", relay.http_addr().unwrap());

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
    let agent_ep = bind_endpoint(
        EndpointConfig {
            secret_key: Some(agent_key.clone()),
            backend,
            ..Default::default()
        }
        .with_relay(&relay_url)?,
    )
    .await?;
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
    let mut ok = 0u64;
    // Client endpoints are per-iteration; accumulate their registries
    // so the report keeps total connection/path counters (G7).
    let mut client_metrics: BTreeMap<String, u64> = BTreeMap::new();
    for _ in 0..p.iterations {
        // A fresh client endpoint keeps both resolve and handshake
        // cold, matching a new `rds ssh` invocation.
        let client_ep = bind_endpoint(
            EndpointConfig {
                secret_key: Some(client_key.clone()),
                backend,
                ..Default::default()
            }
            .with_relay(&relay_url)?,
        )
        .await?;
        let timed = tokio::time::timeout(p.timeout, async {
            let t0 = Instant::now();
            let addr = rds_net::resolve_target(Some(directory.clone()), "bench-agent").await?;
            let conn = rds_cli::connect(&client_ep, addr).await?;
            rds_cli::ping(&conn, 1).await?;
            client_ep.metrics().sampler(conn.clone()).sample();
            Ok::<_, anyhow::Error>(t0.elapsed())
        })
        .await;
        if let Ok(Ok(d)) = timed {
            samples.push(d.as_nanos() as u64);
            ok += 1;
        }
        for (k, v) in client_ep.metrics().snapshot() {
            *client_metrics.entry(format!("client_{k}")).or_insert(0) += v;
        }
        client_ep.close().await;
    }
    agent_task.abort();
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
    Ok(BenchReport {
        meta: meta("resolve-connect", p, "discovered", None),
        rtt: Percentiles::of(&samples),
        throughput_mib_s: None,
        attempts: Some((ok, p.iterations as u64)),
        metrics,
        notes,
    })
}

trait PathLabel {
    fn path_label(&self) -> &'static str;
}

impl PathLabel for World {
    fn path_label(&self) -> &'static str {
        if self.proxy_stats.is_some() {
            "direct-impaired"
        } else if self
            .target
            .addrs
            .iter()
            .any(|a| matches!(a, rds_net::TransportAddr::Relay(_)))
            && self
                .target
                .addrs
                .iter()
                .any(|a| matches!(a, rds_net::TransportAddr::Ip(_)))
        {
            "mixed"
        } else if self
            .target
            .addrs
            .iter()
            .any(|a| matches!(a, rds_net::TransportAddr::Relay(_)))
        {
            "relay"
        } else {
            "direct"
        }
    }
}

fn impairment_of(path: &Path) -> Option<Impairment> {
    match path {
        Path::DirectImpaired(i) => Some(*i),
        _ => None,
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
