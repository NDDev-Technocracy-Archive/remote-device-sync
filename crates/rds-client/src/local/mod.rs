//! Agent-owned outgoing sessions, accessed through a same-UID Unix socket.
//! The manager borrows the agent endpoint; it never reads identity material.
mod socket;
mod state;
mod sync;
mod tcp;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rds_core::local::{Command, ErrorCode, Reply, Request, Response, SessionId, VERSION};
use rds_net::{Connection, Endpoint};
use rds_net::{read_frame, write_frame};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::request::RequestStreams;
pub use state::Observer;
pub use tcp::TcpStream;

const PRELUDE_TIMEOUT: Duration = Duration::from_secs(5);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(45);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(55);
const MAX_WORKERS: usize = 96;
const MAX_STREAMS: usize = 64;
const SYNC_TIMEOUT: Duration = rds_sync::engine::TRANSFER_TIMEOUT.saturating_add(OPERATION_TIMEOUT);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("local IPC I/O failed")]
    Io(#[from] std::io::Error),
    #[error(
        "control directory must be an absolute, private owner directory without symlinks or untrusted ancestors"
    )]
    UnsafeDirectory,
    #[error("unsafe control socket")]
    UnsafeSocket,
    #[error("control directory already has a running owner")]
    AlreadyRunning,
    #[error("local peer UID does not match the process owner")]
    PeerIdentity,
    #[error(transparent)]
    Rejected(#[from] ErrorCode),
    #[error("local manager worker failed")]
    Worker(#[source] tokio::task::JoinError),
    #[error("local manager stopped with an error")]
    Service(#[source] Arc<Error>),
    #[error("unexpected local response")]
    Protocol,
}

/// Dedicated control directory beside a key, without reading or creating it.
/// Explicit control paths still take precedence. The resulting path must pass
/// the same owner/ancestor checks as any other local socket directory.
pub fn control_dir_for_key(key: &Path) -> Result<PathBuf, Error> {
    let key = std::path::absolute(key)?;
    let mut name = key
        .file_name()
        .ok_or(Error::UnsafeDirectory)?
        .to_os_string();
    name.push(".control");
    Ok(key.with_file_name(name))
}

/// Bind and validate before loading the agent's key or contacting the network.
pub struct Prepared {
    listener: UnixListener,
    _owner: socket::SocketOwner,
}

impl Prepared {
    pub async fn bind(directory: impl AsRef<Path>) -> Result<Self, Error> {
        let (owner, listener) = socket::SocketOwner::bind(directory.as_ref().to_path_buf()).await?;
        Ok(Self {
            listener,
            _owner: owner,
        })
    }
}

/// Owns the runner, requests, streams and outgoing connections. Does not own
/// endpoint shutdown: inbound agent service uses that same endpoint.
pub struct Server {
    stop: CancellationToken,
    task: Option<JoinHandle<Result<(), Error>>>,
    outcome: Option<Result<(), Arc<Error>>>,
    observer: Option<Observer>,
    enabled: bool,
}

impl Server {
    pub fn start(
        prepared: Option<Prepared>,
        endpoint: Endpoint,
        directory: Option<rds_discovery::client::Client>,
    ) -> Self {
        let stop = CancellationToken::new();
        let enabled = prepared.is_some();
        let (task, observer) = if let Some(prepared) = prepared {
            let shared = state::State::new(endpoint.id());
            let observer = Observer::new(&shared);
            let owner = state::Owner(shared);
            (
                Some(tokio::spawn(run(
                    prepared,
                    owner,
                    endpoint,
                    directory,
                    stop.clone(),
                ))),
                Some(observer),
            )
        } else {
            (None, None)
        };
        Self {
            stop,
            task,
            outcome: None,
            observer,
            enabled,
        }
    }

    pub fn take_observer(&mut self) -> Option<Observer> {
        self.observer.take()
    }

    async fn join(&mut self) -> Result<(), Error> {
        if let Some(task) = self.task.as_mut() {
            let result = match task.await {
                Ok(result) => result,
                Err(e) => Err(Error::Worker(e)),
            };
            self.outcome = Some(result.map_err(Arc::new));
            self.task = None;
        }
        self.outcome
            .as_ref()
            .cloned()
            .unwrap_or(Ok(()))
            .map_err(Error::Service)
    }

    pub async fn stopped(&mut self) -> Result<(), Error> {
        if !self.enabled {
            return std::future::pending().await;
        }
        self.join().await
    }

    /// Canceling this waiter does not cancel ownership or lose the result.
    pub async fn close(&mut self) -> Result<(), Error> {
        self.stop.cancel();
        self.join().await
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn run(
    prepared: Prepared,
    owner: state::Owner,
    endpoint: Endpoint,
    directory: Option<rds_discovery::client::Client>,
    stop: CancellationToken,
) -> Result<(), Error> {
    let mut workers = JoinSet::new();
    let streams = Arc::new(Semaphore::new(MAX_STREAMS));
    let transfers = Arc::new(Semaphore::new(sync::MAX_TRANSFERS));
    let mut sample = tokio::time::interval(Duration::from_secs(1));
    sample.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break Ok(()),
            result = workers.join_next(), if !workers.is_empty() => {
                if let Some(Err(e)) = result { break Err(Error::Worker(e)); }
            }
            _ = sample.tick() => {
                match state::lock(&owner.0) {
                    Ok(mut state) => for entry in state.entries.values_mut() {
                        if let Some(sampler) = &mut entry.sampler { sampler.sample(); }
                    },
                    Err(e) => break Err(e.into()),
                }
            }
            accepted = prepared.listener.accept(), if workers.len() < MAX_WORKERS => {
                let (stream, _) = match accepted { Ok(pair) => pair, Err(e) => break Err(e.into()) };
                if socket::authenticate(&stream).is_err() { continue; }
                let shared = owner.0.clone();
                let endpoint = endpoint.clone();
                let directory = directory.clone();
                let streams = streams.clone();
                let transfers = transfers.clone();
                workers.spawn(async move {
                    // Responses carry typed reasons; never log the request or
                    // an upstream error containing a ticket/grant/private path.
                    let _ = serve(stream, shared, endpoint, directory, streams, transfers).await;
                });
            }
        }
    };
    drop(owner); // Seal reservations and close all QUIC connections first.
    workers.shutdown().await;
    drop(prepared);
    result
}

mod desktop;

pub use desktop::{ManagedControl, ManagedDesktop, ManagedMessage, RelayedFrame};

struct Output {
    reply: Reply,
    reservation: Option<state::Reservation>,
    tcp: Option<(RequestStreams, OwnedSemaphorePermit)>,
    desktop: Option<(rds_desktop::client::DesktopSession, OwnedSemaphorePermit)>,
}

impl Output {
    fn reply(reply: Reply) -> Self {
        Self {
            reply,
            reservation: None,
            tcp: None,
            desktop: None,
        }
    }
}

async fn serve(
    mut stream: UnixStream,
    shared: state::Shared,
    endpoint: Endpoint,
    directory: Option<rds_discovery::client::Client>,
    streams: Arc<Semaphore>,
    transfers: Arc<Semaphore>,
) -> Result<(), Error> {
    let request: Request = tokio::time::timeout(PRELUDE_TIMEOUT, read_frame(&mut stream))
        .await
        .map_err(|_| ErrorCode::Timeout)??;
    let mut probe = [0];
    let timeout = if matches!(request.command, Command::Sync { .. }) {
        SYNC_TIMEOUT
    } else {
        OPERATION_TIMEOUT
    };
    let result = if request.version != VERSION {
        Err(ErrorCode::Version)
    } else {
        // EOF means the caller canceled; extra request bytes are a protocol
        // error. TCP clients must wait for Opened before sending their body.
        tokio::select! {
            biased;
            _ = stream.read(&mut probe) => return Ok(()),
            result = tokio::time::timeout(timeout, execute(request.command, &shared, &endpoint, directory, streams, transfers)) => result.unwrap_or(Err(ErrorCode::Timeout)),
        }
    };
    let response = Response {
        version: VERSION,
        result: result.as_ref().map(|o| o.reply.clone()).map_err(|e| *e),
    };
    tokio::time::timeout(PRELUDE_TIMEOUT, write_frame(&mut stream, &response))
        .await
        .map_err(|_| ErrorCode::Timeout)??;
    if let Ok(mut output) = result {
        if let Some(reservation) = &mut output.reservation {
            reservation.commit()?;
        }
        if let Some((mut tcp, _permit)) = output.tcp {
            let (send, recv) = tcp.get_mut();
            tcp::serve(&mut stream, send, recv).await?;
            // Both explicit byte directions finished. Preserve buffered QUIC
            // data/FIN instead of resetting a successfully completed upload.
            drop(tcp.release());
        }
        if let Some((session, _permit)) = output.desktop {
            desktop::serve(&mut stream, session).await?;
        }
    }
    Ok(())
}

struct DialGuard(Option<Connection>);
impl Drop for DialGuard {
    fn drop(&mut self) {
        if let Some(conn) = &self.0 {
            conn.close(0u32.into(), b"local dial canceled");
        }
    }
}

async fn execute(
    command: Command,
    shared: &state::Shared,
    endpoint: &Endpoint,
    directory: Option<rds_discovery::client::Client>,
    streams: Arc<Semaphore>,
    transfers: Arc<Semaphore>,
) -> Result<Output, ErrorCode> {
    match command {
        Command::Sync { session, operation } => {
            let observed = match operation {
                rds_core::local::SyncOperation::Send { .. } => rds_observe::Operation::SyncSend,
                rds_core::local::SyncOperation::Recv { .. } => rds_observe::Operation::SyncRecv,
            };
            rds_observe::observe(observed, sync::run(shared, session, operation, transfers))
                .await
                .map(Output::reply)
        }
        Command::Ticket => Ok(Output::reply(Reply::Ticket(
            rds_net::Ticket::of(endpoint).to_string(),
        ))),
        Command::List => Ok(Output::reply(Reply::Snapshot(
            state::lock(shared)?.snapshot(),
        ))),
        Command::Connect { target, grant } => {
            if target.len() > 8192 {
                return Err(ErrorCode::InvalidRequest);
            }
            let addr = rds_net::resolve_target(directory, &target)
                .await
                .map_err(|_| ErrorCode::Resolve)?;
            if addr.id == endpoint.id() {
                return Err(ErrorCode::InvalidRequest);
            }
            let credential = grant
                .as_ref()
                .map(|g| postcard::to_stdvec(g).map(|b| *blake3::hash(&b).as_bytes()))
                .transpose()
                .map_err(|_| ErrorCode::InvalidRequest)?;
            let reservation = match state::reserve(shared, addr.id, credential)? {
                state::Reserved::Existing(id) => return Ok(Output::reply(Reply::Connected(id))),
                state::Reserved::New(reservation) => reservation,
            };
            let conn = tokio::select! {
                biased;
                _ = reservation.cancel.cancelled() => return Err(ErrorCode::NotFound),
                result = async {
                    let conn = match grant {
                        Some(grant) => crate::connect_authorized(endpoint, addr, &grant).await,
                        None => crate::connect(endpoint, addr).await,
                    }.map_err(|_| ErrorCode::Connect)?;
                    let guard = DialGuard(Some(conn));
                    // Authz validates managed admission; plain allowlist mode
                    // needs one service reply to establish application readiness.
                    if credential.is_none() {
                        crate::ping(guard.0.as_ref().ok_or(ErrorCode::Internal)?, rand::random()).await.map_err(|_| ErrorCode::Connect)?;
                    }
                    Ok::<_, ErrorCode>(guard)
                } => result?,
            };
            let mut guard = conn;
            let mut state = state::lock(shared)?;
            let entry = state
                .entries
                .get_mut(&reservation.id)
                .ok_or(ErrorCode::NotFound)?;
            let conn = guard.0.take().ok_or(ErrorCode::Internal)?;
            entry.sampler = Some(endpoint.metrics().sampler(conn.clone()));
            entry.conn = Some(conn);
            if state.selected.is_none() {
                state.selected = Some(reservation.id);
            }
            state.changed();
            drop(state);
            Ok(Output {
                reply: Reply::Connected(reservation.id),
                reservation: Some(reservation),
                tcp: None,
                desktop: None,
            })
        }
        Command::Renew { session, grant } => {
            let credential =
                *blake3::hash(&postcard::to_stdvec(&grant).map_err(|_| ErrorCode::InvalidRequest)?)
                    .as_bytes();
            let (reservation, conn) = state::renew(shared, session)?;
            tokio::select! {
                biased;
                _ = reservation.cancel.cancelled() => return Err(ErrorCode::NotFound),
                result = crate::renew_authorization(&conn, &grant) => result.map_err(|_| ErrorCode::Remote)?,
            }
            let mut state = state::lock(shared)?;
            let entry = state.entries.get_mut(&session).ok_or(ErrorCode::NotFound)?;
            entry.credential = Some(credential);
            // Keep the transaction reserved until the local success reply is
            // written. Drop/cancel/reply failure removes this same pinned session.
            drop(state);
            Ok(Output {
                reply: Reply::Done,
                reservation: Some(reservation),
                tcp: None,
                desktop: None,
            })
        }
        Command::Select { session } => {
            let mut state = state::lock(shared)?;
            state.connection(Some(session))?;
            if state.selected != Some(session) {
                state.selected = Some(session);
                state.changed();
            }
            Ok(Output::reply(Reply::Done))
        }
        Command::Disconnect { session } => {
            if !state::lock(shared)?.remove(session) {
                return Err(ErrorCode::NotFound);
            }
            Ok(Output::reply(Reply::Done))
        }
        Command::Ping { session, nonce } => {
            let (session, conn) = state::lock(shared)?.connection(session)?;
            let elapsed = crate::ping(&conn, nonce)
                .await
                .map_err(|_| ErrorCode::Remote)?;
            Ok(Output::reply(Reply::Pong {
                session,
                micros: elapsed.as_micros().min(u128::from(u64::MAX)) as u64,
            }))
        }
        Command::Info { session } => {
            let (session, conn) = state::lock(shared)?.connection(session)?;
            let info = crate::info(&conn).await.map_err(|_| ErrorCode::Remote)?;
            Ok(Output::reply(Reply::Info { session, info }))
        }
        Command::OpenTcp { session, target } => {
            let permit = streams
                .try_acquire_owned()
                .map_err(|_| ErrorCode::Capacity)?;
            let (session, conn) = state::lock(shared)?.connection(session)?;
            let (host, port) = target.into_parts();
            let pair = crate::open_tcp(&conn, &host, port)
                .await
                .map_err(|_| ErrorCode::Remote)?;
            Ok(Output {
                reply: Reply::Opened(session),
                reservation: None,
                tcp: Some((RequestStreams::new(pair), permit)),
                desktop: None,
            })
        }
        Command::Desktop { session, hello } => {
            let permit = streams
                .try_acquire_owned()
                .map_err(|_| ErrorCode::Capacity)?;
            let (session, conn) = state::lock(shared)?.connection(session)?;
            // Relay mode publishes encoded frames for IPC forwarding; the
            // viewer decodes, so the manager never needs a codec. A fresh
            // v2 session ID keeps the frame route isolated from any other
            // desktop session on this connection.
            let remote = rds_desktop::client::DesktopSession::connect_opts(
                &conn,
                *hello,
                rds_desktop::client::SessionOpts {
                    session: Some(rand::random()),
                    relay_encoded: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| {
                tracing::warn!("managed desktop session open failed: {e:?}");
                ErrorCode::Remote
            })?;
            Ok(Output {
                reply: Reply::DesktopOpened {
                    session,
                    caps: remote.caps().clone(),
                },
                reservation: None,
                tcp: None,
                desktop: Some((remote, permit)),
            })
        }
    }
}

/// A keyless client: one authenticated local request per socket.
#[derive(Clone)]
pub struct Client {
    directory: PathBuf,
}

impl Client {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    async fn exchange(&self, command: Command) -> Result<(Reply, UnixStream), Error> {
        let body = matches!(command, Command::OpenTcp { .. } | Command::Desktop { .. });
        let timeout = if matches!(command, Command::Sync { .. }) {
            SYNC_TIMEOUT + Duration::from_secs(10)
        } else {
            CLIENT_TIMEOUT
        };
        tokio::time::timeout(timeout, async {
            let mut stream = socket::connect(&self.directory).await?;
            write_frame(
                &mut stream,
                &Request {
                    version: VERSION,
                    command,
                },
            )
            .await?;
            let response: Response = read_frame(&mut stream).await?;
            if response.version != VERSION {
                return Err(ErrorCode::Version.into());
            }
            if !body {
                // The server publishes a reserved transaction after writing
                // its reply and before closing IPC. EOF is the commit barrier;
                // a following renewal must not race a still-reserved entry.
                let mut trailing = [0];
                if stream.read(&mut trailing).await? != 0 {
                    return Err(Error::Protocol);
                }
            }
            Ok((response.result?, stream))
        })
        .await
        .map_err(|_| ErrorCode::Timeout)?
    }

    pub async fn request(&self, command: Command) -> Result<Reply, Error> {
        if matches!(command, Command::OpenTcp { .. } | Command::Desktop { .. }) {
            return Err(Error::Protocol);
        }
        self.exchange(command).await.map(|(reply, _)| reply)
    }

    /// Open a managed desktop channel: the manager runs the remote session
    /// and relays encoded frames; the caller decodes and sends controls.
    /// The returned socket speaks `DesktopDown`/`DesktopUp`.
    pub async fn desktop(
        &self,
        session: Option<SessionId>,
        hello: rds_core::DesktopHello,
    ) -> Result<ManagedDesktop, Error> {
        let display = hello.display;
        let (reply, stream) = self
            .exchange(Command::Desktop {
                session,
                hello: Box::new(hello),
            })
            .await?;
        match reply {
            Reply::DesktopOpened { session, caps } => {
                Ok(ManagedDesktop::new(stream, session, caps, display))
            }
            _ => Err(Error::Protocol),
        }
    }

    pub async fn snapshot(&self) -> Result<rds_core::local::Snapshot, Error> {
        match self.request(Command::List).await? {
            Reply::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(Error::Protocol),
        }
    }

    /// Resolve the selection once. Callers pin this handle before accepting
    /// sockets so another CLI's Select cannot redirect an existing forward.
    pub async fn selected(&self, session: Option<SessionId>) -> Result<SessionId, Error> {
        let snapshot = self.snapshot().await?;
        let id = session
            .or(snapshot.selected)
            .ok_or(ErrorCode::NoSelection)?;
        if !snapshot
            .sessions
            .iter()
            .any(|s| s.id == id && s.status == rds_core::local::Status::Connected)
        {
            return Err(ErrorCode::NotFound.into());
        }
        Ok(id)
    }

    pub async fn open_tcp(
        &self,
        session: SessionId,
        target: rds_core::TcpTarget,
    ) -> Result<TcpStream, Error> {
        let (reply, stream) = self
            .exchange(Command::OpenTcp {
                session: Some(session),
                target,
            })
            .await?;
        match reply {
            Reply::Opened(id) if id == session => Ok(TcpStream::new(stream)),
            _ => Err(Error::Protocol),
        }
    }

    /// Local listener lifetime belongs to its CLI; canceling it aborts all its
    /// workers but leaves the manager's shared peer connection intact.
    pub async fn forward(
        &self,
        session: SessionId,
        listener: TcpListener,
        target: rds_core::TcpTarget,
        limit: std::num::NonZeroU16,
    ) -> Result<(), Error> {
        let mut workers = JoinSet::new();
        let watch = async {
            loop {
                if let Err(error) = self.selected(Some(session)).await {
                    break error;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        };
        tokio::pin!(watch);
        let result = loop {
            tokio::select! {
                error = &mut watch => break Err(error),
                result = workers.join_next(), if !workers.is_empty() => {
                    if let Some(Err(e)) = result { break Err(Error::Worker(e)); }
                }
                accepted = listener.accept(), if workers.len() < usize::from(limit.get()) => {
                    let (mut local, _) = match accepted {
                        Ok(pair) => pair,
                        Err(error) => break Err(error.into()),
                    };
                    let client = self.clone();
                    let target = target.clone();
                    workers.spawn(async move {
                        match client.open_tcp(session, target).await {
                            Ok(mut remote) => {
                                let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                                remote.close().await;
                            }
                            Err(error) => tracing::warn!(reason = %error, "managed forward stream rejected"),
                        }
                    });
                }
            }
        };
        drop(listener);
        workers.shutdown().await;
        result
    }
}
