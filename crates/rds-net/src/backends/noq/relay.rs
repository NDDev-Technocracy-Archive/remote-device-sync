//! Relay transport: an [`AsyncUdpSocket`] that tunnels datagrams through
//! the owned relay protocol (`rds-relay/0`), plugged into the socket
//! mux as just another child.
//!
//! QUIC paths are 4-tuples of `SocketAddr`s, so relayed remotes get a
//! **synthetic address**: an IPv4 in the RFC 2544 benchmark range
//! `198.19.0.0/16` derived from the peer's `EndpointId`. The address is
//! never routed — a transmit to it only ever means "via the relay
//! tunnel", and a received `RecvMeta` with it only ever means "arrived
//! via the relay". IPv4 specifically because noq rejects IPv6 remotes
//! on IPv4-bound endpoints. RDS reserves this range for relay routing; it
//! cannot simultaneously address real network hosts in this range.
//!
//! Ownership: the socket keeps a helper endpoint (same secret key →
//! same `EndpointId`) connected to the relay server. The relay sees
//! the endpoint's real identity; datagrams are forwarded as
//! `[src_key][payload]`.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

use crate::relay_control::{read_control, write_control};
use iroh::{EndpointAddr, EndpointId, RelayUrl, SecretKey};
use noq::udp::{RecvMeta, Transmit};
use noq::{AsyncUdpSocket, Runtime, UdpSender};
use rds_core::relay::{self, RelayControl};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::Connection;
mod peers;
use peers::PeerRegistry;
pub use peers::{PeerLease, PeerRegistrationError};

type Datagram = (bytes::Bytes, SocketAddr);

// A shared watch does not accumulate one-shot subscribers across connection
// churn on a long-lived tunnel. This guard also signals on pump cancellation.
struct LinkLifetime(watch::Sender<bool>);
impl Drop for LinkLifetime {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
}

#[derive(Default)]
struct DropCounters {
    queue_full: AtomicU64,
    peer_rejected: AtomicU64,
    oversized: AtomicU64,
}

/// Per-tunnel occupancy and cumulative loss diagnostics. Fields are individual
/// concurrent snapshots, not a transaction across transport and peer state.
#[derive(Debug, Clone, Copy)]
pub struct RelaySocketStats {
    pub queue_capacity: usize,
    pub queued_datagrams: usize,
    pub peer_entries: usize,
    pub pinned_peers: usize,
    pub dropped_queue_full: u64,
    pub rejected_peer_frames: u64,
    pub dropped_oversized: u64,
}

/// First two octets marking a synthetic relay-mapped address.
const SYNTHETIC_PREFIX: [u8; 2] = [198, 19];

/// Whether `ip` is a synthetic relay-mapped address.
pub fn is_synthetic_ip(ip: IpAddr) -> bool {
    matches!(ip.to_canonical(), IpAddr::V4(v4) if v4.octets()[0..2] == SYNTHETIC_PREFIX)
}

/// Whether `addr` is a synthetic relay-mapped address.
pub fn is_synthetic(addr: SocketAddr) -> bool {
    is_synthetic_ip(addr.ip())
}

/// The synthetic remote address representing `peer` over the relay.
///
/// Deterministic: every endpoint computes the same address for a given peer.
/// BLAKE3 fills 32 free bits (16 host bits and 16 port bits). This is not an
/// identity: colliding peer registrations are refused while an existing
/// owner is pinned or within its inactivity grace. Hash port zero maps to one:
/// QUIC rejects a zero remote port. All previously nonzero mappings stay stable.
pub fn synthetic_for(id: &EndpointId) -> SocketAddr {
    let h = blake3::hash(id.as_bytes());
    let b = h.as_bytes();
    SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(198, 19, b[0], b[1])),
        u16::from_be_bytes([b[2], b[3]]).max(1),
    )
}

/// Encode the relay's endpoint address as the `TransportAddr::Relay`
/// payload: `rds-relay://<id>@<ip>:<port>`.
pub fn relay_url_for(relay: &EndpointAddr) -> Option<RelayUrl> {
    let sock = relay.addrs.iter().find_map(|a| match a {
        iroh::TransportAddr::Ip(s) => Some(*s),
        _ => None,
    })?;
    rds_discovery::OwnedRelayRoute {
        key: rds_discovery::EndpointKey(*relay.id.as_bytes()),
        addr: sock,
    }
    .to_string()
    .parse()
    .ok()
}

/// Parse an `rds-relay://` url back into `(relay_id, socket_addr)`.
pub fn parse_relay_url(url: &RelayUrl) -> Option<(EndpointId, SocketAddr)> {
    let route: rds_discovery::OwnedRelayRoute = url.as_str().parse().ok()?;
    Some((EndpointId::from_bytes(&route.key.0).ok()?, route.addr))
}

