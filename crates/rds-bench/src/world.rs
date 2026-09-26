//! Test world: an in-process relay, a serving agent and a client
//! endpoint, with the advertised address controlling which path QUIC
//! actually uses.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;

#[cfg(feature = "transport-noq")]
use anyhow::Context as _;
use rds_agent::{Agent, AgentPolicy};
use rds_net::{Endpoint, EndpointAddr, EndpointConfig, TransportAddr, bind_endpoint};
use tokio::net::TcpListener;
use tokio::task::{JoinHandle, JoinSet};

use crate::impair::{self, Impairment, Proxy};

/// Which path the advertised address exposes.
#[derive(Debug, Clone, Copy)]
pub enum Path {
    /// Ticket carries the agent's UDP socket address only.
    Direct,
    /// Impaired direct path. iroh: ticket carries a UDP proxy address
    /// (proxied leg only). noq: each endpoint's socket itself impairs —
    /// every datagram crosses impairment regardless of path migration.
    DirectImpaired(Impairment),
    /// Ticket carries only the relay URL — QUIC must use the relay path.
    RelayOnly,
    /// Relay ticket with the endpoint↔relay attachment impaired
    /// (noq only; the iroh relay leg is TCP, outside the UDP model).
    RelayImpaired(Impairment),
    /// Direct plus relay, as a real deployment would advertise.
    Mixed,
}

impl Path {
    pub fn label(&self) -> &'static str {
        match self {
            Path::Direct => "direct",
            Path::DirectImpaired(_) => "direct-impaired",
            Path::RelayOnly => "relay",
            Path::RelayImpaired(_) => "relay-impaired",
            Path::Mixed => "mixed",
        }
    }

    /// Impairment this path applies, if any.
    pub fn impairment(&self) -> Option<Impairment> {
        match self {
            Path::DirectImpaired(i) | Path::RelayImpaired(i) => Some(*i),
            _ => None,
        }
    }
}

/// Relay kept alive for a world's lifetime; the inner value is held
/// for `Drop`, never read.
#[allow(dead_code)]
pub(crate) enum WorldRelay {
    None,
    Iroh(iroh_relay::server::Server),
    #[cfg(feature = "transport-noq")]
    Owned(rds_relay::server::Relay),
}

/// Everything a scenario needs; dropping `tasks` stops the world.
pub struct World {
    pub client: Endpoint,
    pub target: EndpointAddr,
    pub agent: Arc<Agent>,
    pub transfer_port: u16,
    /// The advertised path this world pins the run to.
    pub path: Path,
    /// UDP proxy leg for iroh impaired paths.
    pub proxy_stats: Option<Arc<Proxy>>,
    /// Socket-level impairment probes (noq endpoints), one per endpoint.
    #[cfg(feature = "transport-noq")]
    pub socket_stats: Vec<impair::StatsHandle>,
    /// Impair proxies sitting on endpoint↔relay attachment legs.
    relay_leg_proxies: Vec<Arc<Proxy>>,
    tasks: Vec<JoinHandle<()>>,
    _relay: WorldRelay,
}

impl World {
    /// TCP verified-receipt service the agent permits (for `transfer`).
    pub fn transfer_target(&self) -> (String, u16) {
        ("127.0.0.1".into(), self.transfer_port)
    }

    /// Graceful endpoint shutdown; scenarios call this before the
    /// report so drop-aborts don't pollute logs with ungraceful-close
    /// errors.
    pub async fn close(&self) {
        self.client.close().await;
        self.agent.endpoint.close().await;
    }

    /// Metrics snapshot for the report (G7): if `conn` is given, its
    /// cumulative path stats are folded into the client registry first,
    /// then both registries are read — `client_*`/`agent_*` prefixes.
    /// The agent samples its own connections once a second, so a short
    /// scenario still sees its side via this one-shot fold.
    pub fn metrics_snapshot(
        &self,
        conn: Option<&rds_net::Connection>,
    ) -> std::collections::BTreeMap<String, u64> {
        let mut out = std::collections::BTreeMap::new();
        // Keep the observation owner through the scrape: dropping a sampler
        // invalidates its last selected-path gauge, without retaining I/O.
        let mut sampler = conn.map(|conn| self.client.metrics().sampler(conn.clone()));
        if let Some(sampler) = &mut sampler {
            sampler.sample();
        }
        for (k, v) in self.client.metrics().snapshot() {
            out.insert(format!("client_{k}"), v);
        }
        for (k, v) in self.agent.endpoint.metrics().snapshot() {
            out.insert(format!("agent_{k}"), v);
        }
        // Offered/delivered accounting for whatever impairment actually
        // ran under this world — machine-readable, not prose.
        let imp = self.impair_totals();
        if imp.probes > 0 {
            out.insert("bench_impair_probes".into(), imp.probes);
            out.insert("bench_impair_forwarded_datagrams".into(), imp.forwarded);
            out.insert("bench_impair_dropped_datagrams".into(), imp.dropped);
            out.insert("bench_impair_forwarded_bytes".into(), imp.bytes);
        }
        out
    }

