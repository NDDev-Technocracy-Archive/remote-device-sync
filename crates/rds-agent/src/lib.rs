//! The agent: accepts authenticated rds connections and serves streams.
//!
//! Authorization is two-layered:
//!
//! - **Membership** — the peer's QUIC-authenticated `EndpointId` must be
//!   in `policy.allow`, checked as soon as the handshake completes,
//!   before any service stream is read.
//! - **Capability** — when `policy.issuers` is non-empty the peer must
//!   additionally present a [`Grant`](rds_core::grant::Grant) signed by
//!   a trusted issuer as the first stream on the connection. Until a
//!   authorization begins, service streams are refused; streams overlapping
//!   its reply/commit transaction wait with a deadline. Afterwards each
//!   stream is checked against the committed grant's service scope and
//!   constraints (TCP ports, displays, bitrate ceiling).
//!
//! Revocation: `policy.denylist` holds revoked grant ids — a live
//! connection whose grant lands on the denylist is closed, and a grant
//! that expires mid-session closes its connection at `expires_at`. The
//! same grant cannot authorize two concurrent connections (replay
//! guard in `active_grants`).
//!
//! TCP forwarding is restricted to an explicit set of `(host, port)`
//! targets; the default set is exactly the configured SSH socket.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rds_core::grant::{GrantId, VerifiedGrant};
use rds_core::{AgentInfo, HelloAck, PROTOCOL_VERSION, ServiceKind, StreamHello};
use rds_net::{Connection, Endpoint, EndpointId, read_frame, write_frame};
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tracing::{Instrument, debug, info, info_span, warn};

mod authz;
mod limits;
mod revocations;
pub mod settings;
mod sys;
use authz::{ConnAuthz, ConnectionLifetime, authorize};
pub use limits::AgentLimits;
pub use revocations::{RevocationFeed, RevocationPolicy, watch_revocations};
pub use settings::{
    AgentConfigError, AgentOverrides, AgentSettings, ResolvedAgent, Role, ServiceName,
};

/// Session ids and the `rds.conn` span shape are minted by rds-observe so
/// both sides of a connection share the correlation convention.
use rds_observe::{Reason, conn_span, next_session_id};

/// A peer that opens a stream but never writes its `StreamHello` would
/// otherwise park a task per stream for the connection's lifetime —
/// bounded here so silent streams cost seconds, not the session.
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const AUTHZ_REPLY_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Operational deadlines on the serving side. Deployments tune these at
/// the policy level; session and transfer internals keep their own
/// service-scoped budgets. These four fields are the serving-side subset
/// of the shared deadline classes (`rds_net::DeadlinePolicy`, W2.6):
/// `handshake`/`hello` → `Handshake`, `authz` → `Authz`, `shutdown` →
/// `Shutdown`. `Dial`/`Idle`/`Progress` are client- and transfer-side
/// classes owned by their own enforcement sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutPolicy {
    /// Inbound connection-handshake budget.
    pub handshake: Duration,
    /// Per-stream `StreamHello` read deadline — and the reply deadline for
    /// greeting refusals that must not outlive a parked peer task.
    pub hello: Duration,
    /// Authorization-path reply budget: refusal answers and the final
    /// `HelloAck` write while watcher/reservation state is already owned.
    pub authz: Duration,
    /// Join budget for established connection tasks during shutdown; the
    /// runner never waits unboundedly for drained peers.
    pub shutdown: Duration,
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            handshake: HANDSHAKE_TIMEOUT,
            hello: HELLO_TIMEOUT,
            authz: AUTHZ_REPLY_TIMEOUT,
            shutdown: SHUTDOWN_TIMEOUT,
        }
    }
}

/// The largest accepted timeout; guards against effectively-unbounded
/// deadline configuration (roughly one workday of idle handshake budget
/// is never the intent).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

