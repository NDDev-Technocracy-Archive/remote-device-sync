//! Versioned, side-effect-free endpoint configuration shared by binaries.
//! Endpoint secrets and grant/directory authorities are provisioned separately.

use std::collections::HashSet;
use std::io::Read;
use std::net::SocketAddr;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Backend, EndpointConfig, RelayUrl};

pub const CONFIG_VERSION: u32 = 1;
pub const MAX_CONFIG_BYTES: usize = 16 * 1024;
const MAX_BINDS: usize = 16;
const MAX_RELAYS: usize = 8;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("endpoint configuration I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("endpoint configuration JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid endpoint configuration: {0}")]
    Invalid(&'static str),
}

impl FromStr for Backend {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "iroh" => Ok(Self::Iroh),
            #[cfg(feature = "transport-noq")]
            "noq" => Ok(Self::Noq),
            _ => Err(ConfigError::Invalid(
                "unknown or unavailable backend; expected iroh or feature-enabled noq",
            )),
        }
    }
}

/// Positive per-tunnel resource limits. These bound application queues and
/// peer mappings, not the QUIC engine's buffers or global endpoint resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelayLimits {
    pub max_peers: NonZeroU16,
    pub datagram_queue: NonZeroU16,
    /// Unpinned entries expire after inactivity; capacity pressure may evict
    /// them earlier. Active connection leases are never evicted.
    pub peer_grace_secs: NonZeroU32,
}
impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            max_peers: NonZeroU16::new(1024).expect("positive limit"),
            datagram_queue: NonZeroU16::new(128).expect("positive limit"),
            peer_grace_secs: NonZeroU32::new(30).expect("positive grace"),
        }
    }
}
impl RelayLimits {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Explicit reachability preset. Custom relays disable public lookup.
/// Empty struct variants also reject unknown fields during deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelaySettings {
    /// Iroh's public relay/lookup preset, or direct-only on owned transport.
    Default {},
    /// Direct-only, with no public relay or lookup services.
    Disabled {},
    /// Iroh protocol relay origins; no public lookup services.
    Iroh { urls: Vec<RelayUrl> },
    /// Key-pinned owned relay locator: `rds-relay://PUBLIC_HEX_KEY@IP:PORT`.
    Owned {
        route: String,
        #[serde(default, skip_serializing_if = "RelayLimits::is_default")]
        limits: RelayLimits,
    },
}

impl Default for RelaySettings {
    fn default() -> Self {
        Self::Default {}
    }
}

/// Endpoint-only file schema; deliberately contains no signing keys or policy
/// authorities. Unknown fields, unsupported versions and modes are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSettings {
    pub schema_version: u32,
    #[serde(default)]
    pub backend: Backend,
    #[serde(default)]
    pub bind_addrs: Vec<SocketAddr>,
    #[serde(default)]
    pub relay: RelaySettings,
    #[serde(default)]
    pub max_multipath_paths: Option<u32>,
}

impl Default for EndpointSettings {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_VERSION,
            backend: Backend::default(),
            bind_addrs: Vec::new(),
            relay: RelaySettings::default(),
            max_multipath_paths: None,
        }
    }
}

/// Only explicitly supplied flags override a file; repeated relay/bind flags
/// replace their whole list. Relay modes are mutually exclusive.
#[derive(Debug, Default)]
pub struct EndpointOverrides {
    pub backend: Option<Backend>,
    pub bind_addrs: Vec<SocketAddr>,
    pub relays: Vec<String>,
    pub owned_relay: Option<String>,
    pub no_relay: bool,
}