    /// Aggregate counters across every impairment device in the world:
    /// iroh direct proxy, noq endpoint sockets, relay-leg proxies.
    pub fn impair_totals(&self) -> ImpairTotals {
        let mut t = ImpairTotals::default();
        let mut add = |s: impair::ProxyStats| {
            t.forwarded += s.forwarded;
            t.dropped += s.dropped;
            t.bytes += s.bytes;
            t.probes += 1;
        };
        if let Some(p) = &self.proxy_stats {
            add(p.stats());
        }
        for p in &self.relay_leg_proxies {
            add(p.stats());
        }
        #[cfg(feature = "transport-noq")]
        for h in &self.socket_stats {
            add(h.get());
        }
        t
    }

    /// Post-traffic integrity check (W0.3): observed counters must prove
    /// the claimed path carried the payload and impairment — if any —
    /// actually sat under it. `min_payload_bytes` is the floor the caller
    /// knows it moved (probe bytes, transfer size).
    pub fn enforce_path_integrity(
        &self,
        metrics: &std::collections::BTreeMap<String, u64>,
        min_payload_bytes: u64,
    ) -> anyhow::Result<()> {
        let get = |k: &str| metrics.get(k).copied().unwrap_or(0);
        let direct_rx = get("client_rds_net_bytes_received_total{via=\"direct\"}");
        let relay_rx = get("client_rds_net_bytes_received_total{via=\"relay\"}");
        let relay_tx = get("client_rds_net_datagrams_sent_total{via=\"relay\"}");
        match self.path {
            Path::Direct | Path::DirectImpaired(_) => {
                anyhow::ensure!(
                    relay_tx == 0 && relay_rx == 0,
                    "{}: relay counters non-zero (rx={relay_rx} tx={relay_tx}) — \
                     traffic escaped the advertised direct path",
                    self.path.label()
                );
            }
            Path::RelayOnly | Path::RelayImpaired(_) => {
                anyhow::ensure!(
                    relay_tx > 0,
                    "{}: no relay datagrams observed — ticket did not route via relay",
                    self.path.label()
                );
                anyhow::ensure!(
                    relay_rx >= min_payload_bytes,
                    "{}: relay carried {relay_rx}B < {min_payload_bytes}B payload — \
                     a clean unadvertised path moved the data",
                    self.path.label()
                );
                anyhow::ensure!(
                    direct_rx < min_payload_bytes,
                    "{}: direct path carried {direct_rx}B ≥ payload floor — \
                     silent switch to a clean address",
                    self.path.label()
                );
            }
            Path::Mixed => {
                anyhow::ensure!(
                    relay_tx > 0 || direct_rx > 0,
                    "mixed: neither direct nor relay path observed traffic"
                );
            }
        }
        if let Some(imp) = self.path.impairment() {
            let t = self.impair_totals();
            anyhow::ensure!(
                t.probes > 0,
                "{}: impaired path claims no impairment device ran",
                self.path.label()
            );
            anyhow::ensure!(
                t.forwarded + t.dropped > 0,
                "{}: impairment devices saw zero datagrams — traffic bypassed the impaired leg",
                self.path.label()
            );
            // Offered-vs-observed floor: every datagram the client sent
            // on the claimed kind had to enter an impairment device — a
            // partial count means part of the payload rode a clean path
            // the ticket did not name (e.g. migration to an in-band
            // learned address).
            let via = match self.path {
                Path::RelayImpaired(_) => "relay",
                _ => "direct",
            };
            let sent = get(&format!(
                "client_rds_net_datagrams_sent_total{{via=\"{via}\"}}"
            ));
            let offered = t.forwarded + t.dropped;
            anyhow::ensure!(
                offered >= sent,
                "{}: impairment saw {offered} of {sent} client datagrams — \
                 a clean path carried the rest",
                self.path.label()
            );
            // A configured one-way delay must surface in the measured
            // path RTT; a bypassed impaired leg leaves loopback-fast RTT.
            // rtt_us = 0 means no live path sample was folded into the
            // registry (e.g. cold-connect scenarios) — not a measurement.
            if imp.delay_ms > 0
                && let Some(&rtt_us) = metrics.get("client_rds_net_rtt_us")
                && rtt_us > 0
            {
                let floor = imp.delay_ms * 1000 / 2;
                anyhow::ensure!(
                    rtt_us >= floor,
                    "{}: selected-path RTT {rtt_us}us < {floor}us (half the \
                     configured delay) — payload is not crossing the impaired leg",
                    self.path.label()
                );
            }
        }
        Ok(())
    }
}

