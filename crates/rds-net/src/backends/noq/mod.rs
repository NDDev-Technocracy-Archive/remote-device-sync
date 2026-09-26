//! Owned transport backend on `noq` — work in progress (WS1).
//!
//! Design (docs/research.md §9.1): noq is the QUIC engine; everything
//! above it is ours:
//!
//! - The endpoint binds a plain UDP socket through `noq`'s tokio runtime
//!   today; `socket::Mux` (direct UDP + relay tunnels + multi-interface)
//!   replaces it once `relay_link` lands.
//! - TLS carries raw Ed25519 public keys (RFC 7250), the same scheme the
//!   iroh backend uses — both backends are wire-compatible on `rds/0`.
//! - `Connection::open_path` turns discovered candidate addresses into
//!   QUIC paths; `PathEvents`/`NatTraversalUpdates` report QNT progress.
//! - `observed_external_addr` yields our public address in-band — no STUN
//!   service required.
//! - Path policy (`policy` module) orders candidates and opens extra
//!   paths; per-service pinning lands with the media protocol.
//!
//! Until this backend reaches parity (same-harness benchmarks vs the
//! iroh backend), `iroh` remains the default selected at bind time.

mod candidates;
mod dial;
mod drivers;
mod hmac;
pub mod policy;
pub mod relay;
pub mod socket;
mod telemetry;
#[cfg(test)]
mod telemetry_tests;
mod tls;

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use anyhow::{Context as _, bail};
use iroh::{EndpointAddr, EndpointId, SecretKey, TransportAddr};
use noq::Runtime;
use tracing::debug;

pub use tls::peer_endpoint_id;

/// Path-lifecycle values mirrored from iroh's defaults: heartbeat every
/// 5s, per-path idle timeout 15s, at most 8 concurrent multipath paths,
/// up to 32 remote NAT-traversal address candidates.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const PATH_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MULTIPATH_PATHS: u32 = 8;
const MAX_QNT_ADDRESSES: u8 = 32;

/// QUIC datagram buffer caps: 1 MiB each direction, oldest dropped first.
const DATAGRAM_BUFFER_SIZE: usize = 1 << 20;

/// QUIC transport parameters, mirroring the iroh backend's choices so
/// behavior — and benchmark numbers — are comparable across backends.
fn transport_config(max_multipath_paths: Option<u32>) -> Arc<noq::TransportConfig> {
    let mut cfg = noq::TransportConfig::default();
    cfg.keep_alive_interval(Some(HEARTBEAT_INTERVAL));
    cfg.default_path_keep_alive_interval(Some(HEARTBEAT_INTERVAL));
    cfg.default_path_max_idle_timeout(Some(PATH_MAX_IDLE_TIMEOUT));
    cfg.max_concurrent_multipath_paths(max_multipath_paths.unwrap_or(MAX_MULTIPATH_PATHS));
    cfg.max_remote_nat_traversal_addresses(MAX_QNT_ADDRESSES);
    cfg.server_handshake_migration(true);
    cfg.datagram_receive_buffer_size(Some(DATAGRAM_BUFFER_SIZE));
    cfg.datagram_send_buffer_size(DATAGRAM_BUFFER_SIZE);
    // Same tuning as the iroh backend: BBRv3 pacing for latency +
    // bufferbloat resistance, and windows above the 100Mbps x 100ms
    // defaults so bulk streams do not stall on high-BDP links.
    cfg.congestion_controller_factory(Arc::new(noq_proto::congestion::Bbr3Config::default()));
    cfg.stream_receive_window(noq_proto::VarInt::from_u32(4 * 1024 * 1024));
    cfg.send_window(32 * 1024 * 1024);
    Arc::new(cfg)
}