/// Shared handle the endpoint keeps to steer the relay socket inside
/// the mux.
#[derive(Clone)]
pub struct RelayHandle {
    /// The relay server's endpoint id — matches peers' advertised
    /// `TransportAddr::Relay` urls.
    pub relay_id: EndpointId,
    /// The relay server's socket address — what we dial.
    pub relay_sock: SocketAddr,
    /// The `rds-relay://` url we advertise as `TransportAddr::Relay`.
    pub url: RelayUrl,
    /// synthetic → EndpointId, filled on connect/accept/receive so
    /// `poll_send` can decode transmit destinations.
    peers: Arc<PeerRegistry>,
    /// Set when drain is observed; the existing tunnel stays usable through grace.
    drained: Arc<AtomicBool>,
    /// Observability must not keep the socket's helper connection alive.
    connection: noq::WeakConnectionHandle,
    available: watch::Receiver<bool>,
    queue: mpsc::WeakSender<Datagram>,
    queue_capacity: usize,
    drops: Arc<DropCounters>,
}

impl RelayHandle {
    /// Pin an identity's route. Retain the returned lease while I/O needs it.
    /// A collision or a table full of pinned peers is refused without replacing
    /// another identity. Local registration does not prove peer reachability.
    pub fn register_peer(&self, id: EndpointId) -> Result<PeerLease, PeerRegistrationError> {
        self.peers.acquire(id)
    }

    pub fn stats(&self) -> RelaySocketStats {
        let (peer_entries, pinned_peers) = self.peers.occupancy();
        let queued_datagrams = self
            .queue
            .upgrade()
            .map(|queue| queue.max_capacity() - queue.capacity())
            .unwrap_or(0);
        RelaySocketStats {
            queue_capacity: self.queue_capacity,
            queued_datagrams,
            peer_entries,
            pinned_peers,
            dropped_queue_full: self.drops.queue_full.load(Ordering::Relaxed),
            rejected_peer_frames: self.drops.peer_rejected.load(Ordering::Relaxed),
            dropped_oversized: self.drops.oversized.load(Ordering::Relaxed),
        }
    }

    /// Whether the local authenticated tunnel is open and supports datagrams.
    /// This does not promise that any particular peer is attached or reachable.
    /// Drain remains available during its usable grace period.
    pub fn is_available(&self) -> bool {
        *self.available.borrow()
            && self.connection.upgrade().is_some_and(|conn| {
                conn.close_reason().is_none() && conn.max_datagram_size().is_some()
            })
    }

    pub(super) async fn unavailable(&mut self) {
        let _ = self.available.wait_for(|available| !*available).await;
    }

    /// Whether the relay announced it is draining.
    pub fn drained(&self) -> bool {
        self.drained.load(Ordering::Relaxed)
    }
}

/// A mux child that forwards datagrams through the relay connection.
pub struct RelaySocket {
    /// Helper endpoint holding the relay connection; same key → same id.
    _endpoint: super::Endpoint,
    conn: Connection,
    local: SocketAddr,
    peers: Arc<PeerRegistry>,
    rx: mpsc::Receiver<Datagram>,
    // A UDP socket has no receive EOF while it exists. Owning the channel also
    // permits exact bounded occupancy queries via a weak sender after pump exit.
    _queue_owner: mpsc::Sender<Datagram>,
    drops: Arc<DropCounters>,
    available: watch::Sender<bool>,
    tasks: Option<(JoinHandle<()>, JoinHandle<()>)>,
}

impl RelaySocket {
    /// Attach to `relay` with endpoint key `key`.
    ///
    /// The helper endpoint binds a plain UDP socket (no relay recursion)
    /// and dials the relay on [`relay::RELAY_ALPN`]; registration
    /// completes before the socket reports ready.
    pub async fn connect(
        relay: EndpointAddr,
        key: SecretKey,
        bind: SocketAddr,
    ) -> anyhow::Result<(Self, RelayHandle)> {
        Self::connect_with_limits(relay, key, bind, crate::RelayLimits::default()).await
    }

