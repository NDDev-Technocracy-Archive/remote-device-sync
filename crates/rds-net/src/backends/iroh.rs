//! Endpoint lifecycle for rds: iroh endpoint construction, persistent
//! identity, ticket encoding, connection acceptance.
//!
//! The network identity of an rds peer is the iroh `EndpointId`, an Ed25519
//! public key. QUIC-TLS authentication is derived from it, so a connection
//! is always pinned to a key rather than an IP address.

use std::str::FromStr;

use iroh::{Endpoint, RelayMap, RelayMode};

use crate::{EndpointAddr, EndpointConfig, EndpointId, RelayUrl, TransportAddr};
mod latency;
#[cfg(all(test, feature = "transport-noq"))]
mod latency_tests;

/// Adapter conversions between the owned shared types and iroh-base.
///
/// `rds-core` owns the wire-facing identity/address types so service
/// crates never name a backend type; this module is the only place
/// that translates. String and postcard encodings are byte-identical
/// on both sides, so tickets and records stay interchangeable. Public
/// but doc-hidden: external adapters and interop tests convert here
/// too instead of re-deriving the mapping.
#[doc(hidden)]
pub mod convert {
    use crate::{EndpointAddr, EndpointId, RelayUrl, SecretKey, TransportAddr};

    pub fn id(id: EndpointId) -> iroh::EndpointId {
        iroh::EndpointId::try_from(id.as_bytes()).expect("endpoint ids are validated")
    }

    pub fn id_from(id: iroh::EndpointId) -> EndpointId {
        EndpointId::from_bytes(id.as_bytes()).expect("iroh ids are validated")
    }

    pub fn key(key: &SecretKey) -> iroh::SecretKey {
        iroh::SecretKey::from(&key.to_bytes())
    }

    pub fn relay(url: &RelayUrl) -> iroh::RelayUrl {
        iroh::RelayUrl::from(url.as_url().clone())
    }

    pub fn relay_from(url: iroh::RelayUrl) -> RelayUrl {
        url.to_string()
            .parse()
            .expect("iroh relay urls are normalized")
    }

    pub fn transport(addr: &TransportAddr) -> iroh::TransportAddr {
        match addr {
            TransportAddr::Relay(url) => iroh::TransportAddr::Relay(relay(url)),
            TransportAddr::Ip(sock) => iroh::TransportAddr::Ip(*sock),
            other => unreachable!("TransportAddr variant added upstream: {other}"),
        }
    }

    pub fn transport_from(addr: iroh::TransportAddr) -> TransportAddr {
        match addr {
            iroh::TransportAddr::Relay(url) => TransportAddr::Relay(relay_from(url)),
            iroh::TransportAddr::Ip(sock) => TransportAddr::Ip(sock),
            other => unreachable!("TransportAddr variant added upstream: {other}"),
        }
    }

    pub fn addr(addr: &EndpointAddr) -> iroh::EndpointAddr {
        iroh::EndpointAddr::from_parts(id(addr.id), addr.addrs.iter().map(transport))
    }

    pub fn addr_from(addr: iroh::EndpointAddr) -> EndpointAddr {
        EndpointAddr::from_parts(id_from(addr.id), addr.addrs.into_iter().map(transport_from))
    }
}

/// Fold iroh's (quinn-shaped) `ConnectionError` into the owned kind.
pub(crate) fn close_kind(error: iroh::endpoint::ConnectionError) -> crate::CloseKind {
    use crate::CloseKind;
    use iroh::endpoint::ConnectionError as E;
    match error {
        E::LocallyClosed => CloseKind::Local,
        E::ApplicationClosed(_) => CloseKind::PeerApplication,
        E::ConnectionClosed(_) => CloseKind::PeerTransport,
        E::TimedOut => CloseKind::TimedOut,
        E::Reset => CloseKind::Reset,
        E::VersionMismatch | E::TransportError(_) | E::CidsExhausted => CloseKind::Transport,
    }
}

/// Configured relays without QUIC address discovery. The RDS relay serves HTTP only, so
/// discovery probes to it never answer; through a UDP tunnel that sends every datagram of a
/// socket to its first destination, they reach an unrelated peer instead.
fn owned_relay_map(relays: &[RelayUrl]) -> RelayMap {
    RelayMap::from_iter(
        relays
            .iter()
            .map(|url| iroh::RelayConfig::new(convert::relay(url), None)),
    )
}