/// Mutex acquisition that survives a poisoned lock: every mutex here
/// guards plain data (a state word, an `Option`, a `HashSet`) whose
/// invariants a panic cannot corrupt, so refusing service forever
/// after one panicked holder would be the worse failure.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runtime policy for the agent.
#[derive(Clone)]
pub struct AgentPolicy {
    /// Peers allowed to open any stream at all.
    pub allow: HashSet<EndpointId>,
    /// TCP targets the `TcpConnect` service may splice to.
    /// The default is the configured SSH socket.
    pub tcp_targets: HashSet<(String, u16)>,
    /// Accept any TCP target. Development escape hatch.
    pub allow_any_tcp: bool,
    /// Trusted grant issuers (Ed25519 verifying keys). Empty = grants
    /// not required: the allowlist alone authorizes (legacy mode).
    /// Non-empty = every connection must present a valid grant on its
    /// first stream before any service stream is served.
    pub issuers: HashSet<[u8; 32]>,
    /// Maximum grant lifetime accepted at verify time.
    pub grant_max_ttl: Duration,
    /// Revoked grant ids and freshness; updated by [`watch_revocations`]
    /// from the estate's signed snapshot or by explicit local policy.
    /// The `watch` channel notifies live connections so a revoked grant
    /// drops its session, not just future ones.
    pub denylist: watch::Sender<Arc<RevocationPolicy>>,
    /// Grant ids currently bound to a live connection — the replay
    /// guard: the same grant cannot run two concurrent sessions.
    pub active_grants: Arc<Mutex<HashSet<GrantId>>>,
    /// Directory the `Sync` service may read/write under (WS6). `None`
    /// disables sync entirely.
    pub sync_dir: Option<PathBuf>,
    /// Data-plane services this agent answers. `None` keeps the implicit
    /// set — `Tcp` plus `Desktop` when compiled and `Sync` when `sync_dir`
    /// is configured. `Some` is the explicit set; `Ping`/`Info` are the
    /// always-on control plane — never gated by deployment policy, though
    /// a grant's service scope may still refuse them. A disabled service
    /// is refused before any grant or service work runs.
    pub services: Option<BTreeSet<ServiceKind>>,
    /// Admission and greeting deadlines applied per connection/stream.
    pub timeouts: TimeoutPolicy,
    /// Tenant this agent belongs to (v3 grant claim). `Some` pins the
    /// deployment: every grant must carry the same `tenant` claim —
    /// unscoped and mismatched grants are refused at authorization.
    pub tenant: Option<String>,
    /// Minimum estate policy revision a grant must claim (v3). `Some`
    /// retires grants minted before a policy change without waiting for
    /// their expiry or a revocation snapshot.
    pub min_policy_revision: Option<u64>,
}

impl std::fmt::Debug for AgentPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentPolicy")
            .field("allow", &self.allow)
            .field("tcp_targets", &self.tcp_targets)
            .field("allow_any_tcp", &self.allow_any_tcp)
            .field("issuers", &self.issuers.len())
            .field("grant_max_ttl", &self.grant_max_ttl)
            .field("sync_dir", &self.sync_dir)
            .field("services", &self.services)
            .field("timeouts", &self.timeouts)
            .field("tenant", &self.tenant)
            .field("min_policy_revision", &self.min_policy_revision)
            .finish_non_exhaustive()
    }
}

impl AgentPolicy {
    pub fn ssh_only(ssh: (String, u16)) -> Self {
        Self {
            allow: HashSet::new(),
            tcp_targets: HashSet::from([ssh]),
            allow_any_tcp: false,
            issuers: HashSet::new(),
            grant_max_ttl: Duration::from_secs(300),
            denylist: watch::channel(Arc::new(RevocationPolicy::default())).0,
            active_grants: Arc::new(Mutex::new(HashSet::new())),
            sync_dir: None,
            services: None,
            timeouts: TimeoutPolicy::default(),
            tenant: None,
            min_policy_revision: None,
        }
    }

    pub fn permits_tcp(&self, host: &str, port: u16) -> bool {
        let Ok(target) = rds_core::TcpTarget::new(host, port) else {
            return false;
        };
        self.permits_tcp_target(&target)
    }

    fn permits_tcp_target(&self, target: &rds_core::TcpTarget) -> bool {
        self.allow_any_tcp
            || self.tcp_targets.iter().any(|(host, port)| {
                rds_core::TcpTarget::new(host, *port).is_ok_and(|allowed| &allowed == target)
            })
    }

    /// Whether this connection's peer must present a grant.
    fn grants_required(&self) -> bool {
        !self.issuers.is_empty()
    }

    /// Every service this agent answers: the always-on `Ping`/`Info`
    /// control plane plus either the explicit `services` set or the
    /// implicit set derived from build features and configuration.
    /// `Audio` is wire-reserved but unimplemented, so it is never in the
    /// effective set even if it slips into an explicit one.
    pub fn effective_services(&self) -> BTreeSet<ServiceKind> {
        let mut set = BTreeSet::from([ServiceKind::Ping, ServiceKind::Info]);
        match &self.services {
            // One honest source: an explicit `desktop` entry only counts when
            // this binary can actually serve it, matching the implicit arm
            // and the directory announcement. `validate` still rejects the
            // flag combination at startup.
            Some(explicit) => set.extend(explicit.iter().copied().filter(|k| {
                matches!(k, ServiceKind::Tcp | ServiceKind::Sync)
                    || (matches!(k, ServiceKind::Desktop) && cfg!(feature = "desktop"))
            })),
            None => {
                set.insert(ServiceKind::Tcp);
                if cfg!(feature = "desktop") {
                    set.insert(ServiceKind::Desktop);
                }
                if self.sync_dir.is_some() {
                    set.insert(ServiceKind::Sync);
                }
            }
        }
        set
    }

    /// Whether a service stream is admitted by deployment policy, before
    /// any grant scope or per-service check.
    fn service_enabled(&self, kind: ServiceKind) -> bool {
        self.effective_services().contains(&kind)
    }

