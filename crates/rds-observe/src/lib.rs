//! Shared process logging. JSON is an allowlisted export schema; text is local
//! debugging output and may contain private diagnostics. Neither output is an
//! audit journal. See `docs/observability.md` for loss and shutdown semantics.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event as TraceEvent, Subscriber};
use tracing_subscriber::filter::{FilterExt, filter_fn};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer};

pub mod admin;

mod output;
use output::{Buffer, Output};
pub use output::{ConsolePause, Health, HealthObserver, Shutdown};

const TARGET: &str = "rds_telemetry";
/// Each queued record, including its newline, is at most this size.
pub const MAX_RECORD_BYTES: usize = 4096;
/// At most 4 MiB of record payload plus channel/allocator overhead.
pub const QUEUE_RECORDS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Private, local debugging; free-form fields are retained.
    Text,
    /// Export-safe metadata and typed events. No free-form fields or messages.
    Json,
}

#[derive(Clone, Copy, Debug)]
pub enum Service {
    Agent,
    Cli,
    Server,
    Relay,
    Bench,
}

impl Service {
    fn name(self) -> &'static str {
        match self {
            Self::Agent => "rds-agent",
            Self::Cli => "rds-cli",
            Self::Server => "rds-server",
            Self::Relay => "rds-relay",
            Self::Bench => "rds-bench",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("RDS_LOG_FORMAT must be text or json")]
    Format,
    #[error("RUST_LOG must be valid Unicode and a valid tracing filter")]
    Filter,
    #[error("could not start the logging output worker")]
    Worker(#[source] std::io::Error),
    #[error("a global tracing subscriber is already installed")]
    Subscriber,
}

/// Explicit configuration is also usable by embedders without environment
/// mutation. Invalid filters fail before product initialization.
pub struct Config {
    format: Format,
    filter: EnvFilter,
}

impl Config {
    pub fn format(&self) -> Format {
        self.format
    }
    pub fn new(format: Format, filter: &str) -> Result<Self, InitError> {
        Ok(Self {
            format,
            filter: EnvFilter::builder()
                .with_regex(false)
                .parse(filter)
                .map_err(|_| InitError::Filter)?,
        })
    }

