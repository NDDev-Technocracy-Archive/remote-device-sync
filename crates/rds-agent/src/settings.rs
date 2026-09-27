//! Versioned agent configuration: role, service, peer, authority, limit
//! and timeout policy as one checked document.
//!
//! `AgentSettings` mirrors `EndpointSettings`: a bounded regular file,
//! `schema_version` gated, unknown fields rejected, explicit flags
//! overriding file values. The file carries no secrets — issuers and
//! registry/revocation keys are verifying keys, and state paths are only
//! locations.

use std::collections::BTreeSet;
use std::io::Read;
use std::num::{NonZeroU16, NonZeroU64};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use rds_core::{ServiceKind, TcpTarget};
use rds_net::EndpointId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{MAX_TIMEOUT, TimeoutPolicy};

pub const AGENT_CONFIG_VERSION: u32 = 1;
pub const MAX_AGENT_CONFIG_BYTES: usize = 16 * 1024;
const MAX_ALLOWLIST: usize = 256;
const MAX_TCP_TARGETS: usize = 64;
const MAX_GRANT_TTL_SECS: u64 = 86_400;

#[derive(Debug, Error)]
pub enum AgentConfigError {
    #[error("agent configuration I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("agent configuration JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid agent configuration: {0}")]
    Invalid(&'static str),
}

/// A deployment role preset: which data-plane services the agent serves.
/// `Ping`/`Info` are the always-on control plane in every role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// TCP forwarding only — the historical default.
    Access,
    /// File synchronization only.
    Sync,
    /// Desktop sessions only.
    Desktop,
    /// Every implemented service the binary can serve.
    Full,
}

impl Role {
    fn services(self) -> BTreeSet<ServiceKind> {
        match self {
            Self::Access => BTreeSet::from([ServiceKind::Tcp]),
            Self::Sync => BTreeSet::from([ServiceKind::Sync]),
            Self::Desktop => BTreeSet::from([ServiceKind::Desktop]),
            Self::Full => {
                BTreeSet::from([ServiceKind::Tcp, ServiceKind::Desktop, ServiceKind::Sync])
            }
        }
    }
}

impl FromStr for Role {
    type Err = AgentConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "access" => Ok(Self::Access),
            "sync" => Ok(Self::Sync),
            "desktop" => Ok(Self::Desktop),
            "full" => Ok(Self::Full),
            _ => Err(AgentConfigError::Invalid(
                "unknown role; expected access, sync, desktop or full",
            )),
        }
    }
}

/// A gateable data-plane service name in configuration. `ping` and `info`
/// are control-plane and always served; `audio` is wire-reserved but not
/// implemented and is rejected rather than silently advertised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceName {
    Tcp,
    Desktop,
    Sync,
    Audio,
}

impl ServiceName {
    pub fn kind(self) -> ServiceKind {
        match self {
            Self::Tcp => ServiceKind::Tcp,
            Self::Desktop => ServiceKind::Desktop,
            Self::Sync => ServiceKind::Sync,
            Self::Audio => ServiceKind::Audio,
        }
    }
}

impl FromStr for ServiceName {
    type Err = AgentConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "tcp" => Ok(Self::Tcp),
            "desktop" => Ok(Self::Desktop),
            "sync" => Ok(Self::Sync),
            "audio" => Ok(Self::Audio),
            _ => Err(AgentConfigError::Invalid(
                "unknown service; expected tcp, desktop, sync or audio",
            )),
        }
    }
}

/// Per-service knobs. `tcp_targets` extends the permitted destination set
/// beyond the single SSH socket the `--ssh` flag configures.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceSettings {
    pub ssh_target: Option<String>,
    pub tcp_targets: Vec<String>,
    pub allow_any_tcp: bool,
    pub sync_dir: Option<PathBuf>,
}

/// The membership allowlist: peers that may open any stream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeerSettings {
    pub allow: Vec<String>,
}

/// Registry authority (device-name trust) configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RegistrySettings {
    pub key: Option<String>,
    pub epoch: Option<u64>,
    pub state: Option<PathBuf>,
    pub rotations: Vec<PathBuf>,
}

/// Revocation snapshot authority configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RevocationSettings {
    pub key: Option<String>,
    pub epoch: Option<u64>,
    pub state: Option<PathBuf>,
    pub interval_secs: Option<u64>,
    pub rotations: Vec<PathBuf>,
}

/// Grant issuers, TTL and the directory/revocation authority surfaces.
/// Directory-dependent fields require `directory` so a partial authority
/// posture fails loudly instead of being silently unused.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthoritySettings {
    /// Trusted grant issuers (base32 Ed25519 verifying keys).
    pub issuers: Vec<String>,
    pub grant_ttl_secs: Option<u64>,
    /// Tenant this agent answers (grant v3 claim binding). When set, every
    /// grant must carry the same `tenant`; unscoped grants are refused.
    pub tenant: Option<String>,
    /// Minimum `policy_revision` a grant must claim (grant v3). Grants
    /// minted under older estate policy are refused without waiting for
    /// expiry or revocation.
    pub policy_min_revision: Option<u64>,
    /// Directory HTTP(S) origin or legacy IP:port.
    pub directory: Option<String>,
    pub directory_ca: Option<PathBuf>,
    pub record_ttl_secs: Option<u64>,
    pub record_state: Option<PathBuf>,
    pub registry: Option<RegistrySettings>,
    pub revocations: Option<RevocationSettings>,
}

