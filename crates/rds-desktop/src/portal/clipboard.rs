//! One consented portal clipboard owner, metadata notifications and scoped FD I/O.
use crate::{
    DesktopError,
    clipboard::{
        ClipboardBackend, ClipboardFuture, NativeLifetime,
        image::Png,
        native::{Error, Operation, Policy, Provider, Publication, Snapshot, Text, Value},
        ownership::{Owner, Running},
    },
};
use ashpd::desktop::{
    Session,
    clipboard::{Clipboard, SelectionOwnerChanged, SetSelectionOptions},
    remote_desktop::RemoteDesktop,
};
use ashpd::zbus::OwnedObjectPath;
use bytes::Bytes;
use futures_util::{StreamExt, stream::FuturesUnordered};
use rds_core::clipboard::{Capabilities, Format, Representation, ShortcutModifier};
use std::{
    os::fd::OwnedFd,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::{UnixStream, unix::pipe},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
};

const TEXT: &str = "text/plain;charset=utf-8";
const PNG: &str = "image/png";
fn desktop(error: Error) -> DesktopError {
    DesktopError::Input(error.to_string())
}
fn unavailable<E>(_: E) -> Error {
    Error::Unavailable
}

#[derive(Clone)]
enum Body {
    Text(Text),
    Image(Png),
}
impl Body {
    fn format(&self) -> Format {
        match self {
            Self::Text(_) => Format::TextUtf8,
            Self::Image(_) => Format::ImagePng,
        }
    }
    fn bytes(&self) -> &Bytes {
        match self {
            Self::Text(value) => value.encoded(),
            Self::Image(value) => value.encoded(),
        }
    }
    fn value(&self) -> Value {
        match self {
            Self::Text(value) => Value::Text(value.clone()),
            Self::Image(value) => Value::Image(value.clone()),
        }
    }
    fn mime_types(&self) -> &[&str] {
        match self {
            Self::Text(_) => &[TEXT, "text/plain"],
            Self::Image(_) => &[PNG],
        }
    }
}
#[derive(Clone)]
struct State {
    snapshot: Snapshot,
    text_type: Option<&'static str>,
    active: bool,
    closed: bool,
}
impl State {
    fn empty() -> Self {
        Self {
            snapshot: Snapshot {
                generation: 0,
                owned: false,
                representations: Vec::new(),
            },
            text_type: None,
            active: false,
            closed: false,
        }
    }
    fn advance(&mut self) -> Result<(), Error> {
        self.snapshot.generation = self
            .snapshot
            .generation
            .checked_add(1)
            .ok_or(Error::Unavailable)?;
        Ok(())
    }
    fn published(&mut self, body: &Body) -> Result<Snapshot, Error> {
        self.advance()?;
        self.snapshot.owned = true;
        self.snapshot.representations = vec![Representation {
            format: body.format(),
            bytes: Some(body.bytes().len() as u64),
        }];
        self.text_type = (body.format() == Format::TextUtf8).then_some(TEXT);
        Ok(self.snapshot.clone())
    }
    fn changed(
        &mut self,
        changed: &SelectionOwnerChanged,
        body: &mut Option<Body>,
        pending: &mut Option<Body>,
        policy: &Policy,
    ) -> Result<(), Error> {
        if changed.session_is_owner() == Some(true) {
            if let Some(value) = pending.take() {
                *body = Some(value);
            }
            if self.snapshot.owned && body.is_some() {
                return Ok(());
            }
            self.advance()?;
            self.snapshot.owned = true;
            self.text_type = None;
            self.snapshot.representations = body
                .as_ref()
                .map(|value| {
                    vec![Representation {
                        format: value.format(),
                        bytes: Some(value.bytes().len() as u64),
                    }]
                })
                .unwrap_or_default();
            return Ok(());
        }
        self.advance()?;
        self.snapshot.owned = false;
        self.snapshot.representations.clear();
        self.text_type = None;
        *body = None;
        if changed.session_is_owner() != Some(false)
            || changed.mime_types().len() > policy.native_types
        {
            return Ok(());
        }
        self.text_type = [TEXT, "text/plain", "UTF8_STRING"]
            .into_iter()
            .find(|wanted| changed.mime_types().iter().any(|mime| mime == wanted));
        if self.text_type.is_some() {
            self.snapshot.representations.push(Representation {
                format: Format::TextUtf8,
                bytes: None,
            });
        }
        if changed.mime_types().iter().any(|mime| mime == PNG) {
            self.snapshot.representations.push(Representation {
                format: Format::ImagePng,
                bytes: None,
            });
        }
        Ok(())
    }
}
struct Guard {
    _original: NativeLifetime,
    _body: Arc<OwnedSemaphorePermit>,
    // Drain acknowledgement follows descriptor, purpose and permit cleanup.
    running: Option<Running>,
}
#[derive(Default)]
struct Bodies(Mutex<(usize, Weak<Semaphore>)>);
impl Bodies {
    fn get(&self, maximum: usize) -> Result<Arc<Semaphore>, Error> {
        let mut pool = self.0.lock().map_err(unavailable)?;
        if let Some(bodies) = pool.1.upgrade() {
            return if pool.0 == maximum {
                Ok(bodies)
            } else {
                Err(Error::Settings)
            };
        }
        let bodies = Arc::new(Semaphore::new(maximum));
        *pool = (maximum, Arc::downgrade(&bodies));
        Ok(bodies)
    }
}
// Field order closes the descriptor before releasing purpose/body admission.
struct NativeIo {
    fd: Descriptor,
    _held: Guard,
}
enum Direction {
    Read,
    Write,
}
enum Descriptor {
    Reader(pipe::Receiver),
    Writer(pipe::Sender),
    Socket(UnixStream),
}
impl Descriptor {
    async fn read(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Reader(fd) => {
                fd.readable().await?;
                fd.try_read(buffer)
            }
            Self::Socket(fd) => {
                fd.readable().await?;
                fd.try_read(buffer)
            }
            Self::Writer(_) => Err(std::io::ErrorKind::InvalidInput.into()),
        }
    }
    async fn write(&self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Writer(fd) => {
                fd.writable().await?;
                fd.try_write(buffer)
            }
            Self::Socket(fd) => {
                fd.writable().await?;
                fd.try_write(buffer)
            }
            Self::Reader(_) => Err(std::io::ErrorKind::InvalidInput.into()),
        }
    }
}
impl NativeIo {
    fn new(fd: OwnedFd, held: Guard, direction: Direction) -> Result<Self, Error> {
        struct Opening {
            fd: OwnedFd,
            held: Guard,
        }
        let opening = Opening { fd, held };
        let stat = rustix::fs::fstat(&opening.fd).map_err(std::io::Error::from)?;
        let fd = match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
            rustix::fs::FileType::Fifo => match direction {
                Direction::Read => Descriptor::Reader(pipe::Receiver::from_owned_fd(opening.fd)?),
                Direction::Write => Descriptor::Writer(pipe::Sender::from_owned_fd(opening.fd)?),
            },
            rustix::fs::FileType::Socket => {
                let stream = std::os::unix::net::UnixStream::from(opening.fd);
                stream.set_nonblocking(true)?;
                Descriptor::Socket(UnixStream::from_std(stream)?)
            }
            _ => return Err(Error::Unsupported),
        };
        Ok(Self {
            fd,
            _held: opening.held,
        })
    }
    async fn read(&self, limit: usize, policy: &Policy) -> Result<Bytes, Error> {
        let reading = async {
            let mut bytes = Vec::new();
            let mut buffer = vec![0; policy.portal.selection_chunk_bytes as usize];
            loop {
                match self.fd.read(&mut buffer).await {
                    Ok(0) => return Ok(Bytes::from(bytes)),
                    Ok(n) => {
                        if bytes.len().saturating_add(n) > limit {
                            return Err(Error::Contract(rds_core::clipboard::Error::TooLarge));
                        }
                        let needed = bytes.len() + n;
                        if needed > bytes.capacity() {
                            let capacity =
                                bytes.capacity().saturating_mul(2).max(needed).min(limit);
                            bytes
                                .try_reserve_exact(capacity - bytes.len())
                                .map_err(unavailable)?;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        };
        tokio::time::timeout(Duration::from_millis(policy.operation_timeout_ms), reading)
            .await
            .map_err(unavailable)?
    }
    async fn write(&self, body: &Body, policy: &Policy) -> Result<(), Error> {
        let writing = async {
            let mut bytes = body.bytes().as_ref();
            while !bytes.is_empty() {
                let chunk = &bytes[..bytes
                    .len()
                    .min(policy.portal.selection_chunk_bytes as usize)];
                match self.fd.write(chunk).await {
                    Ok(0) => return Err(Error::Unavailable),
                    Ok(n) => bytes = &bytes[n..],
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        };
        tokio::time::timeout(Duration::from_millis(policy.operation_timeout_ms), writing)
            .await
            .map_err(unavailable)?
    }
}
enum Command {
    Read {
        generation: u64,
        format: Format,
        reply: oneshot::Sender<Result<Value, Error>>,
    },
    Publish {
        generation: u64,
        body: Body,
        reply: oneshot::Sender<Result<Snapshot, Error>>,
    },
}
struct Job {
    command: Command,
    held: Guard,
}
enum Completed {
    Read {
        generation: u64,
        reply: oneshot::Sender<Result<Value, Error>>,
        value: Result<Value, Error>,
        _io: NativeIo,
    },
    Write,
}
type Pending = Pin<Box<dyn Future<Output = Completed> + Send>>;
struct Core {
    jobs: mpsc::Sender<Job>,
    changes: watch::Receiver<State>,
    bodies: Arc<Semaphore>,
    policy: Policy,
    live: Arc<AtomicBool>,
    activate: Mutex<Option<oneshot::Sender<()>>>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    finished: watch::Receiver<bool>,
}
#[derive(Clone)]
pub(super) struct Group(Arc<Core>);
impl Group {
    /// Subscribe before Start. No clipboard method runs until local activation;
    /// directed owner signals emitted during Start remain in these streams.
    pub(super) async fn prepare(
        proxy: Clipboard,
        session: Arc<Session<RemoteDesktop>>,
        policy: Policy,
    ) -> Result<Self, DesktopError> {
        policy.validate().map_err(desktop)?;
        let proxy = Arc::new(proxy);
        let path =
            serde_json::to_value(session.as_ref()).map_err(|_| desktop(Error::Unavailable))?;
        let path = path
            .as_str()
            .ok_or_else(|| desktop(Error::Unavailable))?
            .to_owned();
        let timeout = Duration::from_millis(policy.operation_timeout_ms);
        let owners = tokio::time::timeout(timeout, proxy.receive_signal("SelectionOwnerChanged"))
            .await
            .map_err(|_| desktop(Error::Unavailable))?
            .map_err(|_| desktop(Error::Unavailable))?;
        let transfers = tokio::time::timeout(timeout, proxy.receive_signal("SelectionTransfer"))
            .await
            .map_err(|_| desktop(Error::Unavailable))?
            .map_err(|_| desktop(Error::Unavailable))?;
        let (jobs, mut requests) = mpsc::channel::<Job>(policy.pending_operations);
        let (changes, receiver) = watch::channel(State::empty());
        let (activation, activated) = oneshot::channel();
        let (stop, stopped) = oneshot::channel();
        let (finished, joined) = watch::channel(false);
        let live = Arc::new(AtomicBool::new(false));
        // Outstanding native calls retain the same semaphore across a physical
        // portal owner replacement; a fresh session cannot reset this budget.
        static BODIES: OnceLock<Bodies> = OnceLock::new();
        let bodies = BODIES
            .get_or_init(Bodies::default)
            .get(policy.native_body_operations)
            .map_err(desktop)?;
        let worker_bodies = bodies.clone();
        let worker_policy = policy.clone();
        let worker_live = live.clone();
        tokio::spawn(async move {
            struct Seal {
                live: Arc<AtomicBool>,
                changes: watch::Sender<State>,
                finished: watch::Sender<bool>,
            }
            impl Drop for Seal {
                fn drop(&mut self) {
                    self.live.store(false, Ordering::Release);
                    self.changes.send_modify(|state| state.closed = true);
                    self.finished.send_replace(true);
                }
            }
            let seal = Seal {
                live: worker_live.clone(),
                changes,
                finished,
            };
            tokio::pin!(activated, stopped, owners, transfers);
            tokio::select! { biased; _ = &mut stopped => return, result = &mut activated => if result.is_err() { return; } }
            worker_live.store(true, Ordering::Release);
            let mut state = State::empty();
            state.active = true;
            seal.changes.send_replace(state.clone());
            let mut body: Option<Body> = None;
            let mut own_pending = None;
            let mut pending = FuturesUnordered::<Pending>::new();
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    message = owners.next() => {
                        let Some(message) = message else { break; };
                        let Ok((selected, owner)) = message.body().deserialize::<(OwnedObjectPath, SelectionOwnerChanged)>() else { continue; };
                        if selected.as_str() != path { continue; }
                        if state.changed(&owner, &mut body, &mut own_pending, &worker_policy).is_err() { break; }
                        seal.changes.send_replace(state.clone());
                    },
                    message = transfers.next() => {
                        let Some(message) = message else { break; };
                        let Ok((selected, requested, serial)) = message.body().deserialize::<(OwnedObjectPath, String, u32)>() else { continue; };
                        if selected.as_str() != path { continue; }
                        let value = body.clone().filter(|value| value.mime_types().contains(&requested.as_str()));
                        let permit = worker_bodies.clone().try_acquire_owned();
                        let (Some(value), Ok(permit)) = (value, permit) else {
                            let _ = proxy.selection_write_done(&session, serial, false).await;
                            continue;
                        };
                        let proxy = proxy.clone(); let session = session.clone(); let policy = worker_policy.clone();
                        pending.push(Box::pin(async move {
                            let admission = Arc::new(permit);
                            let held = Guard { running: None, _body: admission.clone(), _original: session.clone() };
                            let result = async {
                                let fd: OwnedFd = proxy.selection_write(&session, serial).await.map_err(unavailable)?.into();
                                let io = NativeIo::new(fd, held, Direction::Write)?;
                                io.write(&value, &policy).await
                            }.await;
                            let _ = proxy.selection_write_done(&session, serial, result.is_ok()).await;
                            drop(admission);
                            Completed::Write
                        }));
                    },
                    complete = pending.next(), if !pending.is_empty() => {
                        if let Some(Completed::Read { generation, reply, value, _io }) = complete {
                            let value = if state.snapshot.generation == generation { value } else { Err(Error::Changed) };
                            let _ = reply.send(value);
                        }
                    },
                    job = requests.recv() => {
                        let Some(job) = job else { break; };
                        if job.held.running.as_ref().is_some_and(Running::closed) { continue; }
                        match job.command {
                            Command::Read { generation, format, reply } => {
                                if reply.is_closed() { continue; }
                                if generation != state.snapshot.generation { let _ = reply.send(Err(Error::Changed)); continue; }
                                if !state.snapshot.representations.iter().any(|r| r.format == format) { let _ = reply.send(Err(Error::Unsupported)); continue; }
                                if state.snapshot.owned {
                                    let value = body.as_ref().filter(|body| body.format() == format).map(Body::value).ok_or(Error::Unsupported);
                                    let _ = reply.send(value); continue;
                                }
                                let target = match format { Format::TextUtf8 => state.text_type, Format::ImagePng => Some(PNG), Format::Files => None };
                                let Some(target) = target else { let _ = reply.send(Err(Error::Unsupported)); continue; };
                                // This retained native job outlives its async waiter's timeout.
                                let fd = match proxy.selection_read(&session, target).await {
                                    Ok(fd) => OwnedFd::from(fd),
                                    Err(_) => { let _ = reply.send(Err(Error::Unavailable)); continue; }
                                };
                                let io = match NativeIo::new(fd, job.held, Direction::Read) { Ok(io) => io, Err(error) => { let _ = reply.send(Err(error)); continue; } };
                                let policy = worker_policy.clone();
                                pending.push(Box::pin(async move {
                                    let value = async {
                                        let bytes = io.read(policy.limits.representation_bytes(format) as usize, &policy).await?;
                                        match format {
                                            Format::TextUtf8 => Ok(Value::Text(Text::new(bytes, &policy.limits)?)),
                                            Format::ImagePng => Ok(Value::Image(Png::new(bytes, &policy.limits)?)),
                                            Format::Files => Err(Error::Unsupported),
                                        }
                                    }.await;
                                    Completed::Read { generation, reply, value, _io: io }
                                }));
                            },
                            Command::Publish { generation, body: value, reply } => {
                                if reply.is_closed() { continue; }
                                if generation != state.snapshot.generation { let _ = reply.send(Err(Error::Changed)); continue; }
                                let previous = body.replace(value.clone());
                                // SetSelection's reply acknowledges native publication. The
                                // portal supplies no cross-client compare-and-set operation.
                                let result = proxy.set_selection(&session, SetSelectionOptions::default().set_mime_types(value.mime_types())).await;
                                let result = if result.is_ok() {
                                    own_pending = Some(value.clone());
                                    state.published(&value)
                                } else { body = previous; Err(Error::Unavailable) };
                                seal.changes.send_replace(state.clone());
                                let _ = reply.send(result);
                                drop(job.held);
                            },
                        }
                    },
                }
            }
            worker_live.store(false, Ordering::Release);

            requests.close();
            // These futures own only nonblocking descriptors and retained guards;
            // dropping them synchronously closes I/O before releasing admission.
            drop(pending);
            while let Some(job) = requests.recv().await {
                drop(job);
            }
            drop(body);
            drop(own_pending);
            drop(proxy);
            drop(session);
            drop(seal);
        });
        Ok(Self(Arc::new(Core {
            jobs,
            changes: receiver,
            bodies,
            policy,
            live,
            activate: Mutex::new(Some(activation)),
            stop: Mutex::new(Some(stop)),
            finished: joined,
        })))
    }
    pub(super) async fn activate(&self) -> Result<(), DesktopError> {
        self.0
            .activate
            .lock()
            .map_err(|_| desktop(Error::Unavailable))?
            .take()
            .ok_or_else(|| desktop(Error::Unavailable))?
            .send(())
            .map_err(|_| desktop(Error::Unavailable))?;
        let mut changes = self.0.changes.clone();
        loop {
            let state = changes.borrow_and_update().clone();
            if state.closed {
                return Err(desktop(Error::Unavailable));
            }
            if state.active {
                return Ok(());
            }
            changes
                .changed()
                .await
                .map_err(|_| desktop(Error::Unavailable))?;
        }
    }
    pub(super) fn available(&self) -> bool {
        self.0.live.load(Ordering::Acquire)
    }
    fn lease(&self, original: Option<NativeLifetime>) -> Result<Lease, Error> {
        if !self.available() {
            return Err(Error::Unavailable);
        }
        let owner = Owner::new();
        if let Some(original) = original
            && !owner.retain(original)
        {
            return Err(Error::Changed);
        }
        Ok(Lease {
            group: self.clone(),
            owner,
        })
    }
    pub(super) fn native(
        &self,
        original: NativeLifetime,
    ) -> Result<Arc<dyn Provider>, DesktopError> {
        Ok(Arc::new(self.lease(Some(original)).map_err(desktop)?))
    }
    pub(super) fn worker(&self, watching: bool) -> Result<Box<dyn ClipboardBackend>, DesktopError> {
        let provider = self.lease(None).map_err(desktop)?;
        let owner = provider.owner.clone();
        let seen = self.0.changes.borrow().snapshot.generation;
        Ok(Box::new(crate::clipboard::native::Legacy::new(
            Arc::new(provider),
            owner,
            watching,
            Some(seen),
        )))
    }
    fn stop(&self) {
        self.0.live.store(false, Ordering::Release);

        if let Ok(mut stop) = self.0.stop.lock()
            && let Some(stop) = stop.take()
        {
            let _ = stop.send(());
        }
    }
    pub(super) async fn close(&self) {
        self.stop();
        let mut finished = self.0.finished.clone();
        while !*finished.borrow_and_update() {
            if finished.changed().await.is_err() {
                break;
            }
        }
    }
}
impl Drop for Core {
    fn drop(&mut self) {
        if let Ok(stop) = self.stop.get_mut()
            && let Some(stop) = stop.take()
        {
            let _ = stop.send(());
        }
    }
}
struct Lease {
    group: Group,
    owner: Arc<Owner>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.owner.close();
    }
}
impl Lease {
    async fn call<T: Send>(
        &self,
        original: NativeLifetime,
        command: impl FnOnce(oneshot::Sender<Result<T, Error>>) -> Command + Send,
    ) -> Result<T, Error> {
        if !self.group.available() {
            return Err(Error::Unavailable);
        }
        let running = self.owner.begin().ok_or(Error::Changed)?;
        let permit = self
            .group
            .0
            .bodies
            .clone()
            .try_acquire_owned()
            .map_err(unavailable)?;
        let (reply, result) = oneshot::channel();
        self.group
            .0
            .jobs
            .try_send(Job {
                command: command(reply),
                held: Guard {
                    running: Some(running),
                    _body: Arc::new(permit),
                    _original: original,
                },
            })
            .map_err(unavailable)?;
        let value = tokio::time::timeout(
            Duration::from_millis(self.group.0.policy.operation_timeout_ms),
            result,
        )
        .await
        .map_err(unavailable)?
        .map_err(unavailable)??;
        if self.owner.closed() {
            return Err(Error::Changed);
        }
        Ok(value)
    }
}
impl Provider for Lease {
    fn close(&self) -> ClipboardFuture<'_, ()> {
        self.owner.close();
        Box::pin(self.owner.drain())
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            receive: vec![Format::TextUtf8, Format::ImagePng],
            offer: vec![Format::TextUtf8, Format::ImagePng],
            shortcut: ShortcutModifier::Control,
            limits: self.group.0.policy.limits.clone(),
        }
    }
    fn current(&self) -> Operation<'_, Snapshot> {
        Box::pin(async move {
            let _running = self.owner.begin().ok_or(Error::Changed)?;
            let state = self.group.0.changes.borrow().clone();
            if state.closed || !self.group.available() {
                return Err(Error::Unavailable);
            }
            Ok(state.snapshot)
        })
    }
    fn changed(&self, after: u64) -> Operation<'_, Snapshot> {
        Box::pin(async move {
            let _running = self.owner.begin().ok_or(Error::Changed)?;
            let mut changes = self.group.0.changes.clone();
            loop {
                let state = changes.borrow_and_update().clone();
                if state.closed || !self.group.available() {
                    return Err(Error::Unavailable);
                }
                if state.snapshot.generation != after {
                    return Ok(state.snapshot);
                }
                tokio::select! { _ = self.owner.closed_signal() => return Err(Error::Changed), result = changes.changed() => result.map_err(unavailable)?, }
            }
        })
    }
    fn read(
        &self,
        generation: u64,
        format: Format,
        original: NativeLifetime,
    ) -> Operation<'_, Value> {
        Box::pin(self.call(original, move |reply| Command::Read {
            generation,
            format,
            reply,
        }))
    }
    fn publish(
        &self,
        generation: u64,
        value: Publication,
        original: NativeLifetime,
    ) -> Operation<'_, Snapshot> {
        Box::pin(async move {
            let body = match value {
                Publication::Text(value) => Body::Text(value),
                Publication::Image(value) => Body::Image(value),
                Publication::Files(_) => return Err(Error::Unsupported),
            };
            self.group
                .0
                .policy
                .limits
                .validate_value(body.format(), body.bytes())?;
            self.call(original, move |reply| Command::Publish {
                generation,
                body,
                reply,
            })
            .await
        })
    }
}

#[cfg(test)]
mod tests;