    pub fn from_env(default_filter: &str) -> Result<Self, InitError> {
        let format = match std::env::var("RDS_LOG_FORMAT") {
            Ok(s) if s == "json" => Format::Json,
            Ok(s) if s == "text" => Format::Text,
            Err(std::env::VarError::NotPresent) => Format::Text,
            _ => return Err(InitError::Format),
        };
        let filter = match std::env::var("RUST_LOG") {
            Ok(s) => s,
            Err(std::env::VarError::NotPresent) => default_filter.to_owned(),
            _ => return Err(InitError::Filter),
        };
        Self::new(format, &filter)
    }
}

/// Own this in main until product tasks have stopped. Explicit `shutdown`
/// reports whether stderr drained; Drop performs the same bounded wait.
pub struct Telemetry {
    output: Output,
    format: Format,
}

pub fn init(service: Service, default_filter: &str) -> Result<Telemetry, InitError> {
    install(service, Config::from_env(default_filter)?)
}

/// Common binary entrypoint after CLI parsing. An initialized process never
/// hands its error back to Rust's synchronous stderr Termination renderer.
pub async fn run_main<E: std::fmt::Display>(
    service: Service,
    default_filter: &str,
    future: impl Future<Output = Result<(), E>>,
) -> std::process::ExitCode {
    let telemetry = match init(service, default_filter) {
        Ok(telemetry) => telemetry,
        Err(error) => {
            // Pre-initialization failure; no product work has been polled.
            eprintln!("could not initialize RDS logging: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let result = telemetry.run(future).await;
    telemetry.finish(result)
}

pub fn install(service: Service, config: Config) -> Result<Telemetry, InitError> {
    install_with_writer(service, config, std::io::stderr())
}

/// An explicit owned sink separates protocol/terminal stderr from telemetry.
/// The same bounded queue, privacy schema and shutdown rules apply.
pub fn install_with_writer<W: std::io::Write + Send + 'static>(
    service: Service,
    config: Config,
    writer: W,
) -> Result<Telemetry, InitError> {
    let (subscriber, telemetry) = subscriber(service, config, writer)?;
    tracing::subscriber::set_global_default(subscriber).map_err(|_| InitError::Subscriber)?;
    Ok(telemetry)
}

fn subscriber<W: std::io::Write + Send + 'static>(
    service: Service,
    config: Config,
    writer: W,
) -> Result<(impl Subscriber + Send + Sync, Telemetry), InitError> {
    let output = Output::new(writer, QUEUE_RECORDS).map_err(InitError::Worker)?;
    let layer = RecordLayer {
        shared: Arc::new(Shared {
            service: service.name(),
            run_id: format!("{:032x}", rand::random::<u128>()),
            sequence: AtomicU64::new(0),
            started: Instant::now(),
            sink: output.sink(),
            format: config.format,
        }),
    };
    // Operational events and the numeric connection context remain available
    // even under RUST_LOG=off. Legacy diagnostics obey the requested filter.
    let telemetry_layer = layer.clone().with_filter(filter_fn(|meta| {
        meta.target() == TARGET || (meta.is_span() && meta.name() == "rds.conn")
    }));
    let diagnostics = layer
        // Per-layer filters also filter event_scope(). Retain the numeric
        // connection span even when its INFO level is below the requested
        // diagnostic threshold; otherwise WARN records lose session context.
        .with_filter(config.filter.or(filter_fn(|meta| {
            meta.is_span() && meta.name() == "rds.conn"
        })))
        .with_filter(filter_fn(|meta| meta.target() != TARGET));
    Ok((
        tracing_subscriber::registry()
            .with(telemetry_layer)
            .with(diagnostics),
        Telemetry {
            output,
            format: config.format,
        },
    ))
}

impl Telemetry {
    /// Wait until prior writes finish, then hold console output for an owned
    /// terminal UI. Drop the guard before finishing/shutting down telemetry.
    pub async fn pause_console(&self) -> std::io::Result<ConsolePause> {
        self.output.pause().await
    }

    pub fn health(&self) -> Health {
        self.output.health()
    }

    /// Observe output loss independently of the potentially failing log sink.
    pub fn health_observer(&self) -> HealthObserver {
        self.output.health_observer()
    }

    /// This heartbeat means the main future is being polled, not readiness or
    /// proof that a remote device is usable. The product owns readiness events.
    pub async fn run<T, E>(&self, future: impl Future<Output = Result<T, E>>) -> Result<T, E> {
        emit(Event::ProcessStarted);
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(15),
            Duration::from_secs(15),
        );
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::pin!(future);
        let result = loop {
            tokio::select! {
                biased;
                result = &mut future => break result,
                _ = tick.tick() => emit(Event::Heartbeat),
            }
        };
        emit(if result.is_ok() {
            Event::ProcessCompleted
        } else {
            Event::ProcessFailed
        });
        result
    }

    pub fn shutdown(self) -> Shutdown {
        self.output.shutdown()
    }

    /// Finish a binary entrypoint, preserving its result/exit status while
    /// enforcing output privacy. Best-effort output loss does not fail product
    /// work; callers needing the local drain receipt can use `shutdown` instead.
    pub fn finish<E: std::fmt::Display>(self, result: Result<(), E>) -> std::process::ExitCode {
        let status = if let Err(error) = result {
            if self.format == Format::Text {
                // Same bounded adapter, including the final cause chain. JSON
                // never formats it: process_failed already records the outcome.
                tracing::error!(target: "rds_telemetry", event = "terminal_failure",
                    error = %format_args!("{error:#}"));
            }
            std::process::ExitCode::FAILURE
        } else {
            std::process::ExitCode::SUCCESS
        };
        self.shutdown();
        status
    }
}

/// Stable, low-cardinality events. Never accept caller-supplied text here.
#[derive(Clone, Copy, Debug)]
pub enum Event {
    ProcessStarted,
    ProcessCompleted,
    ProcessFailed,
    Heartbeat,
    ListenerReady,
    PeerAccepted,
    PeerRejected,
    HandshakeFailed,
    HandshakeTimedOut,
    ConnectionBudgetExhausted,
    /// A `rds.conn` span opened: emitted once per accepted session.
    SessionOpened,
    /// The connection's observed data path moved to a different route.
    PathMigrated,
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Self::ProcessStarted => "process_started",
            Self::ProcessCompleted => "process_completed",
            Self::ProcessFailed => "process_failed",
            Self::Heartbeat => "heartbeat",
            Self::ListenerReady => "listener_ready",
            Self::PeerAccepted => "peer_accepted",
            Self::PeerRejected => "peer_rejected",
            Self::HandshakeFailed => "handshake_failed",
            Self::HandshakeTimedOut => "handshake_timed_out",
            Self::ConnectionBudgetExhausted => "connection_budget_exhausted",
            Self::SessionOpened => "session_opened",
            Self::PathMigrated => "path_migrated",
        }
    }
}