/// Bind an rds endpoint: configured ALPNs, identity and relay mode.
///
/// With a custom relay the endpoint uses `presets::Minimal` — no n0 address
/// lookup — so a private deployment does not publish to third-party DNS.
/// Without one, `presets::N0` gives the public relays plus DNS/Pkarr lookup.
pub async fn bind_endpoint(config: EndpointConfig) -> anyhow::Result<Endpoint> {
    config.validate_for(crate::Backend::Iroh)?;
    let mut builder = match (config.relays.is_empty(), config.discovery) {
        (false, _) => Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(RelayMode::Custom(owned_relay_map(&config.relays))),
        (true, true) => Endpoint::builder(iroh::endpoint::presets::N0),
        // No relay, no lookup: Minimal binds a plain QUIC socket.
        (true, false) => Endpoint::builder(iroh::endpoint::presets::Minimal),
    };
    builder = builder.keep_relays_connected(config.keep_relays_connected);
    if let Some(key) = config.secret_key {
        builder = builder.secret_key(convert::key(&key));
    }
    builder = match config.transports {
        // Hard bound on reachable path kinds: iroh's in-band NAT
        // traversal (QNT) advertises direct addrs over any connection
        // upon which the peer can open unadvertised direct paths. Its
        // transport config cannot disable that exchange (floor of 8),
        // so the transport itself is removed — escape is then
        // impossible rather than merely unobserved.
        crate::Transports::All => builder,
        crate::Transports::DirectOnly => builder.clear_relay_transports(),
        crate::Transports::RelayOnly => builder.clear_ip_transports(),
    };
    if config.path_preference == crate::PathPreference::Latency {
        builder = if config.prefer_relay_order {
            builder.path_selector(std::sync::Arc::new(latency::OrderedRelaySelector {
                order: config.relays.iter().map(convert::relay).collect(),
            }))
        } else {
            builder.path_selector(std::sync::Arc::new(latency::LatencySelector))
        };
    }
    // Tuning on top of iroh's multipath-aware defaults:
    // - BBRv3 remains our default; explicit Cubic selection enables
    //   same-path qualification without changing priorities or windows.
    // - 4 MiB stream receive window: upstream tunes for ~100 Mbps x
    //   100 ms; a larger per-stream window keeps a big keyframe or sync
    //   chunk stream from stalling on high-BDP links.
    // - 32 MiB connection send window keeps several bulk streams busy.
    let controller = config.congestion_control.factory();
    let controller = if config.path_preference == crate::PathPreference::Latency {
        crate::ack_progress::factory(
            controller,
            std::sync::Arc::new(|| tokio::time::Instant::now().into_std()),
        )
    } else {
        controller
    };
    let mut transport = iroh::endpoint::QuicTransportConfig::builder()
        .congestion_controller_factory(controller)
        .prefer_same_path_acks(config.path_preference == crate::PathPreference::Latency)
        .stream_receive_window(noq_proto::VarInt::from_u32(4 * 1024 * 1024))
        .send_window(32 * 1024 * 1024);
    if config.packetization == crate::Packetization::Conservative {
        transport = transport
            .initial_mtu(1200)
            .min_mtu(1200)
            .mtu_discovery_config(None)
            .enable_segmentation_offload(false);
    }
    if let Some(max_paths) = config.max_multipath_paths {
        transport = transport.max_concurrent_multipath_paths(max_paths);
        if max_paths == 1 {
            // iroh floors the multipath cap at 14 and its QNT exchange
            // cannot be switched off, so a single-path endpoint still
            // migrates: the in-band advertisement lets the peer open a
            // path to any reachable addr and the RTT-biased default
            // selector moves traffic onto it — straight past an impaired
            // proxy. `PinnedSelector` selects the handshake path once
            // and keeps returning it, so later paths are never picked
            // for payload.
            builder = builder.path_selector(std::sync::Arc::new(PinnedSelector));
        }
    }
    if !config.observed_address_reports {
        transport = transport
            .send_observed_address_reports(false)
            .receive_observed_address_reports(false);
    }
    builder = builder.transport_config(transport.build());
    // iroh manages its own sockets; a single bind address is all it
    // accepts. Multi-interface binding is a `noq`-backend capability.
    if config.transports != crate::Transports::RelayOnly
        && let Some(addr) = config.bind_addrs.first()
    {
        // An explicit bind is the complete interface contract. Iroh's
        // builder otherwise keeps its unspecified socket in the other
        // family, advertising additional interfaces during bring-up.
        builder = builder.clear_ip_transports().bind_addr(*addr)?;
        if addr.ip().is_loopback() {
            // Port mapping of a loopback-only socket cannot make that socket
            // reachable on a gateway. It can advertise an unrelated mapping
            // from another local endpoint during asynchronous startup.
            builder = builder.portmapper_config(iroh::endpoint::PortmapperConfig::Disabled);
        }
    }
    let endpoint = builder.alpns(config.alpns).bind().await?;
    Ok(endpoint)
}

