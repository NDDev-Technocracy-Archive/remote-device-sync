//! Relay server library for rds.
//!
//! Two generations live here:
//!
//! - [`serve`] — the default relay: embedded `iroh-relay` with an
//!   endpoint allowlist. Forwards already-encrypted QUIC traffic; cannot
//!   read session content.
//! - [`proto`] — the owned relay wire protocol. The `owned-relay` feature
//!   adds its QUIC server; [`RelayArgs`] selects either implementation for
//!   the standalone binary and the GDS-facing `rds-server` composition.

pub mod proto;
mod runtime;
pub use runtime::{
    PreparedRelay, ReadyRelay, RelayArgs, RelayBackend, RelayBinding, RelayConfigError,
    RelayMetrics, RelayRuntimeError, RunningRelay, shutdown_signal,
};
#[cfg(feature = "owned-relay")]
pub mod server;

/// Whether the owned QUIC relay backend (`--relay-backend noq`) was
/// compiled into this build.
///
/// Workspace feature unification can enable it on this library while a
/// depending package's own `owned-relay` flag stays off — for example
/// `cargo test --workspace` builds `rds-bench`'s dev-dependency edge
/// (`transport-noq`), which turns the backend on inside the `rds-server`
/// binary even though `rds-server`'s flag is unset. Tests that spawn
/// the composed binary must probe this constant rather than their own
/// `cfg!(feature = "owned-relay")`, which would answer for the wrong
/// package.
pub const OWNED_BACKEND_COMPILED: bool = cfg!(feature = "owned-relay");

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use iroh::EndpointId;
use iroh_relay::server::{
    Access, AccessControl, AcmeConfig, CertConfig, ClientRequest, ConnectionId, RelayConfig,
    Server, ServerConfig, TlsConfig,
};

/// Admit only endpoint ids on the relay allowlist.
#[derive(Debug)]
struct AllowList(HashSet<EndpointId>);

impl AccessControl for AllowList {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        if self.0.contains(&request.endpoint_id()) {
            Access::Allow
        } else {
            Access::Deny {
                reason: Some("endpoint id not on relay allowlist".into()),
            }
        }
    }

    fn on_disconnect(&self, _endpoint_id: EndpointId, _connection_id: ConnectionId) {}
}

/// TLS mode for the relay's HTTPS listener.
///
/// When set, the relay protocol moves to `https_addr`; `addr` keeps
/// serving only the plaintext captive-portal probe. Endpoints then dial
/// `--relay https://<host>:<port>`.
#[derive(Debug)]
pub enum RelayTls {
    /// PEM certificate chain and private key files (e.g. certbot or an
    /// internal CA). Relayed payloads stay end-to-end encrypted either
    /// way; TLS here hides relay *usage* metadata from on-path observers.
    Manual {
        https_addr: SocketAddr,
        cert_pem: PathBuf,
        key_pem: PathBuf,
    },
    /// Let's Encrypt via in-process ACME (TLS-ALPN-01). Requires the
    /// HTTPS listener on port 443, reachable from the internet.
    LetsEncrypt {
        https_addr: SocketAddr,
        domains: Vec<String>,
        /// ACME contacts; emails need a `mailto:` prefix.
        contact: Vec<String>,
        /// Directory caching issued certificates across restarts.
        cache_dir: PathBuf,
        /// Use the LE staging directory (for tests; certs untrusted).
        staging: bool,
    },
}

/// Spawn the iroh relay on `addr`, restricted to `allow` when non-empty.
/// Returns the bound server; dropping it stops the relay.
pub async fn serve(
    addr: SocketAddr,
    allow: Vec<EndpointId>,
    tls: Option<RelayTls>,
) -> anyhow::Result<Server> {
    let tls = tls.map(tls_config).transpose()?;
    serve_prepared(addr, allow, tls).await
}

async fn serve_prepared(
    addr: SocketAddr,
    allow: Vec<EndpointId>,
    tls: Option<TlsConfig>,
) -> anyhow::Result<Server> {
    let mut relay_config = RelayConfig::new(addr);
    if !allow.is_empty() {
        relay_config.access = Arc::new(AllowList(allow.into_iter().collect()));
    }
    relay_config.tls = tls;
    let mut config = ServerConfig::default();
    config.relay = Some(relay_config);
    Ok(Server::spawn(config).await?)
}

/// Resolve CLI-shaped TLS flags into a [`RelayTls`].
///
/// `None` when neither manual nor ACME flags are present. ACME requires a
/// cache directory: without one every restart re-issues certificates and
/// hits Let's Encrypt rate limits.
pub fn tls_from_flags(
    https_addr: SocketAddr,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    acme_domains: Vec<String>,
    acme_contact: Vec<String>,
    acme_cache: Option<PathBuf>,
    acme_staging: bool,
) -> anyhow::Result<Option<RelayTls>> {
    match (cert, key, acme_domains.is_empty()) {
        (Some(cert_pem), Some(key_pem), _) => Ok(Some(RelayTls::Manual {
            https_addr,
            cert_pem,
            key_pem,
        })),
        (None, None, false) => {
            let cache_dir = acme_cache
                .ok_or_else(|| anyhow::anyhow!("--tls-acme-cache is required for ACME"))?;
            Ok(Some(RelayTls::LetsEncrypt {
                https_addr,
                domains: acme_domains,
                contact: acme_contact,
                cache_dir,
                staging: acme_staging,
            }))
        }
        (None, None, true) => Ok(None),
        // clap `requires`/`conflicts_with` guard the halves.
        _ => anyhow::bail!("--tls-cert and --tls-key must be given together"),
    }
}

fn tls_config(tls: RelayTls) -> anyhow::Result<TlsConfig> {
    // Reuse the existing ring provider. Its Rust API includes native crypto;
    // this does not claim a pure-Rust transitive dependency graph.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?;
    Ok(match tls {
        RelayTls::Manual {
            https_addr,
            cert_pem,
            key_pem,
        } => {
            use rustls_pki_types::pem::PemObject;
            let cert_bytes = read_bounded(&cert_pem, 1024 * 1024)?;
            let key_bytes = read_bounded(&key_pem, 64 * 1024)?;
            let certs = rustls_pki_types::CertificateDer::pem_slice_iter(&cert_bytes)
                .collect::<Result<Vec<_>, _>>()?;
            let key = rustls_pki_types::PrivateKeyDer::from_pem_slice(&key_bytes)?;
            let server_config = builder.with_no_client_auth().with_single_cert(certs, key)?;
            TlsConfig::new(https_addr, CertConfig::Manual { server_config })
        }
        RelayTls::LetsEncrypt {
            https_addr,
            domains,
            contact,
            cache_dir,
            staging,
        } => {
            let acme = AcmeConfig::letsencrypt(!staging)
                .domains(domains)
                .contact(contact)
                .cache_path(cache_dir);
            TlsConfig::new(
                https_addr,
                CertConfig::LetsEncrypt {
                    acme_config: acme,
                    // iroh-relay injects the ACME cert resolver itself.
                    server_config_builder: builder.with_no_client_auth(),
                },
            )
        }
    })
}

fn read_bounded(path: &std::path::Path, limit: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "relay PEM file exceeds its size limit",
        ));
    }
    Ok(bytes)
}
