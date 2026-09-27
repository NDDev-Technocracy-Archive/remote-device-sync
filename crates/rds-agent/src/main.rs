//! `rds-agent`: daemon on a controlled device.

use clap::Parser;
use rds_agent::{
    Agent, AgentLimits, AgentOverrides, AgentPolicy, AgentSettings, Role, ServiceName,
};
use rds_net::{
    EndpointOverrides, EndpointSettings, Ticket, acquire_key, bind_endpoint, default_key_path,
};

#[derive(Parser)]
#[command(
    version,
    about = "RDS agent: serve SSH and desktop sessions to allowed peers"
)]
struct Cli {
    /// Local session directory; default is <key-file>.control.
    #[arg(long, conflicts_with = "no_control")]
    control_dir: Option<std::path::PathBuf>,
    /// Disable local session control (explicit server-only operation).
    #[arg(long, conflicts_with = "registry_key")]
    no_control: bool,
    #[command(flatten)]
    admin: rds_observe::admin::Args,
    /// Path to the endpoint secret key (created if missing).
    #[arg(long)]
    key_file: Option<std::path::PathBuf>,
    /// Custom relay URL; default is the n0 public relays. Repeatable —
    /// ≥2 relays give automatic client-side failover.
    #[arg(long, conflicts_with_all = ["owned_relay", "no_relay"])]
    relay: Vec<String>,
    /// Transport backend: `iroh` (default) or `noq` (with the
    /// `transport-noq` feature).
    #[arg(long)]
    backend: Option<rds_net::Backend>,
    /// Versioned endpoint JSON configuration. Explicit flags override it.
    #[arg(long)]
    endpoint_config: Option<std::path::PathBuf>,
    /// Key-pinned owned relay: rds-relay://PUBLIC_HEX_KEY@IP:PORT.
    /// Repeatable — each occurrence attaches a warm standby relay.
    #[arg(long, conflicts_with_all = ["relay", "no_relay"])]
    owned_relay: Vec<String>,
    /// Disable relay and public address lookup services.
    #[arg(long, conflicts_with_all = ["relay", "owned_relay"])]
    no_relay: bool,
    /// Local UDP bind address; repeatable on the owned backend.
    #[arg(long)]
    bind_address: Vec<std::net::SocketAddr>,
    /// Versioned agent JSON configuration: role, services, peers,
    /// authority, limits and timeouts. Explicit flags override it.
    #[arg(long)]
    agent_config: Option<std::path::PathBuf>,
    /// Deployment role preset: access | sync | desktop | full.
    /// Mutually exclusive with --service.
    #[arg(long, conflicts_with = "service")]
    role: Option<Role>,
    /// Data-plane service to serve: tcp | desktop | sync. Repeatable;
    /// replaces the implicit set and any configured role.
    #[arg(long = "service", conflicts_with = "role")]
    service: Vec<ServiceName>,
    /// Remove a service from the role/implicit set. Repeatable.
    #[arg(long = "no-service")]
    no_service: Vec<ServiceName>,
    /// Allowed peer EndpointId. Repeatable.
    #[arg(long = "allow")]
    allow: Vec<String>,
    /// SSH socket the TcpConnect service may reach (default 127.0.0.1:22).
    #[arg(long)]
    ssh: Option<rds_core::TcpTarget>,
    /// Permit TcpConnect to any host:port (development only).
    #[arg(long)]
    allow_any_tcp: bool,
    /// Pending handshakes and admitted connections; positive 16-bit limit.
    #[arg(long)]
    max_connections: Option<std::num::NonZeroU16>,
    /// Concurrent tasks per connection (default 64). Grant mode needs >=2;
    /// one slot is reserved from service bodies for authorization/renewal.
    #[arg(long)]
    max_streams: Option<std::num::NonZeroU16>,
    /// Inbound connection handshake deadline in seconds (1..=3600).
    #[arg(long)]
    handshake_timeout: Option<u64>,
    /// Per-stream greeting read deadline in seconds (1..=3600).
    #[arg(long)]
    hello_timeout: Option<u64>,
    /// Directory HTTP(S) origin or legacy IP:port; the agent publishes its
    /// signed record and keeps it fresh.
    #[arg(long)]
    directory: Option<String>,
    /// PEM CA bundle for directory HTTPS; replaces the public root store.
    #[arg(long)]
    directory_ca: Option<std::path::PathBuf>,
    /// Trusted registry authority for local-manager device-name resolution.
    #[arg(long)]
    registry_key: Option<String>,
    /// Bootstrap registry authority epoch (default 1).
    #[arg(long)]
    registry_epoch: Option<u64>,
    /// Durable name-trust state; default is beside --key-file.
    #[arg(long)]
    registry_state: Option<std::path::PathBuf>,
    /// Registry authority rotation receipt; repeat in epoch order.
    #[arg(long)]
    registry_rotation: Vec<std::path::PathBuf>,
    /// Record TTL when `--directory` is set (default 300).
    #[arg(long)]
    record_ttl: Option<u64>,
    /// Private durable publisher state; default is beside --key-file.
    #[arg(long)]
    record_state: Option<std::path::PathBuf>,
    /// Trusted grant issuer (base32 verifying key). Repeatable. When
    /// set, every connection must present a valid estate-signed grant
    /// before any service stream opens.
    #[arg(long = "issuer")]
    issuers: Vec<String>,
    /// Maximum grant lifetime accepted, in seconds (default 300).
    #[arg(long)]
    grant_ttl: Option<u64>,
    /// Verifying key that signs the estate revocation snapshot
    /// (`GET /v1/revocations`). Required for denylist polling when
    /// `--directory` is set.
    #[arg(long)]
    revocations_key: Option<String>,
    /// Epoch of the independently provisioned bootstrap revocation
    /// authority (default 1).
    #[arg(long)]
    revocations_epoch: Option<u64>,
    /// Private durable policy directory; default is beside --key-file.
    #[arg(long)]
    revocations_state: Option<std::path::PathBuf>,
    /// Dual-signed authority rotation receipt. Repeat in epoch order.
    #[arg(long)]
    authority_rotation: Vec<std::path::PathBuf>,
    /// Revocation poll interval in seconds (default 30).
    #[arg(long)]
    revocations_interval: Option<u64>,
    /// Directory the Sync service may read/write under.
    #[arg(long)]
    sync_dir: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    rds_observe::run_main(rds_observe::Service::Agent, "info", run(cli)).await
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let prepared_admin = cli.admin.bind().await?;
    let mut config = cli
        .endpoint_config
        .as_deref()
        .map(EndpointSettings::load)
        .transpose()?
        .unwrap_or_default()
        .apply(EndpointOverrides {
            backend: cli.backend,
            bind_addrs: cli.bind_address,
            relays: cli.relay,
            owned_relay: cli.owned_relay,
            no_relay: cli.no_relay,
        })?
        .into_endpoint()?;