/// Path selector that pins each remote to the path its handshake
/// established. This is how `max_multipath_paths = 1` pins an iroh
/// connection: the transport config cannot express it (multipath floor
/// is 14; QNT cannot be disabled below 8 advertised addresses), and
/// merely returning an empty selection leaves every opened path usable
/// at the QUIC layer. Actively re-selecting the handshake path lets
/// iroh mark the rest as backup.
///
/// The first `select` call for a remote runs on its first path event —
/// only the dialed path can exist then (in-band-learned paths require
/// an established connection), so the first candidate is the handshake
/// path. Afterwards `ctx.current()` is that path and is kept.
#[derive(Debug)]
struct PinnedSelector;

impl iroh::endpoint::transports::PathSelector for PinnedSelector {
    fn select(
        &self,
        ctx: &iroh::endpoint::transports::PathSelectionContext<'_>,
    ) -> iroh::endpoint::transports::PathSelection {
        let mut selection = iroh::endpoint::transports::PathSelection::none();
        for path in ctx.paths() {
            // Once a path is selected, keep selecting it while it
            // exists. If it is gone, select nothing — the connection
            // stalls rather than silently escaping onto a clean path.
            let keep = match ctx.current() {
                Some(current) => path.network_path() == current,
                None => true,
            };
            if keep {
                selection.set(&path);
                break;
            }
        }
        selection
    }
}

// Compatibility path; persistent identity is shared by every backend.
pub use crate::identity::{default_key_path, load_or_create_key};

/// Serialized `EndpointAddr` for copy-paste dialing: `rds1<base32(postcard)>`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Ticket(pub EndpointAddr);

impl Ticket {
    /// Current address of `endpoint`, including its home relay once online.
    pub fn of(endpoint: &crate::Endpoint) -> Self {
        Self(endpoint.addr())
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.0.id
    }
}

impl std::fmt::Display for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = postcard::to_stdvec(&self.0).map_err(|_| std::fmt::Error)?;
        write!(
            f,
            "rds1{}",
            data_encoding::BASE32_NOPAD.encode(&bytes).to_lowercase()
        )
    }
}

/// A ticket carries one endpoint id plus a handful of transport addrs —
/// a few hundred encoded bytes. Refuse oversized bodies before base32
/// allocates and postcard decodes an unbounded address set.
const MAX_TICKET_BODY: usize = 4096;

impl FromStr for Ticket {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let body = s
            .strip_prefix("rds1")
            .ok_or_else(|| anyhow::anyhow!("ticket must start with 'rds1'"))?;
        anyhow::ensure!(body.len() <= MAX_TICKET_BODY, "ticket too long");
        let bytes = data_encoding::BASE32_NOPAD
            .decode(body.to_uppercase().as_bytes())
            .map_err(|_| anyhow::anyhow!("ticket is not valid base32"))?;
        let addr = postcard::from_bytes(&bytes)
            .map_err(|e| anyhow::anyhow!("ticket payload invalid: {e}"))?;
        Ok(Self(addr))
    }
}

/// Bare `EndpointId` or full ticket → dialable address.
///
/// An `EndpointId` alone relies on configured address lookup (n0 DNS by
/// default); a ticket carries relay and direct addresses explicitly.
pub fn parse_target(target: &str) -> anyhow::Result<EndpointAddr> {
    match Ticket::from_str(target) {
        Ok(ticket) => Ok(ticket.0),
        Err(ticket_error) => match EndpointId::from_str(target) {
            Ok(id) => Ok(EndpointAddr {
                id,
                addrs: Default::default(),
            }),
            // An rds1-prefixed string was meant as a ticket: surface the
            // ticket decode error, not a misleading endpoint-id complaint.
            Err(_) if target.starts_with("rds1") => Err(ticket_error),
            Err(id_error) => Err(id_error.into()),
        },
    }
}