/// Bind an owned-transport endpoint.
///
/// One socket mux carries all QUIC traffic; multipath and QNT are
/// negotiated so additional paths and NAT traversal can be layered on
/// after connect.
pub async fn bind_endpoint(mut config: crate::EndpointConfig) -> anyhow::Result<Endpoint> {
    config.validate_for(crate::Backend::Noq)?;
    let runtime = Arc::new(noq::TokioRuntime);
    let binds = if config.bind_addrs.is_empty() {
        vec![SocketAddr::from(([0, 0, 0, 0], 0))]
    } else {
        config.bind_addrs.clone()
    };
    let mut sockets: Vec<Box<dyn noq::AsyncUdpSocket>> = Vec::with_capacity(binds.len() + 1);
    for bind in &binds {
        let socket =
            std::net::UdpSocket::bind(bind).with_context(|| format!("bind udp socket {bind}"))?;
        sockets.push(runtime.wrap_udp_socket(socket)?);
    }

    // Relay attachment: a tunnel socket joins the mux so relayed paths
    // behave like any other QUIC path — opened via candidates/QNT,
    // scheduled by the connection driver's RTT selection.
    let key = config
        .secret_key
        .clone()
        .unwrap_or_else(SecretKey::generate);
    config.secret_key = Some(key.clone());
    let relay_handle = match &config.relay_endpoint {
        Some(relay_addr) => {
            // The primary socket already owns its configured port. The outer
            // relay connection needs a separate ephemeral port on that interface.
            let relay_bind = SocketAddr::new(binds[0].ip(), 0);
            let (socket, handle) = relay::RelaySocket::connect_with_limits(
                relay_addr.clone(),
                key,
                relay_bind,
                config.relay_limits,
            )
            .await?;
            sockets.push(Box::new(socket));
            Some(handle)
        }
        None => None,
    };

    let mux = socket::Mux::new(sockets)?;
    let local_addrs = mux.local_addrs();
    bind_with_mux(config, mux, local_addrs, runtime, relay_handle).await
}

/// Bind an endpoint on a caller-provided transport.
///
/// The seam for custom transports: the socket mux (default), a
/// simulation harness socket, or any other `AsyncUdpSocket`
/// implementation. `local_addrs` advertises the transports' bound
/// addresses; `relay` carries the tunnel handle when a relay socket is
/// muxed in (None for injected transports like the sim).
pub async fn bind_with_socket(
    config: crate::EndpointConfig,
    socket: Box<dyn noq::AsyncUdpSocket>,
    local_addrs: Vec<SocketAddr>,
    runtime: Arc<dyn Runtime>,
    relay: Option<relay::RelayHandle>,
) -> anyhow::Result<Endpoint> {
    bind_socket(config, socket, local_addrs, runtime, relay, None).await
}

/// Bind a mux with shared failure state wired into endpoint advertisements and
/// connection policy. Generic `bind_with_socket` cannot introspect a trait object.
pub async fn bind_with_mux(
    config: crate::EndpointConfig,
    mux: socket::Mux,
    local_addrs: Vec<SocketAddr>,
    runtime: Arc<dyn Runtime>,
    relay: Option<relay::RelayHandle>,
) -> anyhow::Result<Endpoint> {
    let health = mux.health();
    anyhow::ensure!(
        local_addrs
            .iter()
            .all(|address| health.bound_available(*address)),
        "mux advertisements must name active bound transports"
    );
    bind_socket(
        config,
        Box::new(mux),
        local_addrs,
        runtime,
        relay,
        Some(health),
    )
    .await
}

async fn bind_socket(
    config: crate::EndpointConfig,
    socket: Box<dyn noq::AsyncUdpSocket>,
    local_addrs: Vec<SocketAddr>,
    runtime: Arc<dyn Runtime>,
    relay: Option<relay::RelayHandle>,
    transport_health: Option<socket::Health>,
) -> anyhow::Result<Endpoint> {
    config.validate_for(crate::Backend::Noq)?;
    let local_addr = local_addrs
        .first()
        .copied()
        .context("endpoint has no local address")?;
    anyhow::ensure!(
        config.relay_endpoint.is_some() == relay.is_some(),
        "injected transport relay configuration does not match its attached relay"
    );
    let secret_key = config.secret_key.unwrap_or_else(SecretKey::generate);
    let tls = tls::TlsConfig::new(secret_key.clone());

    let server_crypto = tls.server_config(config.alpns.clone())?;

    let endpoint_config =
        noq::EndpointConfig::new(Arc::new(hmac::Blake3HmacKey::new(&mut rand::rng())));

    let transport = transport_config(config.max_multipath_paths);
    let mut server_config = noq::ServerConfig::with_crypto(Arc::new(server_crypto));
    server_config.transport = transport.clone();
    // An immutable per-protocol offer avoids both silent ALPN fallback and
    // races caused by changing the endpoint's default config before dialing.
    let mut client_configs = std::collections::BTreeMap::new();
    for alpn in &config.alpns {
        let crypto = tls.client_config(vec![alpn.clone()])?;
        let mut client = noq::ClientConfig::new(Arc::new(crypto));
        client.transport_config(transport.clone());
        client_configs.insert(alpn.clone(), client);
    }

    let endpoint = noq::Endpoint::new_with_abstract_socket(
        endpoint_config,
        Some(server_config),
        socket,
        runtime,
    )
    .context("create noq endpoint")?;

    debug!(?local_addrs, id = %secret_key.public(), "noq endpoint bound");

    Ok(Endpoint {
        inner: endpoint,
        id: secret_key.public(),
        local_addr,
        local_addrs,
        client_configs: Arc::new(client_configs),
        relay,
        metrics: crate::metrics::Registry::default(),
        drivers: Arc::new(drivers::Drivers::new(transport_health.clone())),
        transport_health,
        transports: config.transports,
        pinned: config.max_multipath_paths == Some(1),
    })
}