/// Admission budgets; absent values keep the built-in defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitSettings {
    pub max_connections: Option<NonZeroU16>,
    pub max_streams: Option<NonZeroU16>,
    /// Process-wide open-descriptor ceiling; once the kernel reports the
    /// process at or above it, new connections are refused until usage
    /// falls. Absent = ungated.
    pub max_fds: Option<NonZeroU64>,
    /// Process-wide resident-set ceiling in MiB, same admission rule as
    /// `max_fds`. Absent = ungated.
    pub max_rss_mb: Option<NonZeroU64>,
}

/// Connection-admission and stream-greeting deadlines in seconds, each
/// 1..=3600. Service-internal budgets are separate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutSettings {
    pub handshake_secs: Option<u64>,
    pub hello_secs: Option<u64>,
    /// Authorization-path reply budget (refusal and `HelloAck` writes).
    pub authz_secs: Option<u64>,
    /// Join budget for established connection tasks during shutdown.
    pub shutdown_secs: Option<u64>,
}

/// Agent-level file schema; holds policy identities, never secret key
/// material. Unknown fields and unsupported versions are refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    pub schema_version: u32,
    /// Service preset; mutually exclusive with `services`.
    #[serde(default)]
    pub role: Option<Role>,
    /// Explicit data-plane service list; mutually exclusive with `role`.
    #[serde(default)]
    pub services: Option<Vec<ServiceName>>,
    /// Services removed from the role/implicit set after selection.
    #[serde(default)]
    pub disabled_services: Vec<ServiceName>,
    #[serde(default)]
    pub service: ServiceSettings,
    #[serde(default)]
    pub peers: PeerSettings,
    #[serde(default)]
    pub authority: AuthoritySettings,
    #[serde(default)]
    pub limits: LimitSettings,
    #[serde(default)]
    pub timeouts: TimeoutSettings,
}

/// Only explicitly supplied flags override a file; repeated flags replace
/// their whole corresponding list. `--no-service` replaces the file's
/// `disabled_services` list.
#[derive(Debug, Default)]
pub struct AgentOverrides {
    pub role: Option<Role>,
    pub services: Vec<ServiceName>,
    pub no_services: Vec<ServiceName>,
    pub ssh: Option<TcpTarget>,
    pub allow_any_tcp: bool,
    pub sync_dir: Option<PathBuf>,
    pub allow: Vec<String>,
    pub issuers: Vec<String>,
    pub grant_ttl: Option<u64>,
    pub tenant: Option<String>,
    pub policy_min_revision: Option<u64>,
    pub directory: Option<String>,
    pub directory_ca: Option<PathBuf>,
    pub record_ttl: Option<u64>,
    pub record_state: Option<PathBuf>,
    pub registry_key: Option<String>,
    pub registry_epoch: Option<u64>,
    pub registry_state: Option<PathBuf>,
    pub registry_rotations: Vec<PathBuf>,
    pub revocations_key: Option<String>,
    pub revocations_epoch: Option<u64>,
    pub revocations_state: Option<PathBuf>,
    pub revocations_rotations: Vec<PathBuf>,
    pub revocations_interval: Option<u64>,
    pub max_connections: Option<NonZeroU16>,
    pub max_streams: Option<NonZeroU16>,
    pub max_fds: Option<NonZeroU64>,
    pub max_rss_mb: Option<NonZeroU64>,
    pub handshake_timeout: Option<u64>,
    pub hello_timeout: Option<u64>,
    pub authz_timeout: Option<u64>,
    pub shutdown_timeout: Option<u64>,
}

/// The merged, typed configuration the binary consumes.
#[derive(Debug)]
pub struct ResolvedAgent {
    /// `None` keeps the implicit service set (`services`/`role` absent and
    /// nothing disabled); `Some` is the resolved explicit set.
    pub services: Option<BTreeSet<ServiceKind>>,
    pub ssh_target: Option<TcpTarget>,
    pub tcp_targets: Vec<TcpTarget>,
    pub allow_any_tcp: bool,
    pub sync_dir: Option<PathBuf>,
    pub allow: Vec<EndpointId>,
    pub issuers: Vec<[u8; 32]>,
    pub grant_ttl: Option<u64>,
    pub tenant: Option<String>,
    pub policy_min_revision: Option<u64>,
    pub directory: Option<String>,
    pub directory_ca: Option<PathBuf>,
    pub record_ttl: Option<u64>,
    pub record_state: Option<PathBuf>,
    pub registry_key: Option<String>,
    pub registry_epoch: Option<u64>,
    pub registry_state: Option<PathBuf>,
    pub registry_rotations: Vec<PathBuf>,
    pub revocations_key: Option<String>,
    pub revocations_epoch: Option<u64>,
    pub revocations_state: Option<PathBuf>,
    pub revocations_rotations: Vec<PathBuf>,
    pub revocations_interval: Option<u64>,
    pub max_connections: Option<NonZeroU16>,
    pub max_streams: Option<NonZeroU16>,
    /// Process fd ceiling (see `LimitSettings::max_fds`).
    pub max_fds: Option<NonZeroU64>,
    /// Process resident-set ceiling in MiB.
    pub max_rss_mb: Option<NonZeroU64>,
    pub timeouts: Option<TimeoutPolicy>,
}