/// Relay URL advertised by an address, if any.
pub fn relay_url_of(addr: &EndpointAddr) -> Option<RelayUrl> {
    addr.addrs.iter().find_map(|a| match a {
        TransportAddr::Relay(url) => Some(url.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecretKey;

    #[test]
    fn owned_relays_get_no_quic_address_discovery() {
        let relays: Vec<RelayUrl> = ["http://192.0.2.1:3340", "http://192.0.2.2:3340"]
            .iter()
            .map(|url| url.parse().unwrap())
            .collect();
        let map = owned_relay_map(&relays);
        assert_eq!(map.len(), 2);
        for url in &relays {
            let config = map.get(&convert::relay(url)).expect("relay kept");
            assert!(config.quic.is_none(), "{url} must not be probed for QAD");
        }
    }

    #[test]
    fn ticket_roundtrip() {
        let mut addrs = std::collections::BTreeSet::new();
        addrs.insert(TransportAddr::Ip(std::net::SocketAddr::from((
            [10, 0, 0, 7],
            12345,
        ))));
        addrs.insert(TransportAddr::Relay(
            RelayUrl::from_str("https://relay.example.com").unwrap(),
        ));
        let addr = EndpointAddr {
            id: SecretKey::generate().public(),
            addrs,
        };
        let text = Ticket(addr.clone()).to_string();
        assert!(text.starts_with("rds1"));
        let parsed = Ticket::from_str(&text).unwrap();
        assert_eq!(parsed.0, addr);
    }

    #[test]
    fn bare_endpoint_id_parses() {
        let id = SecretKey::generate().public();
        let addr = parse_target(&id.to_string()).unwrap();
        assert_eq!(addr.id, id);
        assert!(addr.addrs.is_empty());
    }

    /// An `rds1` string that fails ticket decode must surface the ticket
    /// error, not an endpoint-id complaint — the prefix already committed
    /// the input to the ticket grammar.
    #[test]
    fn malformed_ticket_surfaces_ticket_error() {
        let err = parse_target("rds1!!!not-base32!!!").unwrap_err();
        assert!(
            err.to_string().contains("ticket"),
            "unexpected error: {err}"
        );

        let oversized = format!("rds1{}", "a".repeat(MAX_TICKET_BODY + 1));
        let err = parse_target(&oversized).unwrap_err();
        assert_eq!(err.to_string(), "ticket too long");
    }

    /// A non-prefixed string that is neither ticket nor endpoint id keeps
    /// the endpoint-id error — it was never a ticket attempt.
    #[test]
    fn plain_garbage_surfaces_endpoint_error() {
        let err = parse_target("not-a-ticket-or-key").unwrap_err();
        assert!(
            !err.to_string().contains("ticket"),
            "unexpected error: {err}"
        );
    }

    /// The owned types must encode byte-identically to iroh-base:
    /// tickets and discovery records written by either side decode
    /// on the other. Guards the postcard layout contract.
    #[test]
    fn owned_and_iroh_addresses_encode_identically() {
        let key = SecretKey::generate();
        let owned = EndpointAddr::new(key.public())
            .with_ip_addr("10.0.0.7:12345".parse().unwrap())
            .with_relay_url(RelayUrl::from_str("https://relay.example.com").unwrap());
        let iroh_addr = iroh::EndpointAddr::new(convert::id(key.public()))
            .with_ip_addr("10.0.0.7:12345".parse().unwrap())
            .with_relay_url(iroh::RelayUrl::from_str("https://relay.example.com").unwrap());

        assert_eq!(
            postcard::to_stdvec(&owned).unwrap(),
            postcard::to_stdvec(&iroh_addr).unwrap()
        );
        assert_eq!(convert::addr_from(iroh_addr), owned);

        let ticket = Ticket(owned.clone()).to_string();
        let parsed = Ticket::from_str(&ticket).unwrap().0;
        assert_eq!(parsed, owned);
    }

    /// Every `ConnectionError` variant the backend can surface folds into
    /// the owned kind — telemetry must never see backend error vocabulary.
    #[test]
    fn connection_errors_map_to_owned_close_kinds() {
        use crate::CloseKind;
        use iroh::endpoint::{
            ApplicationClose, ConnectionClose, ConnectionError as E, TransportError,
            TransportErrorCode, VarInt,
        };

        let app_close = ApplicationClose {
            error_code: VarInt::from(0u32),
            reason: bytes::Bytes::new(),
        };
        let transport_close = ConnectionClose {
            error_code: TransportErrorCode::NO_ERROR,
            frame_type: noq_proto::MaybeFrame::None,
            reason: bytes::Bytes::new(),
        };
        let cases = [
            (E::LocallyClosed, CloseKind::Local),
            (
                E::ApplicationClosed(app_close.clone()),
                CloseKind::PeerApplication,
            ),
            (
                E::ConnectionClosed(transport_close),
                CloseKind::PeerTransport,
            ),
            (E::TimedOut, CloseKind::TimedOut),
            (E::Reset, CloseKind::Reset),
            (E::VersionMismatch, CloseKind::Transport),
            (
                E::TransportError(TransportError::new(
                    TransportErrorCode::INTERNAL_ERROR,
                    String::new(),
                )),
                CloseKind::Transport,
            ),
            (E::CidsExhausted, CloseKind::Transport),
        ];
        for (error, want) in cases {
            assert_eq!(close_kind(error), want);
        }
    }
}