    // Role/service/authority policy resolves in the same preflight window:
    // before an identity is created or a socket bound.
    let merged = cli
        .agent_config
        .as_deref()
        .map(AgentSettings::load)
        .transpose()?
        .unwrap_or_default()
        .apply(AgentOverrides {
            role: cli.role,
            services: cli.service,
            no_services: cli.no_service,
            ssh: cli.ssh,
            allow_any_tcp: cli.allow_any_tcp,
            sync_dir: cli.sync_dir,
            allow: cli.allow,
            issuers: cli.issuers,
            grant_ttl: cli.grant_ttl,
            directory: cli.directory,
            directory_ca: cli.directory_ca,
            record_ttl: cli.record_ttl,
            record_state: cli.record_state,
            registry_key: cli.registry_key,
            registry_epoch: cli.registry_epoch,
            registry_state: cli.registry_state,
            registry_rotations: cli.registry_rotation,
            revocations_key: cli.revocations_key,
            revocations_epoch: cli.revocations_epoch,
            revocations_state: cli.revocations_state,
            revocations_rotations: cli.authority_rotation,
            revocations_interval: cli.revocations_interval,
            max_connections: cli.max_connections,
            max_streams: cli.max_streams,
            handshake_timeout: cli.handshake_timeout,
            hello_timeout: cli.hello_timeout,
        });
    merged.validate()?;
    // Budget and authority cross-checks run on the merged document before
    // any string is decoded, matching the previous flag-only order.
    let max_streams = merged
        .limits
        .max_streams
        .unwrap_or(std::num::NonZeroU16::new(64).expect("positive limit"));
    anyhow::ensure!(
        merged.authority.issuers.is_empty() || max_streams.get() >= 2,
        "grant mode requires --max-streams at least 2"
    );
    if !merged.authority.issuers.is_empty() && merged.authority.revocations.is_none() {
        anyhow::bail!("managed grants require --directory and --revocations-key");
    }
    let resolved = merged.resolve()?;