fn decode_issuer(encoded: &str) -> Result<[u8; 32], AgentConfigError> {
    let bytes = data_encoding::BASE32_NOPAD
        .decode(encoded.to_uppercase().as_bytes())
        .map_err(|_| AgentConfigError::Invalid("issuer is not base32"))?;
    bytes
        .try_into()
        .map_err(|_| AgentConfigError::Invalid("issuer is not a 32-byte key"))
}

fn duplicate_services(list: &[ServiceName]) -> bool {
    let mut seen = BTreeSet::new();
    list.iter().any(|s| !seen.insert(*s))
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            schema_version: AGENT_CONFIG_VERSION,
            role: None,
            services: None,
            disabled_services: Vec::new(),
            service: ServiceSettings::default(),
            peers: PeerSettings::default(),
            authority: AuthoritySettings::default(),
            limits: LimitSettings::default(),
            timeouts: TimeoutSettings::default(),
        }
    }
}

impl AgentSettings {
    pub fn from_json(bytes: &[u8]) -> Result<Self, AgentConfigError> {
        if bytes.len() > MAX_AGENT_CONFIG_BYTES {
            return Err(AgentConfigError::Invalid("file exceeds 16 KiB"));
        }
        let settings: Self = serde_json::from_slice(bytes)?;
        if settings.schema_version != AGENT_CONFIG_VERSION {
            return Err(AgentConfigError::Invalid("unsupported schema_version"));
        }
        Ok(settings)
    }