/// Translate the bound socket address into dialable direct candidates.
///
/// An unspecified bind (`0.0.0.0`) is not dialable — advertise the
/// loopback address plus the kernel's egress source address for a public
/// destination instead. The egress hint uses UDP `connect()` route
/// lookup, which sends no packets; TEST-NET-1 is documentation space and
/// only selects the source interface. Full interface enumeration lands
/// with the candidate pipeline (WS1b).
fn advertised_addrs(local: SocketAddr) -> std::collections::BTreeSet<TransportAddr> {
    let mut out = std::collections::BTreeSet::new();
    let port = local.port();
    match local.ip() {
        ip if !ip.is_unspecified() => {
            out.insert(TransportAddr::Ip(local));
        }
        IpAddr::V4(_) => {
            out.insert(TransportAddr::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                port,
            )));
            if let Ok(sock) = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
                // Route lookup only — UDP connect emits no traffic.
                if sock.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).is_ok()
                    && let Ok(src) = sock.local_addr()
                    && !src.ip().is_unspecified()
                {
                    out.insert(TransportAddr::Ip(SocketAddr::new(src.ip(), port)));
                }
            }
        }
        IpAddr::V6(_) => {
            out.insert(TransportAddr::Ip(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                port,
            )));
        }
    }
    out
}

/// An owned-transport endpoint: our socket, our TLS, our path policy.
#[derive(Clone)]
pub struct Endpoint {
    inner: noq::Endpoint,
    id: EndpointId,
    local_addr: SocketAddr,
    local_addrs: Vec<SocketAddr>,
    client_configs: Arc<std::collections::BTreeMap<Vec<u8>, noq::ClientConfig>>,
    /// Relay tunnel handle when `relay_endpoint` was configured —
    /// steers synthetic-address sends and advertises the relay url.
    relay: Option<relay::RelayHandle>,
    /// Endpoint metrics — the connection driver records QNT progress
    /// here; the facade surfaces it via `Endpoint::metrics`.
    metrics: crate::metrics::Registry,
    drivers: Arc<drivers::Drivers>,
    transport_health: Option<socket::Health>,
    /// Peer-path kinds this endpoint may use (`EndpointConfig::transports`).
    transports: crate::Transports,
    /// Single-path pinning (`max_multipath_paths == Some(1)`): nothing
    /// beyond the established path can open, so in-band candidate
    /// exchange is suppressed rather than sprayed pointlessly.
    pinned: bool,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("id", &self.id)
            .field("local_addr", &self.local_addr)
            .finish()
    }
}

impl Endpoint {
    /// This endpoint's metrics registry (QNT counters are fed by the
    /// per-connection drivers).
    pub fn metrics(&self) -> crate::metrics::Registry {
        self.metrics.clone()
    }

    /// Policy tasks still running, including their final cleanup. Completed
    /// tasks release storage immediately; endpoint close waits for zero.
    pub fn active_path_drivers(&self) -> usize {
        self.drivers.len()
    }

    /// This endpoint's public identity.
    pub fn id(&self) -> EndpointId {
        self.id
    }