    let ssh_target = resolved
        .ssh_target
        .unwrap_or_else(|| "127.0.0.1:22".parse().expect("static target parses"));
    let (ssh_host, ssh_port) = ssh_target.into_parts();

    let mut policy = AgentPolicy::ssh_only((ssh_host, ssh_port));
    for target in &resolved.tcp_targets {
        let (host, port) = target.clone().into_parts();
        policy.tcp_targets.insert((host, port));
    }
    for id in &resolved.allow {
        policy.allow.insert(*id);
    }
    policy.allow_any_tcp = resolved.allow_any_tcp;
    policy.grant_max_ttl = std::time::Duration::from_secs(resolved.grant_ttl.unwrap_or(300));
    policy.sync_dir = resolved.sync_dir;
    policy.services = resolved.services;
    if let Some(timeouts) = resolved.timeouts {
        policy.timeouts = timeouts;
    }
    policy.issuers.extend(resolved.issuers.iter().copied());
    policy
        .validate()
        .map_err(|why| anyhow::anyhow!("invalid agent policy: {why}"))?;

    let key_path = cli
        .key_file
        .or_else(default_key_path)
        .ok_or_else(|| anyhow::anyhow!("no --key-file and no config dir"))?;
    let prepared_control = if cli.no_control {
        None
    } else {
        let path = match cli.control_dir {
            Some(path) => path,
            None => rds_client::local::control_dir_for_key(&key_path)?,
        };
        Some(rds_client::local::Prepared::bind(path).await?)
    };
    let key_load_path = key_path.clone();
    // Retained through endpoint and service shutdown, including startup errors.
    let identity = tokio::task::spawn_blocking(move || acquire_key(&key_load_path)).await??;
    let secret_key = identity.secret_key().clone();

    config.secret_key = Some(secret_key.clone());