/// Summed impairment counters across all devices in a world.
#[derive(Debug, Default, Clone, Copy)]
pub struct ImpairTotals {
    pub probes: u64,
    pub forwarded: u64,
    pub dropped: u64,
    pub bytes: u64,
}

/// noq endpoint on an `ImpairingSocket`: every outbound datagram — on any
/// path, including a later migration — passes loss/jitter/rate first.
#[cfg(feature = "transport-noq")]
async fn bind_noq_impaired(
    config: EndpointConfig,
    imp: Impairment,
) -> anyhow::Result<(Endpoint, impair::StatsHandle)> {
    let runtime: Arc<dyn noq::Runtime> = Arc::new(noq::TokioRuntime);
    let std_sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let inner = noq::Runtime::wrap_udp_socket(&*runtime, std_sock)?;
    let local = inner.local_addr()?;
    let (socket, stats) = impair::ImpairingSocket::wrap(inner, imp);
    let ep = rds_net::bind_noq_with_socket(config, Box::new(socket), vec![local], runtime).await?;
    Ok((ep, stats))
}

impl Drop for World {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl World {
    /// Spin up relay + agent + client on `backend` and return the world
    /// plus the target address the client should dial for `path`.
    ///
    /// Endpoints run `discovery: false` and, outside `Mixed`, a one-path
    /// multipath cap: dialing uses exactly the advertised ticket, so a
    /// scenario cannot silently migrate onto an unadvertised clean path
    /// (W0.3). `noq` worlds get the owned `rds-relay` server; `iroh`
    /// worlds keep the in-process iroh relay.
    pub async fn spawn(path: Path, backend: rds_net::Backend) -> anyhow::Result<World> {
        let mut tasks = Vec::new();
        let wants_relay = matches!(path, Path::RelayOnly | Path::RelayImpaired(_) | Path::Mixed);
        let relay_impairment = match path {
            Path::RelayImpaired(i) => Some(i),
            _ => None,
        };
        let direct_impairment = match path {
            Path::DirectImpaired(i) => Some(i),
            _ => None,
        };
        #[cfg(not(feature = "transport-noq"))]
        let _ = &direct_impairment;

        #[cfg(not(feature = "transport-noq"))]
        if wants_relay && backend != rds_net::Backend::Iroh {
            anyhow::bail!(
                "path {:?} needs a relay; backend {backend:?} has no relay transport yet (WS2)",
                path.label()
            );
        }

        // Relay per backend: iroh spawns the in-process HTTP relay, noq
        // spawns the owned relay server and attaches endpoints via
        // `relay_endpoint`. For RelayImpaired each endpoint dials the
        // relay through its own UDP impair proxy, so the attachment legs
        // carry loss/jitter/rate in both directions.
        #[cfg_attr(not(feature = "transport-noq"), allow(unused_mut))]
        let mut relay_leg_proxies: Vec<Arc<Proxy>> = Vec::new();
        let mut iroh_relay_url: Option<String> = None;
        #[cfg(feature = "transport-noq")]
        let mut owned_relay_dial: Option<EndpointAddr> = None;
        let relay = match backend {
            rds_net::Backend::Iroh => {
                if relay_impairment.is_some() {
                    anyhow::bail!(
                        "iroh relay legs are TCP; UDP impairment cannot sit \
                         below them — relay-impaired requires --backend noq"
                    );
                }
                if !wants_relay {
                    // Direct worlds bind without any relay transport —
                    // `Transports::DirectOnly` refuses a configured relay.
                    WorldRelay::None
                } else {
                    let mut config = iroh_relay::server::ServerConfig::default();
                    config.relay = Some(iroh_relay::server::RelayConfig::new(
                        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
                    ));
                    let server = iroh_relay::server::Server::spawn(config).await?;
                    iroh_relay_url = Some(format!("http://{}", server.http_addr().unwrap()));
                    WorldRelay::Iroh(server)
                }
            }
            #[cfg(feature = "transport-noq")]
            rds_net::Backend::Noq => {
                if !wants_relay {
                    WorldRelay::None
                } else {
                    let server = rds_relay::server::serve(
                        EndpointConfig {
                            backend: rds_net::Backend::Noq,
                            bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
                            discovery: false,
                            ..Default::default()
                        },
                        // Synthetic loopback world: open relay.
                        Vec::new(),
                    )
                    .await
                    .context("spawn owned relay")?;
                    let real = server.endpoint_addr();
                    let real_udp = real
                        .addrs
                        .iter()
                        .find_map(|a| match a {
                            TransportAddr::Ip(sa) => Some(*sa),
                            _ => None,
                        })
                        .context("owned relay advertises no udp addr")?;
                    if let Some(imp) = relay_impairment {
                        for _ in 0..2 {
                            relay_leg_proxies.push(Arc::new(
                                impair::spawn(real_udp, imp)
                                    .await
                                    .context("relay-leg impair proxy")?,
                            ));
                        }
                    }
                    owned_relay_dial = Some(real);
                    WorldRelay::Owned(server)
                }
            }
            #[allow(unreachable_patterns)]
            _ => WorldRelay::None,
        };

        // `relay_endpoint` an attached noq endpoint should dial: the
        // relay's real addr, or the endpoint's impair-proxy leg.
        #[cfg(feature = "transport-noq")]
        let noq_relay_attach = |leg: usize| -> Option<EndpointAddr> {
            owned_relay_dial.as_ref().map(|real| {
                let mut addr = EndpointAddr::new(real.id);
                if let Some(proxy) = relay_leg_proxies.get(leg) {
                    addr = addr.with_ip_addr(proxy.listen);
                } else {
                    for a in &real.addrs {
                        if let TransportAddr::Ip(sa) = a {
                            addr = addr.with_ip_addr(*sa);
                        }
                    }
                }
                addr
            })
        };

        let endpoint_config = |relay_leg: usize| -> anyhow::Result<EndpointConfig> {
            #[cfg(not(feature = "transport-noq"))]
            let _ = relay_leg;
            let mut config = EndpointConfig::default().with_backend(backend);
            // Bound the peer-path kinds to exactly what the world
            // advertises — both backends exchange direct candidates
            // in-band (iroh QNT cannot be disabled; its config floor is
            // 8), so transports, not tickets, are what keeps traffic on
            // the measured path.
            config.transports = match path {
                Path::Direct | Path::DirectImpaired(_) => rds_net::Transports::DirectOnly,
                Path::RelayOnly | Path::RelayImpaired(_) => rds_net::Transports::RelayOnly,
                Path::Mixed => rds_net::Transports::All,
            };
            // Dial exactly the advertised ticket; nothing is learned
            // in-band. Non-Mixed worlds additionally pin the connection
            // to its established path and disable observed-address
            // reports as defense in depth under the transport bound.
            config.discovery = false;
            if !matches!(path, Path::Mixed) {
                config.max_multipath_paths = Some(1);
                config.observed_address_reports = false;
            }
            if let Some(url) = &iroh_relay_url {
                config = config.with_relay(url)?;
            }
            #[cfg(feature = "transport-noq")]
            if let rds_net::Backend::Noq = backend {
                config.relay_endpoint = noq_relay_attach(relay_leg);
            }
            Ok(config)
        };

        // noq impaired-direct endpoints wrap their UDP socket so
        // impairment sits underneath QUIC — immune to path migration.
        #[cfg(feature = "transport-noq")]
        let mut socket_stats: Vec<impair::StatsHandle> = Vec::new();
        let (agent_ep, client_ep);
        #[cfg(feature = "transport-noq")]
        if let (rds_net::Backend::Noq, Some(imp)) = (backend, direct_impairment) {
            let (a, sa) = bind_noq_impaired(endpoint_config(0)?, imp).await?;
            let (c, sc) = bind_noq_impaired(endpoint_config(1)?, imp).await?;
            socket_stats.extend([sa, sc]);
            agent_ep = a;
            client_ep = c;
        } else {
            agent_ep = bind_endpoint(endpoint_config(0)?).await?;
            client_ep = bind_endpoint(endpoint_config(1)?).await?;
        }
        #[cfg(not(feature = "transport-noq"))]
        {
            agent_ep = bind_endpoint(endpoint_config(0)?).await?;
            client_ep = bind_endpoint(endpoint_config(1)?).await?;
        }

        // TCP verified-receipt target for the transfer scenario.
        let transfer_port = spawn_transfer_target(&mut tasks).await?;

        let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), transfer_port));
        policy.allow.insert(client_ep.id());
        policy.allow_any_tcp = true; // bench targets are ours
        let agent = Arc::new(Agent::new(agent_ep, policy));
        tasks.push(tokio::spawn({
            let agent = agent.clone();
            async move {
                let _ = agent.run().await;
            }
        }));

        // The agent's real UDP address and relay address from its addr().
        // A relay-only endpoint advertises no direct addrs at all
        // (iroh has no IP transport bound; noq suppresses them), so the
        // UDP addr is only resolved for paths that need it.
        let advertised = agent.endpoint.addr();
        let agent_udp = || -> anyhow::Result<SocketAddr> {
            advertised
                .addrs
                .iter()
                .filter_map(|a| match a {
                    TransportAddr::Ip(sa) => Some(*sa),
                    _ => None,
                })
                .find(|sa| sa.ip().is_loopback())
                .or_else(|| {
                    advertised.addrs.iter().find_map(|a| match a {
                        TransportAddr::Ip(sa) => Some(*sa),
                        _ => None,
                    })
                })
                .ok_or_else(|| anyhow::anyhow!("agent endpoint advertises no udp addr"))
        };
        // Relay attach is asynchronous on iroh (Minimal preset): poll
        // the advertised addr until the relay candidate appears — bounded
        // so a broken attach fails loudly instead of hanging the world.
        let agent_relay = if wants_relay {
            let mut found = None;
            for _ in 0..100 {
                found = agent.endpoint.addr().addrs.iter().find_map(|a| match a {
                    TransportAddr::Relay(u) => Some(u.clone()),
                    _ => None,
                });
                if found.is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            found
        } else {
            None
        };

        let mut proxy_stats = None;
        let mut addrs = BTreeSet::new();
        match path {
            Path::Direct => {
                addrs.insert(TransportAddr::Ip(agent_udp()?));
            }
            Path::DirectImpaired(cfg) => {
                #[cfg(feature = "transport-noq")]
                let socket_impaired = backend == rds_net::Backend::Noq;
                #[cfg(not(feature = "transport-noq"))]
                let socket_impaired = false;
                if socket_impaired {
                    // Socket-level impairment: the ticket carries the real
                    // address — every datagram crosses the impaired socket.
                    addrs.insert(TransportAddr::Ip(agent_udp()?));
                } else {
                    let proxy = impair::spawn(agent_udp()?, cfg).await?;
                    addrs.insert(TransportAddr::Ip(proxy.listen));
                    proxy_stats = Some(Arc::new(proxy));
                }
            }
            Path::RelayOnly | Path::RelayImpaired(_) => {
                let relay_addr = agent_relay
                    .ok_or_else(|| anyhow::anyhow!("agent endpoint has no relay addr"))?;
                addrs.insert(TransportAddr::Relay(relay_addr));
            }
            Path::Mixed => {
                let relay_addr = agent_relay
                    .ok_or_else(|| anyhow::anyhow!("agent endpoint has no relay addr"))?;
                addrs.insert(TransportAddr::Ip(agent_udp()?));
                addrs.insert(TransportAddr::Relay(relay_addr));
            }
        }

        Ok(World {
            client: client_ep,
            target: EndpointAddr {
                id: agent.id(),
                addrs,
            },
            agent,
            transfer_port,
            path,
            proxy_stats,
            #[cfg(feature = "transport-noq")]
            socket_stats,
            relay_leg_proxies,
            tasks,
            _relay: relay,
        })
    }
}