    /// The primary bound UDP address of this endpoint.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// All bound UDP addresses — one per muxed transport.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.local_addrs
    }

    /// Local child I/O failures for mux-backed endpoints. None for an opaque
    /// injected socket; this metadata does not establish remote reachability.
    pub fn transport_health(&self) -> Option<Vec<socket::ChildHealth>> {
        self.transport_health.as_ref().map(socket::Health::snapshot)
    }

    /// Advertised address: our identity plus the direct IP candidates we
    /// know about, plus the home relay when one is attached. Observed
    /// external addresses join this set once the candidate pipeline
    /// tracks them. A relay-only endpoint advertises no direct addrs —
    /// `addr()` is the advertised surface, not the bound-socket list.
    pub fn addr(&self) -> EndpointAddr {
        let mut addrs = std::collections::BTreeSet::new();
        if self.transports != crate::Transports::RelayOnly {
            for local in &self.local_addrs {
                if !relay::is_synthetic(*local)
                    && self
                        .transport_health
                        .as_ref()
                        .is_none_or(|health| health.bound_available(*local))
                {
                    addrs.extend(advertised_addrs(*local));
                }
            }
        }
        if let Some(handle) = &self.relay
            && handle.is_available()
            && self
                .transport_health
                .as_ref()
                .is_none_or(|health| health.path_available(relay::synthetic_for(&self.id), None))
        {
            addrs.insert(TransportAddr::Relay(handle.url.clone()));
        }
        EndpointAddr { id: self.id, addrs }
    }

    /// Connect to a peer by advertised address.
    ///
    /// Race up to eight direct candidates plus the attached relay with a
    /// handshake deadline. Only the first authenticated success is retained;
    /// additional addresses then become paths on that same connection.
    /// `alpn` must be one of the protocol ids configured at bind time.
    pub async fn connect(&self, target: EndpointAddr, alpn: &[u8]) -> anyhow::Result<Connection> {
        let Some(client_config) = self.client_configs.get(alpn) else {
            bail!("alpn {alpn:?} not configured on this endpoint");
        };
        let remote_id = target.id;
        anyhow::ensure!(
            self.transport_health
                .as_ref()
                .is_none_or(|health| !health.all_failed()),
            "all endpoint transports failed"
        );
        let local = self
            .transport_health
            .as_ref()
            .map(socket::Health::live_addrs)
            .unwrap_or_else(|| self.local_addrs.clone());
        // A relay-only endpoint never dials direct candidates — even if a
        // ticket lists them, the bound transports are the contract.
        let candidates = if self.transports == crate::Transports::RelayOnly {
            Vec::new()
        } else {
            policy::dial_candidates(&target, &local)
        };
        // A relayed path is usable when the peer's advertised relay is
        // the one we are attached to; it becomes the synthetic remote.
        // Reserve before dialing; cancellation releases to bounded grace.
        // Direct-only tickets also need a lease for later learned relay paths.
        let (peer_lease, relay_error) = match self
            .relay
            .as_ref()
            .map(|handle| handle.register_peer(remote_id))
            .transpose()
        {
            Ok(lease) => (lease, None),
            Err(error) => {
                tracing::debug!(%remote_id, %error, "relay peer registration refused; direct candidates remain usable");
                (None, Some(error))
            }
        };
        let relay_remote = self.relay_remote(&target).filter(|_| peer_lease.is_some());
        let mut attempts = candidates.clone();
        if let Some(relay) = relay_remote
            && !attempts.contains(&relay)
        {
            attempts.push(relay);
        }
        if attempts.is_empty() {
            if let Some(error) = relay_error {
                return Err(error).context("relay-only dial could not reserve the peer route");
            }
            bail!("no reachable addresses for {remote_id}");
        }

        let server_name = tls::name::encode(remote_id);
        let conn = dial::race(&self.inner, client_config, &attempts, &server_name)
            .await
            .context("all connection candidates failed")?;

        // Subscribe before any additional path can finish validation. Only
        // the completed handshake is initially eligible for path selection.
        let telemetry = self.wire_connection(&conn, attempts, peer_lease)?;

        Ok(Connection {
            inner: conn,
            remote_id,
            telemetry,
        })
    }

    /// Accept the next incoming connection attempt.
    pub fn accept(&self) -> impl Future<Output = Option<Incoming>> + '_ {
        let accept = self.inner.accept();
        let mut our_addrs = self.advertised_socket_addrs();
        // The synthetic relay address is worth advertising only when the
        // peer could actually open a second path to it.
        if !self.pinned
            && self
                .relay
                .as_ref()
                .is_some_and(relay::RelayHandle::is_available)
        {
            our_addrs.push(relay::synthetic_for(&self.id));
        }
        let relay = self.relay.clone();
        let metrics = self.metrics.clone();
        let drivers = self.drivers.clone();
        let allow_direct = self.transports != crate::Transports::RelayOnly;
        let pinned = self.pinned;
        async move {
            accept.await.map(|i| {
                Incoming::with_drivers(i, our_addrs, relay, metrics, drivers, allow_direct, pinned)
            })
        }
    }

    /// The peer's synthetic relay remote when it advertises the relay
    /// we are attached to.
    fn relay_remote(&self, target: &EndpointAddr) -> Option<SocketAddr> {
        let handle = self.relay.as_ref()?;
        if !handle.is_available()
            || self.transport_health.as_ref().is_some_and(|health| {
                !health.path_available(relay::synthetic_for(&target.id), None)
            })
        {
            return None;
        }
        target.addrs.iter().find_map(|a| match a {
            TransportAddr::Relay(url) => relay::parse_relay_url(url)
                .filter(|(rid, _)| *rid == handle.relay_id)
                .map(|_| relay::synthetic_for(&target.id)),
            _ => None,
        })
    }

    /// Direct addresses we can dial from, resolved per bound socket.
    /// Synthetic relay-mapped locals are never real candidates; a
    /// relay-only endpoint advertises no direct addresses, and a
    /// single-path endpoint has no use for in-band exchange at all.
    fn advertised_socket_addrs(&self) -> Vec<SocketAddr> {
        if self.transports == crate::Transports::RelayOnly || self.pinned {
            return Vec::new();
        }
        self.local_addrs
            .iter()
            .filter(|l| {
                !relay::is_synthetic(**l)
                    && self
                        .transport_health
                        .as_ref()
                        .is_none_or(|health| health.bound_available(**l))
            })
            .flat_map(|l| advertised_addrs(*l))
            .filter_map(|a| match a {
                TransportAddr::Ip(sock) => Some(sock),
                _ => None,
            })
            .collect()
    }

    /// Common per-connection wiring: advertise our direct addresses to
    /// the peer (plus our synthetic relay address when attached — the
    /// peer can then upgrade to a relayed path in-band), kick a
    /// traversal round so it probes ours, and spawn the driver that
    /// opens paths to its in-band advertised candidates and keeps the
    /// best validated path selected. Subscribe before QNT or extra path opens;
    /// only the authenticated handshake path is seeded.
    fn wire_connection(
        &self,
        conn: &noq::Connection,
        candidates: Vec<SocketAddr>,
        peer_lease: Option<relay::PeerLease>,
    ) -> anyhow::Result<Arc<telemetry::Telemetry>> {
        let mut ours = self.advertised_socket_addrs();
        if !self.pinned
            && peer_lease.is_some()
            && self
                .relay
                .as_ref()
                .is_some_and(relay::RelayHandle::is_available)
        {
            ours.push(relay::synthetic_for(&self.id));
        }
        let allow_direct = self.transports != crate::Transports::RelayOnly;
        let telemetry = self.drivers.spawn(
            conn,
            self.metrics.clone(),
            ours.clone(),
            candidates,
            peer_lease,
            self.relay.clone(),
            allow_direct,
        )?;
        if !self.pinned {
            policy::advertise_addrs(conn, &ours);
        }
        if allow_direct && !self.pinned {
            // Relay-only and pinned endpoints keep the traversal channel
            // closed: there is nothing extra to offer or to learn — the
            // connection stays on its established path.
            policy::initiate_traversal_round(conn, &self.metrics);
        }
        Ok(telemetry)
    }

    /// Close all connections and wait for this endpoint's policy tasks.
    /// Relay tunnel pumps and QUIC packet draining have separate lifecycles.
    pub async fn close(&self) {
        self.drivers.close_admission();
        self.inner.close(0u32.into(), b"closed");
        self.drivers.wait().await;
    }
}