    pub fn load(path: &Path) -> Result<Self, AgentConfigError> {
        use rustix::fs::{Mode, OFlags};
        // Same posture as EndpointSettings::load: a caller-selected path
        // may be a symlink; NONBLOCK plus the regular-file check refuses
        // FIFOs/devices before the size bound is applied.
        let file = std::fs::File::from(
            rustix::fs::open(
                path,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        if !file.metadata()?.is_file() {
            return Err(AgentConfigError::Invalid(
                "configuration must be a regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_AGENT_CONFIG_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Self::from_json(&bytes)
    }

    /// Merge flag overrides. Explicit scalars replace file values;
    /// repeated flags replace their whole list. Role and explicit service
    /// selection are one surface: whichever the flags supply wins.
    pub fn apply(mut self, flags: AgentOverrides) -> Self {
        if !flags.services.is_empty() {
            self.services = Some(flags.services);
            self.role = None;
        } else if let Some(role) = flags.role {
            self.role = Some(role);
            self.services = None;
        }
        if !flags.no_services.is_empty() {
            self.disabled_services = flags.no_services;
        }
        if let Some(ssh) = flags.ssh {
            self.service.ssh_target = Some(ssh.to_string());
        }
        self.service.allow_any_tcp |= flags.allow_any_tcp;
        if flags.sync_dir.is_some() {
            self.service.sync_dir = flags.sync_dir;
        }
        if !flags.allow.is_empty() {
            self.peers.allow = flags.allow;
        }
        if !flags.issuers.is_empty() {
            self.authority.issuers = flags.issuers;
        }
        if flags.grant_ttl.is_some() {
            self.authority.grant_ttl_secs = flags.grant_ttl;
        }
        if flags.tenant.is_some() {
            self.authority.tenant = flags.tenant;
        }
        if flags.policy_min_revision.is_some() {
            self.authority.policy_min_revision = flags.policy_min_revision;
        }
        if flags.directory.is_some() {
            self.authority.directory = flags.directory;
        }
        if flags.directory_ca.is_some() {
            self.authority.directory_ca = flags.directory_ca;
        }
        if flags.record_ttl.is_some() {
            self.authority.record_ttl_secs = flags.record_ttl;
        }
        if flags.record_state.is_some() {
            self.authority.record_state = flags.record_state;
        }
        if flags.registry_key.is_some()
            || flags.registry_epoch.is_some()
            || flags.registry_state.is_some()
            || !flags.registry_rotations.is_empty()
        {
            let registry = self.authority.registry.get_or_insert_with(Default::default);
            if let Some(key) = flags.registry_key {
                registry.key = Some(key);
            }
            if flags.registry_epoch.is_some() {
                registry.epoch = flags.registry_epoch;
            }
            if flags.registry_state.is_some() {
                registry.state = flags.registry_state;
            }
            if !flags.registry_rotations.is_empty() {
                registry.rotations = flags.registry_rotations;
            }
        }
        if flags.revocations_key.is_some()
            || flags.revocations_epoch.is_some()
            || flags.revocations_state.is_some()
            || flags.revocations_interval.is_some()
            || !flags.revocations_rotations.is_empty()
        {
            let revocations = self
                .authority
                .revocations
                .get_or_insert_with(Default::default);
            if let Some(key) = flags.revocations_key {
                revocations.key = Some(key);
            }
            if flags.revocations_epoch.is_some() {
                revocations.epoch = flags.revocations_epoch;
            }
            if flags.revocations_state.is_some() {
                revocations.state = flags.revocations_state;
            }
            if flags.revocations_interval.is_some() {
                revocations.interval_secs = flags.revocations_interval;
            }
            if !flags.revocations_rotations.is_empty() {
                revocations.rotations = flags.revocations_rotations;
            }
        }
        if flags.max_connections.is_some() {
            self.limits.max_connections = flags.max_connections;
        }
        if flags.max_streams.is_some() {
            self.limits.max_streams = flags.max_streams;
        }
        if flags.max_fds.is_some() {
            self.limits.max_fds = flags.max_fds;
        }
        if flags.max_rss_mb.is_some() {
            self.limits.max_rss_mb = flags.max_rss_mb;
        }
        if flags.handshake_timeout.is_some() {
            self.timeouts.handshake_secs = flags.handshake_timeout;
        }
        if flags.hello_timeout.is_some() {
            self.timeouts.hello_secs = flags.hello_timeout;
        }
        if flags.authz_timeout.is_some() {
            self.timeouts.authz_secs = flags.authz_timeout;
        }
        if flags.shutdown_timeout.is_some() {
            self.timeouts.shutdown_secs = flags.shutdown_timeout;
        }
        self
    }

    /// Structural checks on the merged document: mutual exclusion,
    /// duplicates, bounds and cross-field requirements. Typed parsing of
    /// addresses and keys happens in [`AgentSettings::resolve`].
    pub fn validate(&self) -> Result<(), AgentConfigError> {
        if self.role.is_some() && self.services.is_some() {
            return Err(AgentConfigError::Invalid(
                "role and services are mutually exclusive",
            ));
        }
        if let Some(services) = &self.services
            && duplicate_services(services)
        {
            return Err(AgentConfigError::Invalid("duplicate service name"));
        }
        if duplicate_services(&self.disabled_services) {
            return Err(AgentConfigError::Invalid("duplicate disabled service"));
        }
        if self
            .services
            .iter()
            .flatten()
            .chain(self.disabled_services.iter())
            .any(|s| *s == ServiceName::Audio)
        {
            return Err(AgentConfigError::Invalid(
                "audio service is reserved but not implemented",
            ));
        }
        if self.service.tcp_targets.len() > MAX_TCP_TARGETS {
            return Err(AgentConfigError::Invalid(
                "at most 64 permitted tcp targets",
            ));
        }
        if self.peers.allow.len() > MAX_ALLOWLIST {
            return Err(AgentConfigError::Invalid("at most 256 allowed peers"));
        }
        let authority = &self.authority;
        if authority
            .grant_ttl_secs
            .is_some_and(|secs| secs == 0 || secs > MAX_GRANT_TTL_SECS)
        {
            return Err(AgentConfigError::Invalid(
                "grant_ttl_secs must be 1..=86400",
            ));
        }
        if let Some(tenant) = &authority.tenant
            && (tenant.is_empty()
                || tenant.len() > rds_core::grant::MAX_TENANT_LEN
                || tenant.bytes().any(|b| b < 0x21 || b == 0x7f))
        {
            return Err(AgentConfigError::Invalid(
                "tenant must be nonempty, at most 64 bytes and free of control or whitespace bytes",
            ));
        }
        if authority.issuers.is_empty()
            && (authority.tenant.is_some() || authority.policy_min_revision.is_some())
        {
            return Err(AgentConfigError::Invalid(
                "tenant/policy_min_revision require at least one grant issuer",
            ));
        }
        if authority.directory.is_none()
            && (authority.directory_ca.is_some()
                || authority.record_ttl_secs.is_some()
                || authority.record_state.is_some()
                || authority.registry.is_some()
                || authority.revocations.is_some())
        {
            return Err(AgentConfigError::Invalid(
                "registry, revocations and record settings require a directory",
            ));
        }
        if let Some(registry) = &authority.registry
            && registry.key.is_none()
        {
            return Err(AgentConfigError::Invalid("registry requires a key"));
        }
        if let Some(revocations) = &authority.revocations {
            if revocations.key.is_none() {
                return Err(AgentConfigError::Invalid("revocations requires a key"));
            }
            if authority.issuers.is_empty() {
                return Err(AgentConfigError::Invalid(
                    "revocations require at least one grant issuer",
                ));
            }
            if matches!(revocations.interval_secs, Some(0)) {
                return Err(AgentConfigError::Invalid(
                    "revocations interval_secs must be positive",
                ));
            }
        }
        for secs in [
            self.timeouts.handshake_secs,
            self.timeouts.hello_secs,
            self.timeouts.authz_secs,
            self.timeouts.shutdown_secs,
        ]
        .into_iter()
        .flatten()
        {
            if secs == 0 || secs > MAX_TIMEOUT.as_secs() {
                return Err(AgentConfigError::Invalid(
                    "timeouts must be between 1 and 3600 seconds",
                ));
            }
        }
        Ok(())
    }

    /// Lower the validated document into typed values. The resolved set is
    /// explicit when a role, a service list or a disabled entry selects it;
    /// otherwise `None` preserves the implicit runtime set.
    pub fn resolve(&self) -> Result<ResolvedAgent, AgentConfigError> {
        let sync_dir = self.service.sync_dir.clone();
        let explicit: Option<BTreeSet<ServiceKind>> = if let Some(services) = &self.services {
            Some(services.iter().map(|s| s.kind()).collect())
        } else {
            self.role.map(|role| role.services())
        };
        let disabled: BTreeSet<ServiceKind> =
            self.disabled_services.iter().map(|s| s.kind()).collect();
        let services: Option<BTreeSet<ServiceKind>> = match (explicit, disabled.is_empty()) {
            (Some(set), _) => Some(set.difference(&disabled).copied().collect()),
            (None, false) => {
                let mut implicit = BTreeSet::from([ServiceKind::Tcp]);
                if cfg!(feature = "desktop") {
                    implicit.insert(ServiceKind::Desktop);
                }
                if sync_dir.is_some() {
                    implicit.insert(ServiceKind::Sync);
                }
                Some(implicit.difference(&disabled).copied().collect())
            }
            (None, true) => None,
        };
        // An explicit enabled set must name services this build and
        // configuration can actually serve.
        if let Some(set) = &services {
            if set.contains(&ServiceKind::Desktop) && !cfg!(feature = "desktop") {
                return Err(AgentConfigError::Invalid(
                    "desktop service requires the `desktop` build feature",
                ));
            }
            if set.contains(&ServiceKind::Sync) && sync_dir.is_none() {
                return Err(AgentConfigError::Invalid(
                    "sync service requires a configured sync directory",
                ));
            }
        }

        let ssh_target = self
            .service
            .ssh_target
            .as_deref()
            .map(|s| {
                TcpTarget::from_str(s)
                    .map_err(|_| AgentConfigError::Invalid("invalid ssh_target destination"))
            })
            .transpose()?;
        let mut tcp_targets = Vec::with_capacity(self.service.tcp_targets.len());
        for target in &self.service.tcp_targets {
            tcp_targets.push(
                TcpTarget::from_str(target)
                    .map_err(|_| AgentConfigError::Invalid("invalid tcp_targets destination"))?,
            );
        }
        let mut allow = Vec::with_capacity(self.peers.allow.len());
        for peer in &self.peers.allow {
            allow.push(
                EndpointId::from_str(peer)
                    .map_err(|_| AgentConfigError::Invalid("invalid peer endpoint id"))?,
            );
        }
        let mut issuers = Vec::with_capacity(self.authority.issuers.len());
        for issuer in &self.authority.issuers {
            issuers.push(decode_issuer(issuer)?);
        }
        let authority = &self.authority;
        let registry = authority.registry.as_ref();
        let revocations = authority.revocations.as_ref();
        let timeouts = if self.timeouts != TimeoutSettings::default() {
            let default = TimeoutPolicy::default();
            Some(TimeoutPolicy {
                handshake: self
                    .timeouts
                    .handshake_secs
                    .map_or(default.handshake, Duration::from_secs),
                hello: self
                    .timeouts
                    .hello_secs
                    .map_or(default.hello, Duration::from_secs),
                authz: self
                    .timeouts
                    .authz_secs
                    .map_or(default.authz, Duration::from_secs),
                shutdown: self
                    .timeouts
                    .shutdown_secs
                    .map_or(default.shutdown, Duration::from_secs),
            })
        } else {
            None
        };
        Ok(ResolvedAgent {
            services,
            ssh_target,
            tcp_targets,
            allow_any_tcp: self.service.allow_any_tcp,
            sync_dir,
            allow,
            issuers,
            grant_ttl: authority.grant_ttl_secs,
            tenant: authority.tenant.clone(),
            policy_min_revision: authority.policy_min_revision,
            directory: authority.directory.clone(),
            directory_ca: authority.directory_ca.clone(),
            record_ttl: authority.record_ttl_secs,
            record_state: authority.record_state.clone(),
            registry_key: registry.and_then(|r| r.key.clone()),
            registry_epoch: registry.and_then(|r| r.epoch),
            registry_state: registry.and_then(|r| r.state.clone()),
            registry_rotations: registry.map_or_else(Vec::new, |r| r.rotations.clone()),
            revocations_key: revocations.and_then(|r| r.key.clone()),
            revocations_epoch: revocations.and_then(|r| r.epoch),
            revocations_state: revocations.and_then(|r| r.state.clone()),
            revocations_rotations: revocations.map_or_else(Vec::new, |r| r.rotations.clone()),
            revocations_interval: revocations.and_then(|r| r.interval_secs),
            max_connections: self.limits.max_connections,
            max_streams: self.limits.max_streams,
            max_fds: self.limits.max_fds,
            max_rss_mb: self.limits.max_rss_mb,
            timeouts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn parse(json: &str) -> AgentSettings {
        AgentSettings::from_json(json.as_bytes()).expect("parse")
    }

    fn resolve_ok(json: &str) -> ResolvedAgent {
        let settings = parse(json);
        settings.validate().expect("validate");
        settings.resolve().expect("resolve")
    }

    #[test]
    fn version_and_unknown_fields_are_refused() {
        assert!(AgentSettings::from_json(br#"{"schema_version":2}"#).is_err());
        assert!(AgentSettings::from_json(br#"{"schema_version":1,"mystery":1}"#).is_err());
        assert!(AgentSettings::from_json(br#"{"role":"access"}"#).is_err());
        assert!(AgentSettings::from_json(b"[]").is_err());
    }

    #[test]
    fn role_and_services_are_mutually_exclusive() {
        let settings = parse(r#"{"schema_version":1,"role":"sync","services":["tcp"]}"#);
        assert!(settings.validate().is_err());
    }

    #[test]
    fn duplicate_and_reserved_services_rejected() {
        for json in [
            r#"{"schema_version":1,"services":["tcp","tcp"]}"#,
            r#"{"schema_version":1,"services":["audio"]}"#,
            r#"{"schema_version":1,"disabled_services":["audio"]}"#,
            r#"{"schema_version":1,"role":"desktop","disabled_services":["sync","sync"]}"#,
        ] {
            let settings = parse(json);
            assert!(settings.validate().is_err(), "{json}");
        }
    }

    #[test]
    fn roles_resolve_to_service_sets() {
        let check = |json: &str| resolve_ok(json).services.expect("explicit set");
        assert_eq!(
            check(r#"{"schema_version":1,"role":"access"}"#),
            BTreeSet::from([ServiceKind::Tcp])
        );
        assert_eq!(
            check(r#"{"schema_version":1,"role":"sync","service":{"sync_dir":"/tmp/x"}}"#),
            BTreeSet::from([ServiceKind::Sync])
        );
        // Sync without its directory refuses to resolve.
        assert!(
            parse(r#"{"schema_version":1,"services":["sync"]}"#)
                .resolve()
                .is_err()
        );
        // Nothing selected and nothing disabled keeps the implicit set.
        assert_eq!(resolve_ok(r#"{"schema_version":1}"#).services, None);
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn desktop_roles_resolve_on_desktop_builds() {
        let check = |json: &str| resolve_ok(json).services.expect("explicit set");
        assert_eq!(
            check(
                r#"{"schema_version":1,"role":"full","disabled_services":["sync"],
                 "service":{"sync_dir":"/tmp/x"}}"#,
            ),
            BTreeSet::from([ServiceKind::Tcp, ServiceKind::Desktop])
        );
        assert_eq!(
            check(r#"{"schema_version":1,"services":["desktop","tcp"]}"#),
            BTreeSet::from([ServiceKind::Desktop, ServiceKind::Tcp])
        );
    }

    #[cfg(not(feature = "desktop"))]
    #[test]
    fn desktop_roles_refuse_on_non_desktop_builds() {
        for json in [
            r#"{"schema_version":1,"role":"desktop"}"#,
            r#"{"schema_version":1,"services":["desktop","tcp"]}"#,
            r#"{"schema_version":1,"role":"full","service":{"sync_dir":"/tmp/x"}}"#,
        ] {
            assert!(parse(json).resolve().is_err(), "{json}");
        }
    }

    #[test]
    fn disabled_carves_the_implicit_set() {
        // With no role/services, disabled entries make the set explicit:
        // implicit {tcp} on non-desktop builds; sync joins only when a
        // sync_dir is configured.
        let resolved = resolve_ok(
            r#"{"schema_version":1,"disabled_services":["tcp"],"service":{"sync_dir":"/tmp/x"}}"#,
        );
        let set = resolved.services.expect("explicit after disable");
        assert!(!set.contains(&ServiceKind::Tcp));
        assert!(set.contains(&ServiceKind::Sync));
    }

    #[test]
    fn authority_requires_directory_and_issuers() {
        let bad = [
            r#"{"schema_version":1,"authority":{"revocations":{"key":"ab"}}}"#,
            r#"{"schema_version":1,"authority":{"directory":"http://localhost:9","revocations":{"key":"ab"}}}"#,
            r#"{"schema_version":1,"authority":{"registry":{}}}"#,
            r#"{"schema_version":1,"authority":{"directory_ca":"/ca.pem"}}"#,
            r#"{"schema_version":1,"authority":{"record_ttl_secs":30}}"#,
        ];
        for json in bad {
            let settings = parse(json);
            assert!(settings.validate().is_err(), "{json}");
        }
        // A complete revocation authority section validates.
        let ok = parse(
            r#"{"schema_version":1,"authority":{"directory":"http://localhost:9",
             "issuers":["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
             "revocations":{"key":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}}"#,
        );
        ok.validate().expect("complete authority validates");
        let resolved = ok.resolve().expect("resolve");
        assert_eq!(resolved.issuers.len(), 1);
        assert_eq!(
            resolved.revocations_key.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn bounds_are_enforced() {
        let bad = [
            r#"{"schema_version":1,"timeouts":{"handshake_secs":0}}"#,
            r#"{"schema_version":1,"timeouts":{"hello_secs":3601}}"#,
            r#"{"schema_version":1,"timeouts":{"authz_secs":0}}"#,
            r#"{"schema_version":1,"timeouts":{"shutdown_secs":3601}}"#,
            r#"{"schema_version":1,"authority":{"grant_ttl_secs":0}}"#,
            r#"{"schema_version":1,"authority":{"grant_ttl_secs":86401}}"#,
            r#"{"schema_version":1,"authority":{"directory":"http://localhost:9","issuers":["a"],"revocations":{"key":"k","interval_secs":0}}}"#,
        ];
        for json in bad {
            let settings = parse(json);
            assert!(settings.validate().is_err(), "{json}");
        }
        // Zero limits decode as a JSON error through NonZero.
        assert!(
            AgentSettings::from_json(br#"{"schema_version":1,"limits":{"max_streams":0}}"#)
                .is_err()
        );
        assert!(
            AgentSettings::from_json(br#"{"schema_version":1,"limits":{"max_fds":0}}"#).is_err()
        );
        assert!(
            AgentSettings::from_json(br#"{"schema_version":1,"limits":{"max_rss_mb":0}}"#).is_err()
        );
    }

    #[test]
    fn resource_budgets_merge_and_resolve() {
        let resolved =
            resolve_ok(r#"{"schema_version":1,"limits":{"max_fds":512,"max_rss_mb":256}}"#);
        assert_eq!(resolved.max_fds.unwrap().get(), 512);
        assert_eq!(resolved.max_rss_mb.unwrap().get(), 256);

        // Flag values replace file values per-field.
        let settings =
            parse(r#"{"schema_version":1,"limits":{"max_fds":128}}"#).apply(AgentOverrides {
                max_fds: NonZeroU64::new(256),
                max_rss_mb: NonZeroU64::new(64),
                ..Default::default()
            });
        let resolved = settings.resolve().unwrap();
        assert_eq!(resolved.max_fds.unwrap().get(), 256);
        assert_eq!(resolved.max_rss_mb.unwrap().get(), 64);

        // Absent everywhere resolves to no process gate.
        let resolved = resolve_ok(r#"{"schema_version":1}"#);
        assert!(resolved.max_fds.is_none());
        assert!(resolved.max_rss_mb.is_none());
    }

    #[test]
    fn typed_parsing_happens_in_resolve() {
        for json in [
            r#"{"schema_version":1,"service":{"ssh_target":"host:0"}}"#,
            r#"{"schema_version":1,"service":{"tcp_targets":[":22"]}}"#,
            r#"{"schema_version":1,"peers":{"allow":["not-an-id"]}}"#,
            r#"{"schema_version":1,"authority":{"directory":"http://localhost:9","issuers":["not-base32"],"revocations":{"key":"k"}}}"#,
        ] {
            let settings = parse(json);
            settings.validate().expect("structure is valid");
            assert!(settings.resolve().is_err(), "{json}");
        }
        let resolved = resolve_ok(
            r#"{"schema_version":1,"service":{"ssh_target":"10.0.0.7:2222",
             "tcp_targets":["127.0.0.1:8080"],"allow_any_tcp":true}}"#,
        );
        assert_eq!(resolved.ssh_target.unwrap().to_string(), "10.0.0.7:2222");
        assert_eq!(resolved.tcp_targets.len(), 1);
        assert!(resolved.allow_any_tcp);
    }

    #[test]
    fn flag_overrides_merge_over_file() {
        // An explicit --service list replaces both file role and list.
        let settings = parse(r#"{"schema_version":1,"role":"sync"}"#).apply(AgentOverrides {
            services: vec![ServiceName::Tcp],
            ..Default::default()
        });
        let resolved = settings.resolve().unwrap();
        assert_eq!(resolved.services, Some(BTreeSet::from([ServiceKind::Tcp])));

        // --no-service replaces the file's whole disabled list.
        let settings = parse(
            r#"{"schema_version":1,"role":"full","disabled_services":["desktop"]}"#,
        )
        .apply(AgentOverrides {
            no_services: vec![ServiceName::Sync],
            ..Default::default()
        });
        assert_eq!(settings.disabled_services, vec![ServiceName::Sync]);

        // Scalar flags only apply when present; bools are additive-true.
        let settings = parse(
            r#"{"schema_version":1,"service":{"ssh_target":"1.2.3.4:22","allow_any_tcp":false},
             "authority":{"grant_ttl_secs":60},"timeouts":{"hello_secs":5}}"#,
        )
        .apply(AgentOverrides {
            grant_ttl: Some(120),
            ..Default::default()
        });
        let resolved = settings.resolve().unwrap();
        assert_eq!(resolved.ssh_target.unwrap().to_string(), "1.2.3.4:22");
        assert!(!resolved.allow_any_tcp);
        assert_eq!(resolved.grant_ttl, Some(120));
        assert_eq!(resolved.timeouts.unwrap().hello, Duration::from_secs(5));

        // A scalar flag beats the file value.
        let settings = parse(r#"{"schema_version":1,"service":{"ssh_target":"1.2.3.4:22"}}"#)
            .apply(AgentOverrides {
                ssh: Some("9.9.9.9:22".parse().unwrap()),
                ..Default::default()
            });
        assert_eq!(
            settings.resolve().unwrap().ssh_target.unwrap().to_string(),
            "9.9.9.9:22"
        );
    }

    #[test]
    fn nested_authority_flags_merge_per_field() {
        // A flag registry key preserves the file's registry state path —
        // the same partial-merge convention owned-relay limits use.
        let settings = parse(
            r#"{"schema_version":1,"authority":{"directory":"http://localhost:9",
             "registry":{"key":"old","state":"/tmp/reg-state"}}}"#,
        )
        .apply(AgentOverrides {
            registry_key: Some("new".into()),
            ..Default::default()
        });
        let resolved = settings.resolve().unwrap();
        assert_eq!(resolved.registry_key.as_deref(), Some("new"));
        assert_eq!(
            resolved.registry_state.as_deref(),
            Some(Path::new("/tmp/reg-state"))
        );
    }

    #[test]
    fn timeouts_resolve_to_policy() {
        let resolved =
            resolve_ok(r#"{"schema_version":1,"timeouts":{"handshake_secs":9,"hello_secs":3}}"#);
        let policy = resolved.timeouts.unwrap();
        assert_eq!(policy.handshake, Duration::from_secs(9));
        assert_eq!(policy.hello, Duration::from_secs(3));
        // Partial timeout config preserves the other default.
        let resolved = resolve_ok(r#"{"schema_version":1,"timeouts":{"hello_secs":3}}"#);
        let policy = resolved.timeouts.unwrap();
        assert_eq!(policy.handshake, TimeoutPolicy::default().handshake);
        assert_eq!(policy.authz, TimeoutPolicy::default().authz);
        assert_eq!(policy.shutdown, TimeoutPolicy::default().shutdown);
        // The reply/shutdown classes resolve identically.
        let resolved =
            resolve_ok(r#"{"schema_version":1,"timeouts":{"authz_secs":7,"shutdown_secs":2}}"#);
        let policy = resolved.timeouts.unwrap();
        assert_eq!(policy.authz, Duration::from_secs(7));
        assert_eq!(policy.shutdown, Duration::from_secs(2));
        assert_eq!(policy.handshake, TimeoutPolicy::default().handshake);
        assert_eq!(resolve_ok(r#"{"schema_version":1}"#).timeouts, None);
    }

    #[test]
    fn timeout_flags_override_file() {
        let settings =
            parse(r#"{"schema_version":1,"timeouts":{"authz_secs":3}}"#).apply(AgentOverrides {
                authz_timeout: Some(11),
                shutdown_timeout: Some(4),
                ..Default::default()
            });
        let policy = settings.resolve().unwrap().timeouts.unwrap();
        assert_eq!(policy.authz, Duration::from_secs(11));
        assert_eq!(policy.shutdown, Duration::from_secs(4));
    }

    #[test]
    fn tenant_and_revision_binding() {
        // Binding claims without issuers would never evaluate — refused
        // loudly rather than silently inert.
        for json in [
            r#"{"schema_version":1,"authority":{"tenant":"t1"}}"#,
            r#"{"schema_version":1,"authority":{"policy_min_revision":4}}"#,
            r#"{"schema_version":1,"authority":{"tenant":"","issuers":["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],"directory":"http://localhost:9","revocations":{"key":"k"}}}"#,
            r#"{"schema_version":1,"authority":{"tenant":"has space","issuers":["a"],"revocations":{"key":"k"}}}"#,
        ] {
            let settings = parse(json);
            assert!(settings.validate().is_err(), "{json}");
        }
        // A valid binding resolves, and flag values override file values.
        let resolved = resolve_ok(
            r#"{"schema_version":1,"authority":{"issuers":["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
             "tenant":"tenant-a","policy_min_revision":3,"directory":"http://localhost:9",
             "revocations":{"key":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}}"#,
        );
        assert_eq!(resolved.tenant.as_deref(), Some("tenant-a"));
        assert_eq!(resolved.policy_min_revision, Some(3));
        let settings = parse(
            r#"{"schema_version":1,"authority":{"issuers":["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
             "tenant":"tenant-a","policy_min_revision":3,"directory":"http://localhost:9",
             "revocations":{"key":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}}"#,
        )
        .apply(AgentOverrides {
            tenant: Some("tenant-b".into()),
            ..Default::default()
        });
        assert_eq!(
            settings.resolve().unwrap().tenant.as_deref(),
            Some("tenant-b")
        );
    }
}