    /// Deployment preflight: an explicit `services` set may only name
    /// implemented services whose prerequisites are configured, and the
    /// timeout policy must stay inside sane bounds. Programmatic callers
    /// should run this before binding; `Agent::run` runs it too.
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(set) = &self.services {
            for kind in set {
                match kind {
                    ServiceKind::Tcp => {}
                    ServiceKind::Desktop if cfg!(feature = "desktop") => {}
                    ServiceKind::Desktop => {
                        return Err("desktop service requires the `desktop` build feature");
                    }
                    ServiceKind::Sync if self.sync_dir.is_some() => {}
                    ServiceKind::Sync => {
                        return Err("sync service requires a configured sync directory");
                    }
                    ServiceKind::Audio => {
                        return Err("audio service is reserved but not implemented");
                    }
                    _ => {
                        return Err("service set may only contain tcp, desktop or sync; \
                             ping and info are always served");
                    }
                }
            }
        }
        if self.timeouts.handshake.is_zero()
            || self.timeouts.hello.is_zero()
            || self.timeouts.authz.is_zero()
            || self.timeouts.shutdown.is_zero()
            || self.timeouts.handshake > MAX_TIMEOUT
            || self.timeouts.hello > MAX_TIMEOUT
            || self.timeouts.authz > MAX_TIMEOUT
            || self.timeouts.shutdown > MAX_TIMEOUT
        {
            return Err("timeouts must be between 1 and 3600 seconds");
        }
        if let Some(tenant) = &self.tenant
            && (tenant.is_empty()
                || tenant.len() > rds_core::grant::MAX_TENANT_LEN
                || tenant.bytes().any(|b| b < 0x21 || b == 0x7f))
        {
            return Err(
                "tenant must be nonempty, at most 64 bytes and free of control or whitespace bytes",
            );
        }
        // A pinned binding with no trusted issuers never evaluates: grants
        // are not required, so every connection would pass unscoped. Fail
        // loudly instead of silently dropping the deployment's binding.
        if self.issuers.is_empty() && (self.tenant.is_some() || self.min_policy_revision.is_some())
        {
            return Err("tenant/policy-revision binding requires grant issuers");
        }
        Ok(())
    }

    /// Revoke a grant id — pushes onto the denylist and notifies every
    /// live connection watcher. The value is retained without subscribers.
    pub fn revoke(&self, id: GrantId) {
        self.denylist.send_modify(|set| {
            Arc::make_mut(&mut Arc::make_mut(set).ids).insert(id);
        });
    }

    /// Read the current denylist snapshot.
    pub fn denied(&self) -> Arc<HashSet<GrantId>> {
        self.denylist.borrow().ids.clone()
    }

    /// Replace revoked ids without changing freshness. Managed feeds commit
    /// and publish their own complete snapshots; this does not renew a lease.
    /// Live connections re-check on the notification.
    pub fn replace_denylist(&self, ids: HashSet<GrantId>) {
        self.denylist.send_modify(|value| {
            Arc::make_mut(value).ids = Arc::new(ids);
        });
    }
}

/// A bound agent: endpoint plus policy, ready to `run`.
pub struct Agent {
    pub endpoint: Endpoint,
    pub policy: Arc<AgentPolicy>,
    desktop: bool,
    limits: AgentLimits,
    admission: Arc<Semaphore>,
    stream_counter: limits::StreamCounter,
    gate: Option<limits::ResourceGate>,
}

/// Weak, in-memory observation: keeping an exporter alive never owns agent I/O.
#[derive(Clone)]
pub struct AgentMetrics(std::sync::Weak<Agent>);

impl AgentMetrics {
    pub fn snapshot(&self) -> std::collections::BTreeMap<&'static str, u64> {
        let Some(agent) = self.0.upgrade() else {
            return std::collections::BTreeMap::from([("rds_agent_metrics_available", 0)]);
        };
        let mut values = agent.endpoint.metrics().snapshot();
        values.extend([
            ("rds_agent_metrics_available", 1),
            (
                "rds_agent_connections_active",
                agent.active_connections() as u64,
            ),
            (
                "rds_agent_connections_limit",
                agent.limits.connections() as u64,
            ),
            ("rds_agent_streams_active", agent.active_streams() as u64),
            (
                "rds_agent_streams_per_connection_limit",
                agent.limits.streams() as u64,
            ),
            (
                "rds_agent_grants_required",
                u64::from(agent.policy.grants_required()),
            ),
        ]);
        // Process footprint where the kernel reports it; an unobservable
        // platform simply omits the key rather than inventing a number.
        if let Some(fds) = sys::open_fds() {
            values.insert("rds_agent_process_fds", fds as u64);
        }
        if let Some(rss) = sys::rss_bytes() {
            values.insert("rds_agent_process_rss_bytes", rss);
        }
        let grants = agent.policy.active_grants.try_lock().ok();
        values.insert("rds_agent_active_grants_known", u64::from(grants.is_some()));
        if let Some(grants) = grants {
            values.insert("rds_agent_active_grants", grants.len() as u64);
        }
        // Clone the immutable value before checking its lease. No watch borrow
        // spans a clock read or exporter formatting; no identifiers are copied.
        let policy = agent.policy.denylist.borrow().clone();
        values.extend(policy.metrics(
            rds_discovery::clock::Reading::cached_now(),
            agent.policy.grants_required(),
        ));
        values
    }
}