    /// Attach with explicit positive queue, peer and retirement limits.
    pub async fn connect_with_limits(
        relay: EndpointAddr,
        key: SecretKey,
        bind: SocketAddr,
        limits: crate::RelayLimits,
    ) -> anyhow::Result<(Self, RelayHandle)> {
        let relay_id = relay.id;
        let relay_sock = relay
            .addrs
            .iter()
            .find_map(|a| match a {
                iroh::TransportAddr::Ip(s) => Some(*s),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("relay address has no IP candidate"))?;

        // Helper endpoint on a plain UDP socket — no mux, no relay
        // recursion. Same key → same EndpointId, so the relay maps this
        // connection to our real identity. The attachment is pinned to
        // the single dialed path: the configured bootstrap address is
        // the contract, so no in-band candidate exchange may move the
        // tunnel onto an unadvertised (unimpaired) address.
        let our_id = key.public();
        let runtime = Arc::new(noq::TokioRuntime);
        let udp = std::net::UdpSocket::bind(bind)?;
        let local = udp.local_addr()?;
        let socket = runtime.wrap_udp_socket(udp)?;
        let endpoint = super::bind_with_socket(
            crate::EndpointConfig {
                secret_key: Some(key),
                alpns: vec![relay::RELAY_ALPN.to_vec()],
                max_multipath_paths: Some(1),
                ..Default::default()
            },
            socket,
            vec![local],
            runtime,
            None,
        )
        .await?;
        // One deadline covers dial, stream credit, register write and reply.
        let (conn, ctrl_send, mut ctrl_recv) =
            tokio::time::timeout(Duration::from_secs(15), async {
                let conn = endpoint.connect(relay.clone(), relay::RELAY_ALPN).await?;
                let (mut send, mut recv) = conn.open_bi().await?;
                write_control(&mut send, &RelayControl::Register).await?;
                if !matches!(read_control(&mut recv).await?, RelayControl::Registered) {
                    anyhow::bail!("relay registration rejected");
                }
                Ok::<_, anyhow::Error>((conn, send, recv))
            })
            .await
            .map_err(|_| anyhow::anyhow!("relay registration timed out"))??;

        let peers = PeerRegistry::new(
            usize::from(limits.max_peers.get()),
            Duration::from_secs(u64::from(limits.peer_grace_secs.get())),
        );
        let drained = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(DropCounters::default());
        let queue_capacity = usize::from(limits.datagram_queue.get());
        let (tx, rx) = mpsc::channel(queue_capacity);
        let queue_owner = tx.clone();
        let (available, availability) = watch::channel(conn.inner().max_datagram_size().is_some());

        // Datagram pump: relay → synthetic-addressed receives.
        let dgram_pump = tokio::spawn({
            let conn = conn.clone();
            let peers = peers.clone();
            let drops = drops.clone();
            let lifetime = LinkLifetime(available.clone());
            async move {
                let _lifetime = lifetime;
                let mut burst = 0usize;
                loop {
                    if burst == 64 {
                        tokio::task::yield_now().await;
                        burst = 0;
                    }
                    burst += 1;
                    match conn.read_datagram().await {
                        Ok(frame) => {
                            tracing::trace!(len = frame.len(), "relay recv datagram");
                            let Some((src, payload)) = relay::decode_frame(&frame) else {
                                continue;
                            };
                            let Ok(src) = EndpointId::from_bytes(&src) else {
                                continue;
                            };
                            let syn = synthetic_for(&src);
                            if peers.observe(src).is_err() {
                                drops.peer_rejected.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            let payload_start = frame.len() - payload.len();
                            match tx.try_send((frame.slice(payload_start..), syn)) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    drops.queue_full.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => return,
                            }
                        }
                        Err(_) => return,
                    }
                }
            }
        });

        // Control reader: Drain/PeerGone/liveness replies.
        let ctrl_pump = tokio::spawn({
            let drained = drained.clone();
            let conn = conn.clone();
            let lifetime = LinkLifetime(available.clone());
            async move {
                let _lifetime = lifetime;
                let mut ctrl_send = ctrl_send;
                loop {
                    match read_control(&mut ctrl_recv).await {
                        Ok(RelayControl::Drain) => {
                            debug!("relay draining; existing tunnel remains usable during grace");
                            drained.store(true, Ordering::SeqCst);
                        }
                        Ok(RelayControl::PeerGone { peer }) => {
                            debug!(peer = %data_encoding::HEXLOWER.encode(&peer[..8]), "relay peer gone");
                        }
                        Ok(RelayControl::Ping { seq }) => {
                            if !matches!(
                                tokio::time::timeout(
                                    Duration::from_secs(1),
                                    write_control(&mut ctrl_send, &RelayControl::Pong { seq })
                                )
                                .await,
                                Ok(Ok(()))
                            ) {
                                break;
                            }
                        }
                        Ok(RelayControl::Pong { .. } | RelayControl::Health { .. }) => {}
                        _ => break,
                    }
                }
                // Losing/malforming the control stream invalidates this tunnel.
                conn.close(0u32.into(), b"relay control ended");
            }
        });

        // Our own synthetic address — what this endpoint advertises and
        // what reply traffic from peers resolves against.
        let local = synthetic_for(&our_id);
        let handle = RelayHandle {
            relay_id,
            relay_sock,
            url: relay_url_for(&relay)
                .ok_or_else(|| anyhow::anyhow!("relay addr has no IP candidate"))?,
            peers: peers.clone(),
            drained: drained.clone(),
            connection: conn.inner().weak_handle(),
            available: availability,
            queue: queue_owner.downgrade(),
            queue_capacity,
            drops: drops.clone(),
        };
        Ok((
            Self {
                _endpoint: endpoint,
                conn,
                local,
                peers,
                rx,
                _queue_owner: queue_owner,
                drops,
                available,
                tasks: Some((dgram_pump, ctrl_pump)),
            },
            handle,
        ))
    }
}

impl RelaySocket {
    /// Close the tunnel and join its pumps. Socket destruction is the fallback:
    /// it closes transport and requests abort, but cannot join from Drop.
    pub async fn close(&mut self) {
        self.available.send_replace(false);
        self.conn.close(0u32.into(), b"relay socket closed");
        if let Some((datagrams, control)) = self.tasks.take() {
            datagrams.abort();
            control.abort();
            let _ = tokio::join!(datagrams, control);
        }
        self._endpoint.close().await;
    }
}

impl Drop for RelaySocket {
    fn drop(&mut self) {
        self.available.send_replace(false);
        self.conn.close(0u32.into(), b"relay socket dropped");
        if let Some((datagrams, control)) = &self.tasks {
            datagrams.abort();
            control.abort();
        }
    }
}

impl fmt::Debug for RelaySocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelaySocket")
            .field("local", &self.local)
            .finish()
    }
}