impl EndpointSettings {
    pub fn from_json(bytes: &[u8]) -> Result<Self, ConfigError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::Invalid("file exceeds 16 KiB"));
        }
        let settings: Self = serde_json::from_slice(bytes)?;
        if settings.schema_version != CONFIG_VERSION {
            return Err(ConfigError::Invalid("unsupported schema_version"));
        }
        Ok(settings)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        use rustix::fs::{Mode, OFlags};
        // Config paths are caller-selected and may be symlinks. NONBLOCK plus
        // fstat refuses FIFOs/devices without hanging before the size bound.
        let file = std::fs::File::from(
            rustix::fs::open(
                path,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        if !file.metadata()?.is_file() {
            return Err(ConfigError::Invalid("configuration must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Self::from_json(&bytes)
    }

    pub fn apply(mut self, flags: EndpointOverrides) -> Result<Self, ConfigError> {
        let modes = usize::from(!flags.relays.is_empty())
            + usize::from(flags.owned_relay.is_some())
            + usize::from(flags.no_relay);
        if modes > 1 {
            return Err(ConfigError::Invalid(
                "relay, owned-relay and no-relay are mutually exclusive",
            ));
        }
        if let Some(backend) = flags.backend {
            self.backend = backend;
        }
        if !flags.bind_addrs.is_empty() {
            self.bind_addrs = flags.bind_addrs;
        }
        if !flags.relays.is_empty() {
            self.relay = RelaySettings::Iroh {
                urls: flags
                    .relays
                    .iter()
                    .map(|url| {
                        url.parse()
                            .map_err(|_| ConfigError::Invalid("invalid iroh relay origin"))
                    })
                    .collect::<Result<_, _>>()?,
            };
        } else if let Some(route) = flags.owned_relay {
            let limits = match &self.relay {
                RelaySettings::Owned { limits, .. } => *limits,
                _ => RelayLimits::default(),
            };
            self.relay = RelaySettings::Owned { route, limits };
        } else if flags.no_relay {
            self.relay = RelaySettings::Disabled {};
        }
        Ok(self)
    }

    /// Validate and lower settings without creating an identity or socket.
    pub fn into_endpoint(self) -> Result<EndpointConfig, ConfigError> {
        if self.schema_version != CONFIG_VERSION {
            return Err(ConfigError::Invalid("unsupported schema_version"));
        }
        let mut config = EndpointConfig {
            backend: self.backend,
            bind_addrs: self.bind_addrs,
            max_multipath_paths: self.max_multipath_paths,
            discovery: self.backend == Backend::Iroh,
            ..Default::default()
        };
        match self.relay {
            RelaySettings::Default {} => {}
            RelaySettings::Disabled {} => config.discovery = false,
            RelaySettings::Iroh { urls } => {
                if urls.is_empty() {
                    return Err(ConfigError::Invalid(
                        "iroh relay mode requires at least one origin",
                    ));
                }
                config.relays = urls;
                config.discovery = false;
            }
            RelaySettings::Owned { route, limits } => set_owned_relay(&mut config, &route, limits)?,
        }
        config.validate()?;
        Ok(config)
    }
}

#[cfg(feature = "transport-noq")]
fn set_owned_relay(
    config: &mut EndpointConfig,
    route: &str,
    limits: RelayLimits,
) -> Result<(), ConfigError> {
    let route: rds_discovery::OwnedRelayRoute = route
        .parse()
        .map_err(|_| ConfigError::Invalid("invalid owned relay locator"))?;
    let id = crate::EndpointId::from_bytes(&route.key.0)
        .map_err(|_| ConfigError::Invalid("invalid owned relay identity"))?;
    config.relay_endpoint = Some(crate::EndpointAddr::new(id).with_ip_addr(route.addr));
    config.relay_limits = limits;
    config.discovery = false;
    Ok(())
}

#[cfg(not(feature = "transport-noq"))]
fn set_owned_relay(
    _config: &mut EndpointConfig,
    _route: &str,
    _limits: RelayLimits,
) -> Result<(), ConfigError> {
    Err(ConfigError::Invalid(
        "owned relay requires the transport-noq feature",
    ))
}

impl EndpointConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_for(self.backend)
    }

    // Low-level backend entry points choose their backend by function name.
    // Validate the actual backend there too, including injected-socket paths.
    pub(crate) fn validate_for(&self, backend: Backend) -> Result<(), ConfigError> {
        // Port zero requests a fresh allocation, so identical :0 requests
        // become distinct sockets. Only fixed-address duplicates conflict.
        let mut fixed = HashSet::new();
        if self.bind_addrs.len() > MAX_BINDS
            || self
                .bind_addrs
                .iter()
                .any(|a| a.port() != 0 && !fixed.insert(a))
        {
            return Err(ConfigError::Invalid(
                "at most 16 bind addresses; fixed addresses must be distinct",
            ));
        }
        if backend == Backend::Iroh && self.bind_addrs.len() > 1 {
            return Err(ConfigError::Invalid("iroh supports only one bind address"));
        }
        if matches!(self.max_multipath_paths, Some(0 | 33..)) {
            return Err(ConfigError::Invalid("max_multipath_paths must be 1..32"));
        }
        if self.alpns.is_empty()
            || self.alpns.len() > 16
            || self.alpns.iter().any(|a| a.is_empty() || a.len() > 255)
            || self.alpns.iter().collect::<HashSet<_>>().len() != self.alpns.len()
        {
            return Err(ConfigError::Invalid(
                "ALPNs must be 1..16 distinct identifiers of 1..255 bytes",
            ));
        }
        if self.relays.len() > MAX_RELAYS
            || self.relays.iter().collect::<HashSet<_>>().len() != self.relays.len()
        {
            return Err(ConfigError::Invalid(
                "iroh relay origins must be distinct and at most 8",
            ));
        }
        match self.transports {
            crate::Transports::All => {}
            crate::Transports::DirectOnly => {
                if !self.relays.is_empty() {
                    return Err(ConfigError::Invalid(
                        "direct-only transports reject configured relays",
                    ));
                }
            }
            crate::Transports::RelayOnly => {
                if backend == Backend::Iroh && self.relays.is_empty() {
                    return Err(ConfigError::Invalid(
                        "relay-only transports require at least one relay",
                    ));
                }
            }
        }
        for url in &self.relays {
            if url.as_str().len() > 512
                || !matches!(url.scheme(), "http" | "https")
                || url.host().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
                || url.port_or_known_default() == Some(0)
            {
                return Err(ConfigError::Invalid(
                    "iroh relay requires an HTTP(S) origin without credentials, path, query or fragment",
                ));
            }
        }
        self.validate_owned(backend)
    }

    #[cfg(feature = "transport-noq")]
    fn validate_owned(&self, backend: Backend) -> Result<(), ConfigError> {
        if backend == Backend::Noq && !self.relays.is_empty() {
            return Err(ConfigError::Invalid(
                "noq cannot use iroh relay URLs; configure an owned relay",
            ));
        }
        if self.relay_limits != RelayLimits::default()
            && (backend != Backend::Noq || self.relay_endpoint.is_none())
        {
            return Err(ConfigError::Invalid(
                "custom owned-relay limits require an owned relay",
            ));
        }
        if let Some(relay) = &self.relay_endpoint {
            if backend != Backend::Noq {
                return Err(ConfigError::Invalid("owned relay requires the noq backend"));
            }
            if self.transports == crate::Transports::DirectOnly {
                return Err(ConfigError::Invalid(
                    "direct-only transports reject an owned relay attachment",
                ));
            }
            if relay.addrs.len() != 1 || !relay.addrs.iter().all(|a| matches!(a, crate::TransportAddr::Ip(addr) if addr.port() != 0 && !addr.ip().is_unspecified() && !addr.ip().is_multicast())) {
                return Err(ConfigError::Invalid("owned relay bootstrap requires exactly one usable direct IP address"));
            }
        }
        if self.transports == crate::Transports::RelayOnly
            && backend == Backend::Noq
            && self.relay_endpoint.is_none()
        {
            return Err(ConfigError::Invalid(
                "relay-only transports require an owned relay attachment",
            ));
        }
        Ok(())
    }

    #[cfg(not(feature = "transport-noq"))]
    fn validate_owned(&self, _backend: Backend) -> Result<(), ConfigError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