impl Agent {
    pub fn metrics(self: &Arc<Self>) -> AgentMetrics {
        AgentMetrics(Arc::downgrade(self))
    }

    pub fn new(endpoint: Endpoint, policy: AgentPolicy) -> Self {
        let limits = AgentLimits::default();
        Self {
            endpoint,
            policy: Arc::new(policy),
            desktop: cfg!(feature = "desktop"),
            limits,
            admission: Arc::new(Semaphore::new(limits.connections())),
            stream_counter: Default::default(),
            gate: limits::ResourceGate::new(limits),
        }
    }

    /// Select budgets before starting the agent. Consuming self prevents
    /// replacing the admission semaphore while a runner borrows this agent.
    pub fn with_limits(mut self, limits: AgentLimits) -> Self {
        self.limits = limits;
        self.admission = Arc::new(Semaphore::new(limits.connections()));
        self.gate = limits::ResourceGate::new(limits);
        self
    }

    /// Occupied connection slots, including pending handshakes. This differs
    /// from the transport sampler's count of established allowed connections.
    pub fn active_connections(&self) -> usize {
        self.limits.connections() - self.admission.available_permits()
    }

    /// Service tasks across this agent, including hello/Authz waits. This does
    /// not count uni routing, blocking disk jobs or native media workers.
    pub fn active_streams(&self) -> usize {
        self.stream_counter.active()
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Accept connections until the endpoint closes.
    pub async fn run(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.policy.grants_required() || self.limits.streams() >= 2,
            "grant mode requires at least two stream slots"
        );
        self.policy
            .validate()
            .map_err(|why| anyhow::anyhow!("invalid agent policy: {why}"))?;
        info!(id = %self.endpoint.id(), "agent listening");
        rds_observe::emit(rds_observe::Event::ListenerReady);
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                result = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = result {
                        debug!(%error, "connection task ended");
                    }
                }
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else { break; };
                    if self.gate.as_ref().is_some_and(|gate| !gate.allows()) {
                        rds_observe::emit(rds_observe::Event::ConnectionBudgetExhausted);
                        // Dropping Incoming refuses the handshake without a
                        // parked application task or a new connection slot.
                        drop(incoming);
                        debug!("connection refused: process resource budget exceeded");
                        continue;
                    }
                    let Ok(permit) = self.admission.clone().try_acquire_owned() else {
                        rds_observe::emit(rds_observe::Event::ConnectionBudgetExhausted);
                        // Dropping Incoming refuses the handshake without a
                        // parked application task or a new connection slot.
                        drop(incoming);
                        debug!("connection refused: admission budget exhausted");
                        continue;
                    };
                    let audience = *self.endpoint.id().as_bytes();
                    let policy = self.policy.clone();
                    let desktop = self.desktop;
                    let metrics = self.endpoint.metrics();
                    let limits = self.limits;
                    let stream_counter = self.stream_counter.clone();
                    connections.spawn(async move {
                        let _permit = permit;
                        match tokio::time::timeout(policy.timeouts.handshake, incoming).await {
                            Ok(Ok(conn)) => {
                                let span = conn_span(next_session_id());
                                span.record("peer", tracing::field::display(conn.remote_id()));
                                if let Err(error) = serve_connection(conn, audience, policy, desktop, metrics, limits, stream_counter)
                                    .instrument(span).await {
                                    debug!(%error, "connection ended");
                                }
                            }
                            Ok(Err(error)) => {
                                rds_observe::emit(rds_observe::Event::HandshakeFailed);
                                debug!(%error, "incoming handshake failed");
                            }
                            Err(_) => {
                                rds_observe::emit(rds_observe::Event::HandshakeTimedOut);
                                debug!("incoming handshake timed out");
                            }
                        }
                    });
                }
            }
        }
        // Endpoint closure wakes established connections and handshakes. Let
        // their normal paths join service workers before this runner returns.
        if tokio::time::timeout(self.policy.timeouts.shutdown, async {
            while let Some(result) = connections.join_next().await {
                if let Err(error) = result {
                    debug!(%error, "connection task ended");
                }
            }
        })
        .await
        .is_err()
        {
            debug!("agent shutdown budget expired; aborting connection tasks");
            connections.shutdown().await;
        }
        Ok(())
    }

    /// Serve a single already-established connection.
    pub async fn serve(&self, conn: Connection) -> anyhow::Result<()> {
        // Programmatic callers reach serve() without run()'s preflight —
        // the same policy contract applies either way.
        if let Err(why) = self.policy.validate() {
            conn.close(5u32.into(), b"invalid agent policy");
            anyhow::bail!("invalid agent policy: {why}");
        }
        if self.policy.grants_required() && self.limits.streams() < 2 {
            conn.close(5u32.into(), b"invalid grant stream budget");
            anyhow::bail!("grant mode requires at least two stream slots");
        }
        if self.gate.as_ref().is_some_and(|gate| !gate.allows()) {
            rds_observe::emit(rds_observe::Event::ConnectionBudgetExhausted);
            conn.close(5u32.into(), b"agent process resource budget exceeded");
            anyhow::bail!("agent process resource budget exceeded");
        }
        let Ok(_permit) = self.admission.clone().try_acquire_owned() else {
            rds_observe::emit(rds_observe::Event::ConnectionBudgetExhausted);
            conn.close(5u32.into(), b"agent connection budget exhausted");
            anyhow::bail!("agent connection budget exhausted");
        };
        let span = conn_span(next_session_id());
        span.record("peer", tracing::field::display(conn.remote_id()));
        serve_connection(
            conn,
            *self.endpoint.id().as_bytes(),
            self.policy.clone(),
            self.desktop,
            self.endpoint.metrics(),
            self.limits,
            self.stream_counter.clone(),
        )
        .instrument(span)
        .await
    }
}