    let mut directory = resolved
        .directory
        .as_deref()
        .map(|origin| -> anyhow::Result<_> {
            let mut client = rds_discovery::client::Client::from_endpoint(origin)?;
            if let Some(path) = &resolved.directory_ca {
                client = client.with_ca_pem(&std::fs::read(path)?)?;
            }
            Ok(client)
        })
        .transpose()?;
    if let Some(key) = resolved.registry_key {
        let client = directory
            .take()
            .ok_or_else(|| anyhow::anyhow!("registry trust requires --directory"))?;
        let path = resolved
            .registry_state
            .unwrap_or_else(|| key_path.with_extension("registry-state"));
        let epoch = resolved.registry_epoch.unwrap_or(1);
        let rotations = resolved.registry_rotations;
        directory = Some(
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let authority = rds_discovery::authority::Authority::from_base32(&key, epoch)?;
                let rotations = rds_discovery::policy::read_rotations(&rotations)?;
                Ok(client.with_registry_store(authority, path, rotations)?)
            })
            .await??,
        );
    }
    let policy_handle = std::sync::Arc::new(policy.clone());
    let _revocations = if let (Some(client), Some(key)) =
        (directory.clone(), resolved.revocations_key.as_deref())
    {
        let authority = rds_discovery::authority::Authority::from_base32(
            key,
            resolved.revocations_epoch.unwrap_or(1),
        )?;
        let path = resolved
            .revocations_state
            .unwrap_or_else(|| key_path.with_extension("revocations-state"));
        let rotations = resolved.revocations_rotations;
        let store =
            tokio::task::spawn_blocking(move || -> Result<_, rds_discovery::DiscoveryError> {
                let now = rds_discovery::clock::Reading::now()?;
                let mut store = rds_discovery::policy::PolicyStore::open(&path, authority, now)?;
                for receipt in rds_discovery::policy::read_rotations(&rotations)? {
                    store.apply_rotation(&receipt, now)?;
                }
                Ok(store)
            })
            .await??;
        Some(rds_agent::watch_revocations(
            client,
            store,
            policy_handle,
            std::time::Duration::from_secs(resolved.revocations_interval.unwrap_or(30)),
        )?)
    } else {
        None
    };

    let record_issuer = if directory.is_some() {
        let path = resolved
            .record_state
            .unwrap_or_else(|| key_path.with_extension("publisher-state"));
        let key = ed25519_dalek::SigningKey::from_bytes(&secret_key.to_bytes());
        Some(
            tokio::task::spawn_blocking(move || {
                rds_discovery::publisher::RecordIssuer::open(&path, key, rds_discovery::now_unix()?)
            })
            .await??,
        )
    } else {
        None
    };
    let endpoint = bind_endpoint(config).await?;
    // Binding starts local service. iroh's online() waits indefinitely for a
    // relay, including when relays are disabled or unreachable. Reachability
    // develops independently; the announcer publishes address changes.

    let mut announce = if let (Some(client), Some(issuer)) = (directory.clone(), record_issuer) {
        // The directory record advertises what the policy actually serves:
        // Ping is the always-on liveness beacon; data-plane services map
        // from the effective set and only when usable in this binary.
        let services = policy
            .effective_services()
            .iter()
            .filter_map(|kind| match kind {
                rds_core::ServiceKind::Ping => Some(rds_discovery::Service::Ping),
                rds_core::ServiceKind::Tcp => Some(rds_discovery::Service::TcpForward),
                rds_core::ServiceKind::Desktop if cfg!(feature = "desktop") => {
                    Some(rds_discovery::Service::Desktop)
                }
                rds_core::ServiceKind::Sync => Some(rds_discovery::Service::Sync),
                _ => None,
            })
            .collect();
        let announced = rds_net::announce(
            endpoint.clone(),
            rds_net::AnnounceConfig {
                issuer,
                directory: client,
                services,
                ttl: std::time::Duration::from_secs(resolved.record_ttl.unwrap_or(300)),
            },
        );
        match announced {
            Ok(task) => Some(task),
            Err(error) => {
                endpoint.close().await;
                return Err(error.into());
            }
        }
    } else {
        None
    };

    let agent = std::sync::Arc::new(
        Agent::new(endpoint, policy).with_limits(AgentLimits::new(
            resolved
                .max_connections
                .unwrap_or(std::num::NonZeroU16::new(32).expect("positive limit")),
            max_streams,
        )),
    );
    let metrics = agent.metrics();
    let mut control =
        rds_client::local::Server::start(prepared_control, agent.endpoint.clone(), directory);
    let control_metrics = control.take_observer();
    let mut admin = rds_observe::admin::Server::start(prepared_admin, move || {
        let mut snapshot = metrics.snapshot();
        if let Some(control) = &control_metrics {
            snapshot.extend(control.snapshot());
        }
        snapshot
    });
    if let Some(addr) = admin.addr() {
        tracing::info!(%addr, "admin metrics listening");
    }
    println!("endpoint id: {}", agent.id());
    println!("ticket: {}", Ticket::of(&agent.endpoint));
    if agent.policy.allow.is_empty() {
        eprintln!("warning: empty --allow list; every peer will be rejected");
    }
    let result = tokio::select! {
        res = agent.run() => res,
        res = control.stopped() => {
            res.map_err(anyhow::Error::from).and_then(|()| Err(anyhow::anyhow!("local session manager stopped unexpectedly")))
        },
        res = admin.stopped() => {
            res.map_err(anyhow::Error::from).and_then(|()| Err(anyhow::anyhow!("admin metrics stopped unexpectedly")))
        },
        res = async {
            match &mut announce {
                Some(task) => task.wait().await,
                None => std::future::pending().await,
            }
        } => res.map_err(Into::into),
        _ = shutdown_signal() => Ok(()),
    };
    drop(announce);
    // Close the endpoint so peers get CONNECTION_CLOSE instead of an
    // abrupt socket death (and iroh does not log an ungraceful drop).
    let ((), admin_result, control_result) =
        tokio::join!(agent.endpoint.close(), admin.close(), control.close());
    let result = finish_admin(result, admin_result);
    match (result, control_result) {
        (result, Ok(())) => result,
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(control)) => {
            Err(error.context(format!("local manager shutdown also failed: {control}")))
        }
    }
}

/// SIGINT on every platform, SIGTERM on unix (systemd stop).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn finish_admin(
    product: anyhow::Result<()>,
    admin: Result<(), rds_observe::admin::Error>,
) -> anyhow::Result<()> {
    match (product, admin) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(admin)) => {
            Err(error.context(format!("admin shutdown also failed: {admin}")))
        }
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn tcp_target_flags_reject_invalid_destinations() {
        for target in [":22", "host:0", "host:nope", "::1:22", "[::1]22"] {
            assert!(
                Cli::try_parse_from(["rds-agent", "--ssh", target]).is_err(),
                "{target}"
            );
        }
    }
}