/// Typed reason codes for session close and refused work — the export-safe
/// counterpart to free-form error strings, which stay in text mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Finished cleanly.
    Completed,
    /// Work dropped before resolution (guard/cancellation path).
    Cancelled,
    /// A deadline fired (hello, handshake, authorization, dial).
    Timeout,
    /// Policy, allowlist, scope or grant verification refusal.
    Denied,
    /// Grant or lease expiry.
    Expired,
    /// Grant revoked since issuance.
    Revoked,
    /// Capacity refusal (connection/stream/service budgets).
    BudgetExhausted,
    /// Peer closed the association at the application level.
    PeerClosed,
    /// This side closed the association.
    LocalClosed,
    /// Stateless reset observed on the path.
    Reset,
    /// Transport-level failure or transport close frame.
    Transport,
    /// Wire/protocol violation or unexpected message.
    Protocol,
    /// Requested capability is not supported here.
    Unsupported,
    /// Required policy material (e.g. revocation list) unavailable or stale.
    PolicyUnavailable,
    /// Session ended without a reasoned close (error propagation path).
    Aborted,
}

impl Reason {
    fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
            Self::BudgetExhausted => "budget_exhausted",
            Self::PeerClosed => "peer_closed",
            Self::LocalClosed => "local_closed",
            Self::Reset => "reset",
            Self::Transport => "transport",
            Self::Protocol => "protocol",
            Self::Unsupported => "unsupported",
            Self::PolicyUnavailable => "policy_unavailable",
            Self::Aborted => "aborted",
        }
    }
}

/// Monotonic per-process session ids for `rds.conn` spans and session
/// boundary events. The id is scoped to `run_id` — it is never a peer key
/// or a distributed trace id. Both the accepting agent and the dialing
/// side mint from the same counter convention.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn next_session_id() -> u64 {
    SESSION_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// The one span shape that correlates a session's events on this host.
/// `peer` stays declared-but-private: JSON records carry only `session_id`.
pub fn conn_span(session_id: u64) -> tracing::Span {
    tracing::info_span!("rds.conn", session_id, peer = tracing::field::Empty)
}

/// Emit once when a session begins (post-handshake on the agent; after the
/// dial/authorization sequence on the dialing side). Must run inside the
/// session's `rds.conn` span so the record carries `session_id`.
pub fn session_opened() {
    emit(Event::SessionOpened);
}

/// Emit once when the session ends. `elapsed` is the session's lifetime —
/// the remaining W2.8 stage timings arrive via `operation_completed`.
pub fn session_closed(reason: Reason, elapsed: Duration) {
    tracing::info!(target: "rds_telemetry", event = "session_closed",
        reason = reason.name(), elapsed_us = micros(elapsed));
}

/// An inbound handshake produced no connection. `code` is the QUIC transport
/// error code when the cause has one; only text mode shows it, and the JSON
/// export keeps the typed reason.
pub fn handshake_failed(reason: Reason, code: Option<u64>) {
    match code {
        Some(code) => tracing::warn!(target: "rds_telemetry", event = "handshake_failed",
            reason = reason.name(), code),
        None => tracing::warn!(target: "rds_telemetry", event = "handshake_failed",
            reason = reason.name()),
    }
}

/// A request was refused before entering a service path. Emitted at the
/// decision point, inside the session span when one exists.
pub fn request_refused(reason: Reason) {
    tracing::warn!(target: "rds_telemetry", event = "request_refused",
        reason = reason.name());
}

/// Emits `session_closed` on drop inside the session's span, so the record
/// still carries `session_id` when the holder is dropped outside it; call
/// [`SessionGuard::close`] at a reasoned exit so the reason reflects the
/// decision, not the default `Aborted`.
pub struct SessionGuard {
    span: tracing::Span,
    started: Instant,
    reason: Option<Reason>,
}

impl SessionGuard {
    /// Emit `session_opened` inside `span` and arm the close emission.
    /// Pair with [`conn_span`]; keep the returned guard for the session's
    /// whole lifetime and instrument work in [`SessionGuard::span`].
    pub fn open(span: tracing::Span) -> Self {
        span.in_scope(session_opened);
        Self {
            span,
            started: Instant::now(),
            reason: None,
        }
    }