async fn serve_connection(
    conn: Connection,
    audience: [u8; 32],
    policy: Arc<AgentPolicy>,
    desktop: bool,
    metrics: rds_net::metrics::Registry,
    limits: AgentLimits,
    stream_counter: limits::StreamCounter,
) -> anyhow::Result<()> {
    let peer = conn.remote_id();
    if !policy.allow.contains(&peer) {
        rds_observe::emit(rds_observe::Event::PeerRejected);
        warn!(%peer, "rejected: endpoint id not in allowlist");
        conn.close(1u32.into(), b"not allowed");
        anyhow::bail!("peer {peer} not in allowlist");
    }
    info!(%peer, "peer connected");
    rds_observe::emit(rds_observe::Event::PeerAccepted);
    // Drop emits session_closed(aborted) even when this future is aborted —
    // the acceptance loop's shutdown budget can cancel live connections.
    let session = rds_observe::SessionGuard::open(tracing::Span::current());
    let authz = Arc::new(ConnAuthz::new(
        policy.grants_required(),
        audience,
        limits.streams(),
    ));
    let lifetime = ConnectionLifetime {
        conn: conn.clone(),
        authz: authz.clone(),
    };
    // The sampler is part of this future, so cancellation drops its gauges
    // immediately rather than leaving a detached observer holding Connection.
    let mut sampler = metrics.sampler(conn.clone());
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut streams = JoinSet::new();
    // The close cause must be read before `lifetime` drops — its Drop
    // closes the conn locally and would overwrite a peer-side reason.
    let close_reason = loop {
        tokio::select! {
            biased;
            _ = conn.wait_closed() => {
                break match conn.close_kind() {
                    Some(rds_net::CloseKind::Local) => Reason::LocalClosed,
                    Some(rds_net::CloseKind::PeerApplication)
                    | Some(rds_net::CloseKind::PeerTransport) => Reason::PeerClosed,
                    Some(rds_net::CloseKind::TimedOut) => Reason::Timeout,
                    Some(rds_net::CloseKind::Reset) => Reason::Reset,
                    Some(rds_net::CloseKind::Transport) => Reason::Transport,
                    None => Reason::Aborted,
                };
            }
            result = streams.join_next(), if !streams.is_empty() => {
                if let Some(Err(error)) = result { debug!(%error, "stream task ended"); }
            }
            _ = tick.tick() => {
                if sampler.sample() {
                    rds_observe::emit(rds_observe::Event::PathMigrated);
                }
            }
            incoming = conn.accept_bi(), if streams.len() < limits.streams() => {
                let (send, recv) = match incoming {
                    Ok(streams) => streams,
                    Err(error) => {
                        debug!(%peer, %error, "connection closed");
                        break Reason::Transport;
                    }
                };
                let policy = policy.clone();
                let conn = conn.clone();
                let authz = authz.clone();
                let span = tracing::Span::current();
                let active = stream_counter.enter();
                streams.spawn(async move {
                    let _active = active;
                    if let Err(error) = rds_observe::observe(
                        rds_observe::Operation::ServiceStream,
                        serve_stream(conn, send, recv, policy, authz, desktop),
                    ).await {
                        debug!(%error, "stream ended");
                    }
                }.instrument(span));
            }
        }
    };
    sampler.sample();
    authz.close_and_wait().await;
    drop(lifetime);
    streams.shutdown().await;
    session.close(close_reason);
    Ok(())
}