/// Bounded, owned receiver tasks; errors close without a success receipt.
async fn spawn_transfer_target(tasks: &mut Vec<JoinHandle<()>>) -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tasks.push(tokio::spawn(async move {
        let mut receivers = JoinSet::new();
        loop {
            tokio::select! {
                _ = receivers.join_next(), if !receivers.is_empty() => {},
                accepted = listener.accept(), if receivers.len() < 8 => {
                    let Ok((mut socket, _)) = accepted else { break };
                    receivers.spawn(async move {
                        let _ = crate::transfer::receive(&mut socket).await;
                    });
                }
            }
        }
    }));
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn transfer_target_acknowledges_received_bytes_and_digest_after_fin() {
        let mut tasks = Vec::new();
        let port = spawn_transfer_target(&mut tasks).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            let body = b"payload with a receiver completion barrier";
            socket.write_all(body).await.unwrap();
            socket.shutdown().await.unwrap();
            let mut receipt = [0; 40];
            socket
                .read_exact(&mut receipt)
                .await
                .expect("receiver must acknowledge before throughput is reported");
            assert_eq!(&receipt[..8], &(body.len() as u64).to_be_bytes());
            assert_eq!(&receipt[8..], blake3::hash(body).as_bytes());
            assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
        })
        .await
        .unwrap();
    }
}