    /// The span this session's work should be instrumented under.
    pub fn span(&self) -> tracing::Span {
        self.span.clone()
    }

    /// Record the close reason without ending the session yet.
    pub fn set_reason(&mut self, reason: Reason) {
        self.reason = Some(reason);
    }

    /// End the session now with `reason`; the guard will not re-emit.
    pub fn close(mut self, reason: Reason) {
        self.reason = Some(reason);
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _enter = self.span.enter();
        session_closed(
            self.reason.unwrap_or(Reason::Aborted),
            self.started.elapsed(),
        );
    }
}

pub fn emit(event: Event) {
    let name = event.name();
    match event {
        Event::ProcessFailed => tracing::error!(target: "rds_telemetry", event = name),
        Event::PeerRejected
        | Event::HandshakeFailed
        | Event::HandshakeTimedOut
        | Event::ConnectionBudgetExhausted => {
            tracing::warn!(target: "rds_telemetry", event = name)
        }
        _ => tracing::info!(target: "rds_telemetry", event = name),
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Operation {
    Connect,
    ServiceStream,
    SshConnect,
    SshSession,
    GrantAuthorize,
    GrantRenew,
    SyncSend,
    SyncRecv,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::ServiceStream => "service_stream",
            Self::SshConnect => "ssh_connect",
            Self::SshSession => "ssh_session",
            Self::GrantAuthorize => "grant_authorize",
            Self::GrantRenew => "grant_renew",
            Self::SyncSend => "sync_send",
            Self::SyncRecv => "sync_recv",
        }
    }
}

/// Measures completion of the local operation, not remote presentation or
/// durable receipt. Dropping the future emits cancelled, never success.
pub async fn observe<T, E>(
    operation: Operation,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let mut guard = OperationGuard {
        operation,
        started: Instant::now(),
        outcome: "cancelled",
    };
    let result = future.await;
    guard.outcome = if result.is_ok() { "ok" } else { "error" };
    result
}

struct OperationGuard {
    operation: Operation,
    started: Instant,
    outcome: &'static str,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        tracing::info!(target: "rds_telemetry", event = "operation_completed",
            operation = self.operation.name(), outcome = self.outcome,
            elapsed_us = micros(self.started.elapsed()));
    }
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

struct Shared {
    service: &'static str,
    run_id: String,
    sequence: AtomicU64,
    started: Instant,
    sink: output::Sink,
    format: Format,
}