async fn serve_stream(
    conn: Connection,
    mut send: rds_net::SendStream,
    mut recv: rds_net::RecvStream,
    policy: Arc<AgentPolicy>,
    authz: Arc<ConnAuthz>,
    desktop: bool,
) -> anyhow::Result<()> {
    let hello: StreamHello =
        match tokio::time::timeout(policy.timeouts.hello, read_frame(&mut recv)).await {
            Ok(h) => h?,
            Err(_) => {
                rds_observe::request_refused(Reason::Timeout);
                anyhow::bail!("stream hello timed out");
            }
        };
    if let StreamHello::Authz(grant) = hello {
        return rds_observe::observe(
            rds_observe::Operation::GrantAuthorize,
            authorize(&conn, send, grant, &policy, &authz, false),
        )
        .await;
    }
    if let StreamHello::RenewAuthz(grant) = hello {
        return rds_observe::observe(
            rds_observe::Operation::GrantRenew,
            authorize(&conn, send, grant, &policy, &authz, true),
        )
        .await;
    }
    // Deployment policy answers first: a disabled service is refused before
    // grant machinery or per-service work runs.
    if let Some(kind) = service_kind(&hello)
        && !policy.service_enabled(kind)
    {
        rds_observe::request_refused(Reason::Denied);
        // Greeting refusals are bounded by the hello deadline — a stalled
        // peer must not park this task on a refusal write.
        tokio::time::timeout(
            policy.timeouts.hello,
            write_frame(
                &mut send,
                &HelloAck::Error {
                    message: format!("service {kind:?} not enabled on this agent"),
                },
            ),
        )
        .await??;
        send.finish()?;
        anyhow::bail!("service {kind:?} not enabled");
    }
    let grant = match authz.service_scope(&policy).await {
        Ok(g) => g,
        Err(why) => {
            rds_observe::request_refused(why.reason());
            if why.terminal() {
                conn.close(2u32.into(), why.message().as_bytes());
            }
            let why = why.message();
            // Authorization-path answers are bounded by the authz budget.
            tokio::time::timeout(
                policy.timeouts.authz,
                write_frame(
                    &mut send,
                    &HelloAck::Error {
                        message: why.into(),
                    },
                ),
            )
            .await??;
            send.finish()?;
            anyhow::bail!("stream refused: {why}");
        }
    };
    let scope_err = grant.as_ref().and_then(|g| scope_check(g, &hello).err());
    if let Some(why) = scope_err {
        rds_observe::request_refused(Reason::Denied);
        tokio::time::timeout(
            policy.timeouts.authz,
            write_frame(&mut send, &HelloAck::Error { message: why }),
        )
        .await??;
        send.finish()?;
        anyhow::bail!("stream outside grant scope");
    }
    // TCP preflight resolves the target and policy before the slot is
    // consumed — a refused connect must not hold a service lane while
    // its refusal is written. The service arm re-validates on its own.
    if let StreamHello::TcpConnect { host, port } = &hello {
        let refusal = match rds_core::TcpTarget::new(host, *port) {
            Ok(target) if !policy.permits_tcp_target(&target) => {
                Some(format!("tcp target {host}:{port} not permitted"))
            }
            Ok(_) => None,
            Err(error) => Some(format!("invalid TCP destination: {error}")),
        };
        if let Some(message) = refusal {
            rds_observe::request_refused(Reason::Denied);
            tokio::time::timeout(
                policy.timeouts.hello,
                write_frame(&mut send, &HelloAck::Error { message }),
            )
            .await??;
            send.finish()?;
            anyhow::bail!("tcp target refused at preflight");
        }
    }
    // Control greetings (Ping/Info) are short-lived and bypass the service
    // pool; only long-lived data services consume it, so a full pool drains
    // back to one free JoinSet lane for whatever hello arrives next.
    let _service_slot = if matches!(
        service_kind(&hello),
        Some(ServiceKind::Tcp | ServiceKind::Desktop | ServiceKind::Sync | ServiceKind::Audio)
    ) {
        let Some(slot) = authz.try_service_slot() else {
            rds_observe::request_refused(Reason::BudgetExhausted);
            tokio::time::timeout(
                policy.timeouts.hello,
                write_frame(
                    &mut send,
                    &HelloAck::Error {
                        message: "service capacity reached; a lane is reserved for control traffic"
                            .into(),
                    },
                ),
            )
            .await??;
            send.finish()?;
            anyhow::bail!("service capacity reached");
        };
        Some(slot)
    } else {
        None
    };
    let span = info_span!("rds.stream", service = ?service_kind(&hello));
    // Per-session frame route for `DesktopV2`; the shared `Desktop`
    // route serves the legacy greeting. Only the desktop service arm
    // consumes it, and that arm is feature-gated.
    #[cfg(feature = "desktop")]
    let desktop_frame_route = match &hello {
        StreamHello::DesktopV2 { session, .. } => {
            rds_core::UniHello::DesktopFrames { id: *session }
        }
        _ => rds_core::UniHello::Desktop,
    };
    async move {
        match hello {
            StreamHello::Ping { nonce } => {
                write_frame(&mut send, &HelloAck::Ok).await?;
                send.write_all(&nonce.to_be_bytes()).await?;
                send.finish()?;
            }
            StreamHello::Info => {
                let info = AgentInfo {
                    protocol: PROTOCOL_VERSION,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    hostname: hostname(),
                    services: {
                        let enabled = policy.effective_services();
                        [
                            ServiceKind::Ping,
                            ServiceKind::Info,
                            ServiceKind::Tcp,
                            ServiceKind::Desktop,
                            ServiceKind::Sync,
                        ]
                        .into_iter()
                        .filter(|k| enabled.contains(k) && (*k != ServiceKind::Desktop || desktop))
                        .collect()
                    },
                    desktop: desktop_caps(desktop),
                };
                write_frame(&mut send, &HelloAck::Info(info)).await?;
                send.finish()?;
            }
            StreamHello::TcpConnect { host, port } => {
                let target = match rds_core::TcpTarget::new(&host, port) {
                    Ok(target) => target,
                    Err(error) => {
                        write_frame(
                            &mut send,
                            &HelloAck::Error {
                                message: format!("invalid TCP destination: {error}"),
                            },
                        )
                        .await?;
                        return Err(error.into());
                    }
                };
                if !policy.permits_tcp_target(&target) {
                    write_frame(
                        &mut send,
                        &HelloAck::Error {
                            message: format!("tcp target {host}:{port} not permitted"),
                        },
                    )
                    .await?;
                    anyhow::bail!("tcp target {host}:{port} rejected");
                }
                let (host, port) = target.into_parts();
                match TcpStream::connect((host.as_str(), port)).await {
                    Ok(mut tcp) => {
                        write_frame(&mut send, &HelloAck::Ok).await?;
                        let mut quic = tokio::io::join(recv, send);
                        tokio::io::copy_bidirectional(&mut tcp, &mut quic).await?;
                    }
                    Err(e) => {
                        // ErrorKind is a fixed vocabulary — the raw OS error
                        // string (errno text, platform internals) never
                        // crosses the wire. The bail below logs it locally.
                        write_frame(
                            &mut send,
                            &HelloAck::Error {
                                message: format!("connect {host}:{port} failed: {}", e.kind()),
                            },
                        )
                        .await?;
                        anyhow::bail!("tcp connect {host}:{port} failed: {e}");
                    }
                }
            }
            StreamHello::Desktop(hello) | StreamHello::DesktopV2 { hello, .. } => {
                if desktop {
                    #[cfg(feature = "desktop")]
                    match rds_desktop::capabilities() {
                        Ok(caps) => {
                            write_frame(&mut send, &HelloAck::Desktop(caps)).await?;
                            let max_bps = grant.as_ref().and_then(|g| g.max_bps());
                            // Optional output downscale (e.g. 720p for
                            // latency/bandwidth) — a deployment knob, not
                            // a per-session negotiation.
                            let output_height = std::env::var("RDS_DESKTOP_OUTPUT_HEIGHT")
                                .ok()
                                .and_then(|v| v.parse::<u32>().ok())
                                .filter(|&h| (240..=4320).contains(&h));
                            rds_desktop::serve_desktop_with(
                                conn,
                                send,
                                recv,
                                hello,
                                rds_desktop::SessionConfig {
                                    bitrate_ceiling: max_bps,
                                    view_only: grant
                                        .as_ref()
                                        .is_some_and(|g| !g.permits_desktop_control()),
                                    frame_route: Some(desktop_frame_route),
                                    output_height,
                                    ..Default::default()
                                },
                            )
                            .await?;
                        }
                        Err(e) => {
                            write_frame(
                                &mut send,
                                &HelloAck::Error {
                                    message: "desktop unavailable".into(),
                                },
                            )
                            .await?;
                            anyhow::bail!("desktop capability probe failed: {e}");
                        }
                    }
                    #[cfg(not(feature = "desktop"))]
                    {
                        // `desktop` is `cfg!(feature = "desktop")` so this
                        // is unreachable today — but a request path must
                        // refuse, never panic, if that coupling breaks.
                        write_frame(
                            &mut send,
                            &HelloAck::Error {
                                message: "desktop service not compiled in".into(),
                            },
                        )
                        .await?;
                        anyhow::bail!("desktop requested but not compiled in");
                    }
                } else {
                    let _ = hello;
                    write_frame(
                        &mut send,
                        &HelloAck::Error {
                            message: "agent built without desktop support".into(),
                        },
                    )
                    .await?;
                }
            }
            StreamHello::Sync
            | StreamHello::SyncTransfer { .. }
            | StreamHello::SyncTransferV2 { .. } => {
                let transfer = match hello {
                    StreamHello::SyncTransfer { id } => Some(rds_sync::engine::Transfer::new(id)),
                    StreamHello::SyncTransferV2 { id } => {
                        Some(rds_sync::engine::Transfer::new_v2(id))
                    }
                    _ => None,
                };
                let Some(dir) = policy.sync_dir.clone() else {
                    write_frame(
                        &mut send,
                        &HelloAck::Error {
                            message: "sync service not configured".into(),
                        },
                    )
                    .await?;
                    anyhow::bail!("sync service not configured");
                };
                let Some(_sync_slot) = authz.try_sync_slot() else {
                    write_frame(
                        &mut send,
                        &HelloAck::Error {
                            message: "sync session already active on this connection".into(),
                        },
                    )
                    .await?;
                    anyhow::bail!("concurrent sync session refused");
                };
                write_frame(&mut send, &HelloAck::Ok).await?;
                let access = grant
                    .as_ref()
                    .map_or(rds_sync::engine::Access::READ_WRITE, |g| {
                        // Grant v3 `sync_paths` entries passed the decoder's
                        // lexical checks; normalize `.`/empty components the
                        // same way `check_rel_path` normalizes requests so
                        // prefix matching compares like with like. Split on
                        // both separators — `check_scope_path` admits `\` but
                        // `Path::components` on Unix does not.
                        let paths = g.payload.constraints.sync_paths.as_ref().map(|list| {
                            list.iter()
                                .map(|scope| {
                                    scope
                                        .split(['/', '\\'])
                                        .filter(|p| !p.is_empty() && *p != ".")
                                        .collect::<PathBuf>()
                                })
                                .collect::<Vec<_>>()
                                .into()
                        });
                        rds_sync::engine::Access {
                            read: g.permits_sync_read(),
                            write: g.permits_sync_write(),
                            paths,
                        }
                    });
                if let Some(transfer) = transfer {
                    transfer
                        .serve(
                            conn,
                            (send, recv),
                            dir,
                            access,
                            rds_sync::engine::TRANSFER_TIMEOUT,
                        )
                        .await?;
                } else {
                    rds_sync::engine::serve_with_access(
                        conn,
                        send,
                        recv,
                        dir,
                        access,
                        rds_sync::engine::TRANSFER_TIMEOUT,
                    )
                    .await?;
                }
            }
            StreamHello::Audio(_) => {
                // Wire shape landed in protocol v2; capture/codec support
                // is v0.3 scope. Refuse politely rather than hang.
                write_frame(
                    &mut send,
                    &HelloAck::Error {
                        message: "audio service not implemented".into(),
                    },
                )
                .await?;
                anyhow::bail!("audio service not implemented");
            }
            StreamHello::Authz(_) | StreamHello::RenewAuthz(_) => {
                // `authorize` early-returns on Authz, so this is
                // unreachable — a request path still refuses rather
                // than panic if that ever stops holding.
                write_frame(
                    &mut send,
                    &HelloAck::Error {
                        message: "authz is not a service".into(),
                    },
                )
                .await?;
                anyhow::bail!("authz stream on the service path");
            }
        }
        Ok(())
    }
    .instrument(span)
    .await
}