impl AsyncUdpSocket for RelaySocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(RelaySender {
            conn: self.conn.clone(),
            peers: self.peers.clone(),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        if bufs.is_empty() || meta.is_empty() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "relay receive needs a buffer and metadata slot",
            )));
        }
        for _ in 0..8 {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some((data, src))) => {
                    if data.len() > bufs[0].len() {
                        self.drops.oversized.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let n = data.len();
                    bufs[0][..n].copy_from_slice(&data);
                    let mut m = RecvMeta::default();
                    m.addr = src;
                    m.len = n;
                    m.stride = n;
                    m.dst_ip = Some(self.local.ip());
                    meta[0] = m;
                    return Poll::Ready(Ok(1));
                }
                // Link loss is not an I/O error of the logical mux socket.
                Poll::Ready(None) | Poll::Pending => return Poll::Pending,
            }
        }
        // Bound work when a caller supplies a small buffer to a queued burst.
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

/// Sends outer-QUIC packets into the relay tunnel.
struct RelaySender {
    conn: Connection,
    peers: Arc<PeerRegistry>,
}

impl fmt::Debug for RelaySender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelaySender").finish()
    }
}

impl UdpSender for RelaySender {
    fn poll_send(
        self: Pin<&mut Self>,
        transmit: &Transmit<'_>,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let dst = transmit.destination;
        tracing::trace!(%dst, len = transmit.contents.len(), "relay socket send");
        if !is_synthetic(dst) {
            warn!(%dst, "relay socket got non-relay transmit");
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("relay socket got non-relay transmit to {dst}"),
            )));
        }
        let Some(id) = self.peers.get(dst) else {
            // An unknown destination is loss on this candidate. It must not
            // terminate the QUIC connection driver serving healthy direct paths.
            tracing::debug!(%dst, "relay drop: peer mapping unavailable");
            return Poll::Ready(Ok(()));
        };
        let frame = relay::encode_forward(id.as_bytes(), transmit.contents);
        match self.conn.send_datagram(frame) {
            Ok(()) => Poll::Ready(Ok(())),
            // The tunnel is a link with a smaller MTU than the direct
            // path: drop oversized transmits instead of erroring, and
            // path-MTU discovery converges below the ceiling — exactly
            // what a real link does to packets it cannot carry.
            Err(noq::SendDatagramError::TooLarge) => {
                tracing::trace!(%dst, len = transmit.contents.len(), "relay drop: exceeds tunnel MTU");
                Poll::Ready(Ok(()))
            }
            Err(noq::SendDatagramError::ConnectionLost(error)) => {
                // This child link is down; the logical socket and other paths
                // remain usable. QUIC observes loss and times out this path.
                tracing::debug!(%dst, %error, "relay drop: tunnel closed");
                Poll::Ready(Ok(()))
            }
            Err(noq::SendDatagramError::UnsupportedByPeer | noq::SendDatagramError::Disabled) => {
                tracing::debug!(%dst, "relay drop: tunnel datagrams unavailable");
                Poll::Ready(Ok(()))
            }
        }
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}