#[derive(Clone)]
struct RecordLayer {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Session(Option<u64>);

impl Visit for Session {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "session_id" {
            self.0 = Some(value);
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

#[derive(Default, Serialize)]
struct SafeFields {
    event: Option<&'static str>,
    operation: Option<&'static str>,
    outcome: Option<&'static str>,
    reason: Option<&'static str>,
    elapsed_us: Option<u64>,
}

impl Visit for SafeFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        // Map to constants instead of copying arbitrary data under an accepted
        // field name. Even a third-party event using our target is constrained.
        match (field.name(), value) {
            ("event", "process_started") => self.event = Some("process_started"),
            ("event", "process_completed") => self.event = Some("process_completed"),
            ("event", "process_failed") => self.event = Some("process_failed"),
            ("event", "heartbeat") => self.event = Some("heartbeat"),
            ("event", "listener_ready") => self.event = Some("listener_ready"),
            ("event", "peer_accepted") => self.event = Some("peer_accepted"),
            ("event", "peer_rejected") => self.event = Some("peer_rejected"),
            ("event", "handshake_failed") => self.event = Some("handshake_failed"),
            ("event", "handshake_timed_out") => self.event = Some("handshake_timed_out"),
            ("event", "connection_budget_exhausted") => {
                self.event = Some("connection_budget_exhausted")
            }
            ("event", "operation_completed") => self.event = Some("operation_completed"),
            ("event", "session_opened") => self.event = Some("session_opened"),
            ("event", "session_closed") => self.event = Some("session_closed"),
            ("event", "path_migrated") => self.event = Some("path_migrated"),
            ("event", "request_refused") => self.event = Some("request_refused"),
            ("operation", "connect") => self.operation = Some("connect"),
            ("operation", "service_stream") => self.operation = Some("service_stream"),
            ("operation", "ssh_connect") => self.operation = Some("ssh_connect"),
            ("operation", "ssh_session") => self.operation = Some("ssh_session"),
            ("operation", "grant_authorize") => self.operation = Some("grant_authorize"),
            ("operation", "grant_renew") => self.operation = Some("grant_renew"),
            ("operation", "sync_send") => self.operation = Some("sync_send"),
            ("operation", "sync_recv") => self.operation = Some("sync_recv"),
            ("outcome", "ok") => self.outcome = Some("ok"),
            ("outcome", "error") => self.outcome = Some("error"),
            ("outcome", "cancelled") => self.outcome = Some("cancelled"),
            ("reason", "completed") => self.reason = Some("completed"),
            ("reason", "cancelled") => self.reason = Some("cancelled"),
            ("reason", "timeout") => self.reason = Some("timeout"),
            ("reason", "denied") => self.reason = Some("denied"),
            ("reason", "expired") => self.reason = Some("expired"),
            ("reason", "revoked") => self.reason = Some("revoked"),
            ("reason", "budget_exhausted") => self.reason = Some("budget_exhausted"),
            ("reason", "peer_closed") => self.reason = Some("peer_closed"),
            ("reason", "local_closed") => self.reason = Some("local_closed"),
            ("reason", "reset") => self.reason = Some("reset"),
            ("reason", "transport") => self.reason = Some("transport"),
            ("reason", "protocol") => self.reason = Some("protocol"),
            ("reason", "unsupported") => self.reason = Some("unsupported"),
            ("reason", "policy_unavailable") => self.reason = Some("policy_unavailable"),
            ("reason", "aborted") => self.reason = Some("aborted"),
            _ => {}
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "elapsed_us" {
            self.elapsed_us = Some(value);
        }
    }
    // Do not even format Debug/Display: these can allocate, expose secrets or
    // run arbitrary formatting code. JSON uses only typed allowlisted fields.
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

#[derive(Serialize)]
struct Record<'a> {
    schema_version: u8,
    timestamp_unix_us: u64,
    uptime_us: u64,
    service: &'static str,
    version: &'static str,
    run_id: &'a str,
    sequence: u64,
    level: &'a str,
    target: &'a str,
    line: Option<u32>,
    session_id: Option<u64>,
    #[serde(flatten)]
    fields: SafeFields,
    #[serde(flatten)]
    health: Health,
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for RecordLayer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if attrs.metadata().name() == "rds.conn"
            && let Some(span) = ctx.span(id)
        {
            let mut session = Session::default();
            attrs.record(&mut session);
            span.extensions_mut().replace(session);
        }
    }

    fn on_event(&self, event: &TraceEvent<'_>, ctx: Context<'_, S>) {
        let shared = &self.shared;
        let meta = event.metadata();
        let session_id = ctx.event_scope(event).and_then(|scope| {
            scope
                .filter_map(|span| span.extensions().get::<Session>().and_then(|s| s.0))
                .next()
        });
        let mut fields = SafeFields::default();
        if meta.target() == TARGET {
            event.record(&mut fields);
        } else {
            fields.event = Some("diagnostic");
        }
        let record = Record {
            schema_version: 1,
            timestamp_unix_us: micros(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default(),
            ),
            uptime_us: micros(shared.started.elapsed()),
            service: shared.service,
            version: env!("CARGO_PKG_VERSION"),
            run_id: &shared.run_id,
            sequence: shared.sequence.fetch_add(1, Ordering::Relaxed),
            level: meta.level().as_str(),
            target: meta.target(),
            line: meta.line(),
            session_id,
            fields,
            health: shared.sink.health(),
        };
        let mut buffer = Buffer::default();
        let formatted = match shared.format {
            Format::Json => serde_json::to_writer(&mut buffer, &record).is_ok(),
            Format::Text => {
                use std::fmt::Write as _;
                let mut ok = write!(
                    &mut buffer,
                    "{} {} {} session={:?} ",
                    record.timestamp_unix_us, record.level, record.target, record.session_id
                )
                .is_ok();
                let mut visitor = TextFields {
                    buffer: &mut buffer,
                    ok: true,
                };
                event.record(&mut visitor);
                ok &= visitor.ok;
                ok
            }
        };
        if !formatted || std::io::Write::write_all(&mut buffer, b"\n").is_err() {
            shared.sink.oversize();
            return;
        }
        shared.sink.send(buffer);
    }
}

struct TextFields<'a> {
    buffer: &'a mut Buffer,
    ok: bool,
}

impl Visit for TextFields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if self.ok {
            self.ok = write!(self.buffer, "{}={:?} ", field.name(), value).is_ok();
        }
    }
}

#[cfg(test)]
mod tests;