/// An inbound connection attempt. Awaiting it completes the handshake
/// and yields the peer-verified [`Connection`].
pub struct Incoming {
    incoming: Option<noq::Incoming>,
    connecting: Option<noq::Connecting>,
    /// Direct addresses to advertise to this peer once connected.
    our_addrs: Vec<SocketAddr>,
    /// Relay tunnel handle for synthetic-address peer registration.
    relay: Option<relay::RelayHandle>,
    /// Endpoint metrics — the driver records QNT progress here.
    metrics: crate::metrics::Registry,
    drivers: Arc<drivers::Drivers>,
    /// `EndpointConfig::transports != RelayOnly` — whether the connection
    /// policy may turn peer-advertised direct addrs into paths.
    allow_direct: bool,
    /// Single-path endpoint: no in-band advertisement, no traversal.
    pinned: bool,
}

impl Incoming {
    /// Wrap a raw incoming attempt.
    pub fn new(
        incoming: noq::Incoming,
        our_addrs: Vec<SocketAddr>,
        relay: Option<relay::RelayHandle>,
        metrics: crate::metrics::Registry,
    ) -> Self {
        Self::with_drivers(
            incoming,
            our_addrs,
            relay,
            metrics,
            Arc::new(drivers::Drivers::default()),
            true,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_drivers(
        incoming: noq::Incoming,
        our_addrs: Vec<SocketAddr>,
        relay: Option<relay::RelayHandle>,
        metrics: crate::metrics::Registry,
        drivers: Arc<drivers::Drivers>,
        allow_direct: bool,
        pinned: bool,
    ) -> Self {
        Self {
            incoming: Some(incoming),
            connecting: None,
            our_addrs,
            relay,
            metrics,
            drivers,
            allow_direct,
            pinned,
        }
    }

    /// Address the attempt arrived from.
    pub fn remote_address(&self) -> SocketAddr {
        match (&self.incoming, &self.connecting) {
            (Some(i), _) => i.remote_address(),
            (None, Some(c)) => c.remote_address(),
            (None, None) => SocketAddr::from(([0, 0, 0, 0], 0)),
        }
    }
}

impl Future for Incoming {
    type Output = anyhow::Result<Connection>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            if let Some(connecting) = &mut self.connecting {
                let res = ready!(Pin::new(connecting).poll(cx));
                return Poll::Ready(match res {
                    Err(e) => Err(e.into()),
                    Ok(inner) => match peer_endpoint_id(&inner) {
                        Some(remote_id) => {
                            let registration = self
                                .relay
                                .as_ref()
                                .map(|handle| handle.register_peer(remote_id))
                                .transpose();
                            let lease = match registration {
                                Ok(lease) => lease,
                                Err(error) => {
                                    tracing::debug!(%remote_id, %error, "incoming relay registration refused");
                                    let arrived_over_relay = inner
                                        .path(noq::PathId::ZERO)
                                        .and_then(|path| path.remote_address().ok())
                                        .is_some_and(relay::is_synthetic);
                                    if arrived_over_relay {
                                        inner.close(0u32.into(), b"relay peer route unavailable");
                                        return Poll::Ready(Err(error.into()));
                                    }
                                    None
                                }
                            };
                            let mut ours = self.our_addrs.clone();
                            if lease.is_none()
                                || self
                                    .relay
                                    .as_ref()
                                    .is_some_and(|handle| !handle.is_available())
                            {
                                ours.retain(|address| !relay::is_synthetic(*address));
                            }
                            self.drivers
                                .spawn(
                                    &inner,
                                    self.metrics.clone(),
                                    ours.clone(),
                                    Vec::new(),
                                    lease,
                                    self.relay.clone(),
                                    self.allow_direct,
                                )
                                .map(|telemetry| {
                                    if !self.pinned {
                                        policy::advertise_addrs(&inner, &ours);
                                    }
                                    if self.allow_direct && !self.pinned {
                                        policy::initiate_traversal_round(&inner, &self.metrics);
                                    }
                                    Connection {
                                        inner,
                                        remote_id,
                                        telemetry,
                                    }
                                })
                        }
                        None => Err(anyhow::anyhow!("peer presented no identity")),
                    },
                });
            }
            match self.incoming.take() {
                Some(incoming) => match incoming.accept() {
                    Ok(connecting) => self.connecting = Some(connecting),
                    Err(e) => return Poll::Ready(Err(e.into())),
                },
                None => return Poll::Ready(Err(anyhow::anyhow!("incoming consumed"))),
            }
        }
    }
}