/// The service a `StreamHello` selects; `None` for `Authz`, which is
/// not a service stream.
fn service_kind(hello: &StreamHello) -> Option<ServiceKind> {
    Some(match hello {
        StreamHello::Ping { .. } => ServiceKind::Ping,
        StreamHello::Info => ServiceKind::Info,
        StreamHello::TcpConnect { .. } => ServiceKind::Tcp,
        StreamHello::Desktop(_) | StreamHello::DesktopV2 { .. } => ServiceKind::Desktop,
        StreamHello::Sync
        | StreamHello::SyncTransfer { .. }
        | StreamHello::SyncTransferV2 { .. } => ServiceKind::Sync,
        StreamHello::Audio(_) => ServiceKind::Audio,
        StreamHello::Authz(_) | StreamHello::RenewAuthz(_) => return None,
    })
}

/// Whether `hello`'s service is inside `grant`'s scope — service kind
/// plus the constraint that applies to that service.
fn scope_check(grant: &VerifiedGrant, hello: &StreamHello) -> Result<(), String> {
    match hello {
        StreamHello::TcpConnect { port, .. } if !grant.permits_port(*port) => {
            return Err(format!("port {port} outside grant constraints"));
        }
        StreamHello::Desktop(h) | StreamHello::DesktopV2 { hello: h, .. }
            if !grant.permits_display(h.display) =>
        {
            return Err(format!("display {} outside grant constraints", h.display));
        }
        _ => {}
    }
    let Some(kind) = service_kind(hello) else {
        return Err("authz is not a service".into());
    };
    if !grant.permits_service(kind) {
        return Err(format!("service {kind:?} not granted"));
    }
    Ok(())
}

fn desktop_caps(enabled: bool) -> Option<rds_core::DesktopCaps> {
    #[cfg(feature = "desktop")]
    if enabled {
        return rds_desktop::capabilities().ok();
    }
    let _ = enabled;
    None
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
}
