//! `rds`: operator CLI for remote device access.

use std::net::SocketAddr;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use rds_net::{
    EndpointOverrides, EndpointSettings, Ticket, acquire_key, bind_endpoint, default_key_path,
    load_or_create_key,
};
use tracing::Instrument;

use rds_cli::desktop;
#[cfg(unix)]
use rds_cli::logging;
mod managed;
mod ssh;

#[derive(Parser)]
#[command(
    name = "rds",
    version,
    about = "NDDev Remote connection and automation CLI"
)]
struct Cli {
    /// New private log file (8 MiB cap); required for SSH with JSON telemetry.
    #[arg(long, global = true)]
    log_file: Option<std::path::PathBuf>,
    /// Use a separate endpoint explicitly; its identity must not be in use.
    #[arg(long, global = true, conflicts_with = "control_dir")]
    direct: bool,
    /// Agent control directory; default is beside the default endpoint key.
    #[arg(long, global = true)]
    control_dir: Option<std::path::PathBuf>,
    /// Path to the endpoint secret key.
    #[arg(long, global = true)]
    key_file: Option<std::path::PathBuf>,
    /// Custom relay URL; defaults to n0 public relays. Repeatable.
    /// Fallback requires ready paths known to both peers.
    #[arg(long, global = true, conflicts_with_all = ["owned_relay", "no_relay"])]
    relay: Vec<String>,
    /// Transport backend: `iroh` (default) or `noq` (with the
    /// `transport-noq` feature).
    #[arg(long, global = true)]
    backend: Option<rds_net::Backend>,
    /// Versioned endpoint JSON configuration. Explicit flags override it.
    #[arg(long, global = true)]
    endpoint_config: Option<std::path::PathBuf>,
    /// Key-pinned owned relay: rds-relay://PUBLIC_HEX_KEY@IP:PORT.
    /// Repeatable — each occurrence attaches a warm standby relay.
    #[arg(long, global = true, conflicts_with_all = ["relay", "no_relay"])]
    owned_relay: Vec<String>,
    /// Disable relay and public address lookup services.
    #[arg(long, global = true, conflicts_with_all = ["relay", "owned_relay"])]
    no_relay: bool,
    /// Local UDP bind address; repeatable on the owned backend.
    #[arg(long, global = true)]
    bind_address: Vec<SocketAddr>,
    /// Directory HTTP(S) origin or legacy IP:port. Enables bare-key lookups and GDS
    /// device-name resolution; tickets still work without it.
    #[arg(long, global = true)]
    server: Option<String>,
    /// PEM CA bundle for directory HTTPS; replaces the public root store.
    #[arg(long, global = true, requires = "server")]
    directory_ca: Option<std::path::PathBuf>,
    /// Trusted registry verifying key (base32), provisioned by GDS.
    /// Required for device names; tickets and pinned keys are independent.
    #[arg(long, global = true, requires = "server")]
    registry_key: Option<String>,
    /// Bootstrap registry authority epoch (default 1).
    #[arg(long, global = true, requires = "registry_key")]
    registry_epoch: Option<std::num::NonZeroU64>,
    /// Private name-trust state directory; default is beside the endpoint key.
    #[arg(long, global = true, requires = "registry_key")]
    registry_state: Option<std::path::PathBuf>,
    /// Dual-signed authority rotation receipt. Repeat in epoch order.
    #[arg(long, global = true, requires = "registry_key")]
    authority_rotation: Vec<std::path::PathBuf>,
    /// Capability grant file (JSON `Grant` as minted by the estate).
    /// Required when the target agent runs in grant mode.
    #[arg(long, global = true)]
    grant: Option<std::path::PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage connections through the local agent; no key loading or UDP bind.
    Session(managed::Options),
    /// Create a new private admin scrape credential without endpoint initialization.
    AdminToken {
        #[arg(long)]
        file: std::path::PathBuf,
    },
    /// Print this device's endpoint id.
    Id,
    /// Print this device's dialable ticket.
    Ticket,
    /// Measure round-trip latency to a peer.
    Ping {
        /// Peer ticket or bare endpoint id.
        target: String,
        /// Number of probes.
        #[arg(short = 'c', long, default_value = "4")]
        count: u32,
    },
    /// Print peer metadata (version, services, displays).
    Info { target: String },
    /// Open a native SSH terminal or execute a command through the agent.
    Ssh {
        target: String,
        #[command(flatten)]
        options: ssh::Options,
    },
    /// Forward a local port to an arbitrary peer-side TCP target.
    Forward {
        target: String,
        #[arg(short = 'L', long, default_value = "127.0.0.1:0")]
        bind: SocketAddr,
        /// Remote target host:port.
        #[arg(long)]
        remote: rds_core::TcpTarget,
        /// Maximum simultaneous local forwarding workers (positive 16-bit).
        #[arg(long, default_value = "64")]
        max_connections: std::num::NonZeroU16,
    },
    /// Open a remote desktop session (requires the `desktop` feature).
    Desktop {
        target: String,
        #[command(flatten)]
        options: desktop::Options,
    },
    /// Push a file into the peer's sync directory (resumable).
    Send {
        target: String,
        /// Local file to send.
        path: std::path::PathBuf,
    },
    /// Pull a file from the peer's sync directory into `dir`.
    Recv {
        target: String,
        /// Relative path inside the peer's sync directory.
        rel_path: String,
        /// Local directory to receive into.
        #[arg(long, default_value = ".")]
        dir: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    use std::io::IsTerminal as _;
    let cli = Cli::parse();
    let ssh_command = match &cli.command {
        Command::Ssh { .. } => true,
        Command::Session(options) => options.is_ssh(),
        _ => false,
    };
    let terminal_ssh = std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && ssh_command
        && cli.log_file.is_none();
    let setup = async {
        let config = rds_observe::Config::from_env("warn")?;
        anyhow::ensure!(
            !ssh_command || config.format() != rds_observe::Format::Json || cli.log_file.is_some(),
            "SSH JSON telemetry requires --log-file in a private directory; remote stderr must not enter the log collector"
        );
        if let Some(path) = cli.log_file.clone() {
            let file =
                tokio::task::spawn_blocking(move || logging::PrivateLog::create(&path)).await??;
            Ok::<_, anyhow::Error>(rds_observe::install_with_writer(
                rds_observe::Service::Cli,
                config,
                file,
            )?)
        } else {
            Ok(rds_observe::install(rds_observe::Service::Cli, config)?)
        }
    };
    let telemetry = match setup.await {
        Ok(telemetry) => telemetry,
        Err(error) => {
            eprintln!("could not initialize RDS logging: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let pause = if terminal_ssh {
        match telemetry.pause_console().await {
            Ok(guard) => Some(guard),
            Err(error) => return telemetry.finish(Err(error)),
        }
    } else {
        None
    };
    let mut remote_status = 0;
    let result = telemetry
        .run(async {
            match run(cli).await {
                Err(error) if error.is::<ssh::ExitStatus>() => {
                    remote_status = error.downcast_ref::<ssh::ExitStatus>().map_or(255, |e| e.0);
                    Ok(())
                }
                result => result,
            }
        })
        .await;
    drop(pause);
    let status = telemetry.finish(result);
    if status == std::process::ExitCode::SUCCESS {
        std::process::ExitCode::from(remote_status)
    } else {
        status
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    if matches!(cli.command, Command::Session(_)) {
        validate_managed(&cli)?;
        anyhow::ensure!(
            !cli.direct && cli.grant.is_none(),
            "session commands use agent configuration; omit --direct and use connect --grant-file for credentials"
        );
        let directory = control_directory(&cli)?;
        if let Command::Session(options) = cli.command {
            return managed::run(options, directory).await;
        }
    }
    if let Command::AdminToken { file } = &cli.command {
        let file = file.clone();
        tokio::task::spawn_blocking(move || rds_observe::admin::Token::create(&file)).await??;
        return Ok(());
    }
    if !cli.direct && !matches!(cli.command, Command::Id) {
        validate_managed(&cli)?;
        let directory = control_directory(&cli)?;
        return managed::run_default(cli.command, directory, cli.grant).await;
    }
    anyhow::ensure!(
        cli.control_dir.is_none(),
        "--control-dir applies to managed commands; use session list --json to inspect the running identity"
    );
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

    let key_path = cli
        .key_file
        .clone()
        .or_else(default_key_path)
        .ok_or_else(|| {
            anyhow::anyhow!("no endpoint key: pass --key-file or set HOME/XDG_CONFIG_HOME")
        })?;
    // `id` is a pure function of the key: no socket, no relay contact.
    // (An unresolvable key previously fell back to an ephemeral identity,
    // printing a different id on every run.)
    if let Command::Id = cli.command {
        let key = tokio::task::spawn_blocking(move || load_or_create_key(&key_path)).await??;
        println!("{}", key.public());
        return Ok(());
    }
    let identity = tokio::task::spawn_blocking(move || acquire_key(&key_path)).await??;
    config.secret_key = Some(identity.secret_key().clone());
    let directory = cli
        .server
        .as_deref()
        .map(|origin| -> anyhow::Result<_> {
            let mut client = rds_discovery::client::Client::from_endpoint(origin)?;
            if let Some(path) = &cli.directory_ca {
                client = client.with_ca_pem(&read_pem_file(path)?)?;
            }
            if let Some(key) = cli.registry_key.as_deref() {
                let authority = rds_discovery::authority::Authority::from_base32(
                    key,
                    cli.registry_epoch.map_or(1, std::num::NonZeroU64::get),
                )?;
                let path = cli
                    .registry_state
                    .clone()
                    .or_else(|| {
                        cli.key_file
                            .clone()
                            .or_else(default_key_path)
                            .map(|key| key.with_extension("registry-state"))
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "name resolution requires --registry-state or a key/config directory"
                        )
                    })?;
                let rotations = rds_discovery::policy::read_rotations(&cli.authority_rotation)?;
                client = client.with_registry_store(authority, path, rotations)?;
            }
            Ok(client)
        })
        .transpose()?;
    let grant = managed::read_grant(cli.grant).await?.map(|grant| *grant);
    let endpoint = bind_endpoint(config).await?;
    let result = async {
        match cli.command {
            Command::Session(_) => {
                unreachable!("local session handled before endpoint initialization")
            }
            Command::AdminToken { .. } => {
                unreachable!("admin token handled before endpoint initialization")
            }
            Command::Id => unreachable!("handled before endpoint bind"),
            Command::Ticket => {
                // Bound the wait: an unreachable relay must not hang the
                // command; the ticket still carries any resolved addresses.
                if tokio::time::timeout(std::time::Duration::from_secs(15), endpoint.online())
                    .await
                    .is_err()
                {
                    eprintln!(
                        "warning: relay unreachable after 15s; ticket may lack a relay address"
                    );
                }
                println!("{}", Ticket::of(&endpoint));
            }
            Command::Ping { target, count } => {
                let conn = dial(
                    &endpoint,
                    resolve(&directory, &target).await?,
                    grant.clone(),
                )
                .await?;
                println!("connected to {}", conn.remote_id());
                for i in 0..count {
                    let rtt = rds_cli::ping(&conn, i as u64)
                        .instrument(conn.session.span())
                        .await?;
                    println!("pong seq={i} rtt={:.1}ms", rtt.as_secs_f64() * 1000.0);
                }
                let snapshot = conn.path_stats_snapshot();
                println!("path coverage={:?}", snapshot.coverage);
                for p in snapshot.paths {
                    println!(
                        "path id={} via={} selected={} rtt={:.1}ms sent={} lost={} cwnd={}",
                        p.path_id,
                        if p.via_relay { "relay" } else { "direct" },
                        p.selected,
                        p.rtt.as_secs_f64() * 1000.0,
                        p.sent,
                        p.lost,
                        p.cwnd,
                    );
                }
            }
            Command::Info { target } => {
                let conn = dial(
                    &endpoint,
                    resolve(&directory, &target).await?,
                    grant.clone(),
                )
                .await?;
                let info = rds_cli::info(&conn).instrument(conn.session.span()).await?;
                println!("{info:#?}");
            }
            Command::Ssh { target, options } => {
                let conn = dial(
                    &endpoint,
                    resolve(&directory, &target).await?,
                    grant.clone(),
                )
                .await?;
                ssh::direct(&conn, options)
                    .instrument(conn.session.span())
                    .await?;
            }
            Command::Forward {
                target,
                bind,
                remote,
                max_connections,
            } => {
                let session = dial(
                    &endpoint,
                    resolve(&directory, &target).await?,
                    grant.clone(),
                )
                .await?;
                let conn = Arc::new(session.conn.clone());
                let listener = tokio::net::TcpListener::bind(bind).await?;
                rds_cli::forward_bound_listener(conn, listener, remote, max_connections)
                    .instrument(session.session.span())
                    .await?;
            }
            Command::Desktop { target, options } => {
                desktop::direct(
                    &endpoint,
                    resolve(&directory, &target).await?,
                    grant.clone(),
                    options,
                )
                .await?;
            }
            Command::Send { target, path } => {
                let conn = dial(
                    &endpoint,
                    resolve(&directory, &target).await?,
                    grant.clone(),
                )
                .await?;
                let (send, recv) = rds_cli::open_sync(&conn)
                    .instrument(conn.session.span())
                    .await?;
                let stats = rds_sync::engine::send_file(&conn, &path, send, recv)
                    .instrument(conn.session.span())
                    .await?;
                println!(
                    "sent {} ({} chunks, {} bytes)",
                    path.display(),
                    stats.total,
                    stats.bytes
                );
            }
            Command::Recv {
                target,
                rel_path,
                dir,
            } => {
                let conn = dial(
                    &endpoint,
                    resolve(&directory, &target).await?,
                    grant.clone(),
                )
                .await?;
                let (send, recv) = rds_cli::open_sync(&conn)
                    .instrument(conn.session.span())
                    .await?;
                let (dest, stats) = rds_sync::engine::recv_file(&conn, &rel_path, &dir, send, recv)
                    .instrument(conn.session.span())
                    .await?;
                println!(
                    "received {} ({} chunks fetched, {} bytes)",
                    dest.display(),
                    stats.fetched,
                    stats.bytes
                );
            }
        }
        Ok(())
    }
    .await;
    // Dropping the endpoint without close() makes iroh log an
    // "ungraceful abort" error on every command exit.
    endpoint.close().await;
    result
}

fn validate_managed(cli: &Cli) -> anyhow::Result<()> {
    anyhow::ensure!(
        cli.key_file.is_none()
            && cli.relay.is_empty()
            && cli.backend.is_none()
            && cli.endpoint_config.is_none()
            && cli.owned_relay.is_empty()
            && !cli.no_relay
            && cli.bind_address.is_empty()
            && cli.server.is_none()
            && cli.directory_ca.is_none()
            && cli.registry_key.is_none()
            && cli.registry_epoch.is_none()
            && cli.registry_state.is_none()
            && cli.authority_rotation.is_empty(),
        "managed commands use agent configuration; configure the agent or use --direct with a separate identity"
    );
    Ok(())
}

/// Bounded operator-supplied PEM read. NONBLOCK plus the regular-file
/// check refuses FIFOs/devices before the size bound is applied — a
/// plain open on a FIFO blocks forever.
fn read_pem_file(path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    use rustix::fs::{Mode, OFlags};
    use std::io::Read;
    let file = std::fs::File::from(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    anyhow::ensure!(file.metadata()?.is_file(), "{path:?} is not a regular file");
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 1024 * 1024, "{path:?} exceeds 1 MiB");
    Ok(bytes)
}

fn control_directory(cli: &Cli) -> anyhow::Result<std::path::PathBuf> {
    match &cli.control_dir {
        Some(path) => Ok(path.clone()),
        None => Ok(rds_client::local::control_dir_for_key(
            &default_key_path().ok_or_else(|| {
                anyhow::anyhow!(
                    "no control directory: pass --control-dir or set HOME/XDG_CONFIG_HOME"
                )
            })?,
        )?),
    }
}

/// One client-side session: the `rds.conn` span correlates dial, grant,
/// service and close events under one `session_id` for this process, and
/// the guard emits `session_closed` when the command ends — by error
/// propagation, by peer, or cleanly.
struct Dialed {
    conn: rds_net::Connection,
    session: rds_observe::SessionGuard,
}

impl std::ops::Deref for Dialed {
    type Target = rds_net::Connection;
    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

async fn dial(
    endpoint: &rds_net::Endpoint,
    target: rds_net::EndpointAddr,
    grant: Option<rds_core::grant::Grant>,
) -> anyhow::Result<Dialed> {
    let span = rds_observe::conn_span(rds_observe::next_session_id());
    let conn = async {
        match grant {
            Some(g) => rds_cli::connect_authorized(endpoint, target, &g).await,
            None => rds_cli::connect(endpoint, target).await,
        }
    }
    .instrument(span.clone())
    .await?;
    span.record("peer", tracing::field::display(conn.remote_id()));
    let session = rds_observe::SessionGuard::open(span);
    Ok(Dialed { conn, session })
}

async fn resolve(
    directory: &Option<rds_discovery::client::Client>,
    target: &str,
) -> anyhow::Result<rds_net::EndpointAddr> {
    rds_net::resolve_target(directory.clone(), target).await
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn tcp_target_flags_reject_invalid_destinations() {
        for command in ["ssh", "forward"] {
            for target in [":22", "host:0", "host:nope", "::1:22", "[::1]22"] {
                let mut args = vec!["rds", command, "unused-peer", "--remote", target];
                if command == "ssh" {
                    args.extend([
                        "--user",
                        "fixture",
                        "--host-key",
                        "host.pub",
                        "--identity",
                        "key",
                    ]);
                }
                assert!(Cli::try_parse_from(args).is_err(), "{command}: {target}");
            }
        }
    }

    #[test]
    fn ssh_requires_explicit_trust_and_account_and_has_no_listen_port() {
        let args = [
            "rds",
            "ssh",
            "peer",
            "--user",
            "fixture",
            "--host-key",
            "host.pub",
            "--identity",
            "key",
        ];
        assert!(Cli::try_parse_from(args).is_ok());
        assert!(Cli::try_parse_from(["rds", "ssh", "peer"]).is_err());
        assert!(Cli::try_parse_from(args.into_iter().chain(["-L", "127.0.0.1:2222"])).is_err());
    }

    #[test]
    fn renewal_requires_an_explicit_session_and_signed_grant_file() {
        let id = "01010101010101010101010101010101";
        assert!(
            Cli::try_parse_from([
                "rds",
                "session",
                "renew",
                "--session",
                id,
                "--grant-file",
                "grant.json"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["rds", "session", "renew", "--grant-file", "grant.json"]).is_err()
        );
        assert!(Cli::try_parse_from(["rds", "session", "renew", "--session", id]).is_err());
    }
}