/// A QUIC connection to a peer, owned backend.
///
/// API-compatible with the surface `rds-agent`/`rds-cli` use on the iroh
/// backend: stream open/accept, datagrams, `remote_id`, `close`.
#[derive(Clone)]
pub struct Connection {
    inner: noq::Connection,
    remote_id: EndpointId,
    telemetry: Arc<telemetry::Telemetry>,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("remote_id", &self.remote_id)
            .field("remote_address", &self.remote_address())
            .finish()
    }
}

impl Connection {
    pub(crate) fn observer(&self) -> ConnectionObserver {
        ConnectionObserver {
            inner: self.inner.weak_handle(),
            telemetry: self.telemetry.clone(),
        }
    }

    pub(crate) fn path_stats_snapshot(&self) -> crate::PathStatsSnapshot {
        self.telemetry.snapshot(self.inner.close_reason().is_some())
    }

    /// Verified peer identity (from the TLS raw public key).
    pub fn remote_id(&self) -> EndpointId {
        self.remote_id
    }

    /// Remote socket address on the initial path, if it still exists.
    pub fn remote_address(&self) -> Option<SocketAddr> {
        self.inner
            .path(noq::PathId::ZERO)
            .and_then(|p| p.remote_address().ok())
    }

    /// Open a bidirectional stream.
    pub fn open_bi(&self) -> noq::OpenBi<'_> {
        self.inner.open_bi()
    }

    /// Accept the next bidirectional stream opened by the peer.
    pub fn accept_bi(&self) -> noq::AcceptBi<'_> {
        self.inner.accept_bi()
    }

    /// Open a unidirectional stream.
    pub fn open_uni(&self) -> noq::OpenUni<'_> {
        self.inner.open_uni()
    }

    /// Accept the next unidirectional stream opened by the peer.
    pub fn accept_uni(&self) -> noq::AcceptUni<'_> {
        self.inner.accept_uni()
    }

    /// Send an unreliable datagram (QUIC DATAGRAM extension).
    pub fn send_datagram(&self, data: bytes::Bytes) -> Result<(), noq::SendDatagramError> {
        self.inner.send_datagram(data)
    }

    /// Receive the next unreliable datagram.
    pub fn read_datagram(&self) -> noq::ReadDatagram<'_> {
        self.inner.read_datagram()
    }

    /// RTT measured on a path, if it exists.
    pub fn rtt(&self, path: noq::PathId) -> Option<Duration> {
        self.inner.rtt(path)
    }

    /// Stream of path lifecycle events (established/abandoned/observed).
    pub fn path_events(&self) -> noq::PathEvents {
        self.inner.path_events()
    }

    /// Our external address as observed by the peer (QAD).
    pub fn observed_external_addr(&self) -> noq::ObservedExternalAddr {
        self.inner.observed_external_addr()
    }

    /// Close the connection.
    pub fn close(&self, error_code: noq::VarInt, reason: &[u8]) {
        self.inner.close(error_code, reason);
    }

    /// Raw noq connection for the path-policy driver.
    pub fn inner(&self) -> &noq::Connection {
        &self.inner
    }
}

/// Transport observation without a facade or a strong connection handle.
pub(crate) struct ConnectionObserver {
    inner: noq::WeakConnectionHandle,
    telemetry: Arc<telemetry::Telemetry>,
}

impl ConnectionObserver {
    pub fn snapshot(&self) -> crate::PathStatsSnapshot {
        let closed = self
            .inner
            .upgrade()
            .is_none_or(|conn| conn.close_reason().is_some());
        self.telemetry.snapshot(closed)
    }

    pub fn closed(&self) -> impl Future<Output = ()> + Send + 'static {
        let registered = self.inner.upgrade().map(|conn| conn.on_closed());
        async move {
            if let Some(closed) = registered {
                closed.await;
            }
        }
    }
}
