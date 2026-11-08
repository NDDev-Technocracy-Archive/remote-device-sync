//! One sleeping AppKit owner per process, bounded jobs and body admission.
//! A timed-out caller never detaches its original scope from in-flight native I/O.
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicU8, AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;

use rds_core::clipboard::{Capabilities, Format, ShortcutModifier};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};

use super::Board;
use crate::clipboard::{
    ClipboardBackend, ClipboardFuture, NativeLifetime,
    native::{Error, Operation, Policy, Provider, Publication, Snapshot, Text, Value},
    ownership::{Owner, Running},
};

static POOL: OnceLock<Mutex<Weak<Core>>> = OnceLock::new();
static NATIVE_OWNER: Semaphore = Semaphore::const_new(1);
const STARTING: u8 = 0;
const READY: u8 = 1;
const ENDED: u8 = 2;

struct Core {
    jobs: mpsc::SyncSender<Message>,
    snapshots: watch::Receiver<Option<Snapshot>>,
    state: Arc<AtomicU8>,
    watchers: Arc<AtomicUsize>,
    bodies: Arc<Semaphore>,
    policy: Policy,
}
enum Message {
    Wake,
    Work(Job),
}
struct Job {
    command: Command,
    running: Running,
    _core: Arc<Core>,
    _body: Option<OwnedSemaphorePermit>,
}
enum Command {
    Current(oneshot::Sender<Result<Snapshot, Error>>),
    Read {
        generation: u64,
        format: Format,
        reply: oneshot::Sender<Result<Value, Error>>,
    },
    Publish {
        generation: u64,
        value: Publication,
        reply: oneshot::Sender<Result<Snapshot, Error>>,
    },
}

#[derive(Clone)]
pub(crate) struct Group(Arc<Core>);
impl Group {
    pub(crate) fn start(policy: Policy) -> Result<Self, Error> {
        policy.validate()?;
        let mut pool = POOL
            .get_or_init(|| Mutex::new(Weak::new()))
            .lock()
            .map_err(|_| Error::Unavailable)?;
        if let Some(core) = pool.upgrade() {
            if core.policy != policy {
                return Err(Error::Settings);
            }
            return Ok(Self(core));
        }
        let group = Self::spawn(policy, Board::general)?;
        *pool = Arc::downgrade(&group.0);
        Ok(group)
    }
    fn spawn(policy: Policy, board: fn() -> Result<Board, Error>) -> Result<Self, Error> {
        policy.validate()?;
        let permit = NATIVE_OWNER.try_acquire().map_err(|_| Error::Unavailable)?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::Unavailable)?;
        let (jobs, receiver) = mpsc::sync_channel(policy.pending_operations);
        let (snapshots, changes) = watch::channel(None);
        let state = Arc::new(AtomicU8::new(STARTING));
        let watchers = Arc::new(AtomicUsize::new(0));
        let core = Arc::new(Core {
            jobs,
            snapshots: changes,
            state: state.clone(),
            watchers: watchers.clone(),
            bodies: Arc::new(Semaphore::new(policy.limits.parallel_transfers as usize)),
            policy: policy.clone(),
        });
        runtime.spawn_blocking(move || {
            let _permit = permit;
            struct Exit {
                state: Arc<AtomicU8>,
                snapshots: watch::Sender<Option<Snapshot>>,
            }
            impl Drop for Exit {
                fn drop(&mut self) {
                    self.state.store(ENDED, Ordering::Release);
                    self.snapshots.send_replace(None);
                }
            }
            let _exit = Exit {
                state: state.clone(),
                snapshots: snapshots.clone(),
            };
            let mut board = match objc2::rc::autoreleasepool(|_| board()) {
                Ok(board) => board,
                Err(error) => {
                    tracing::warn!(%error,"native clipboard owner unavailable");
                    return;
                }
            };
            state.store(READY, Ordering::Release);
            snapshots.send_replace(board.snapshot(&policy).ok());
            loop {
                let message = if watchers.load(Ordering::Acquire) == 0 {
                    match receiver.recv() {
                        Ok(message) => Some(message),
                        Err(_) => break,
                    }
                } else {
                    match receiver.recv_timeout(Duration::from_millis(policy.watch_interval_ms)) {
                        Ok(message) => Some(message),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                };
                if let Some(Message::Work(job)) = message {
                    execute(job, &mut board, &policy);
                }
                if watchers.load(Ordering::Acquire) > 0 {
                    let previous = snapshots
                        .borrow()
                        .as_ref()
                        .map(|snapshot| snapshot.generation);
                    if board.generation().ok() != previous {
                        snapshots.send_replace(board.snapshot(&policy).ok());
                    }
                }
            }
        });
        Ok(Self(core))
    }
    pub(crate) async fn ready(&self) -> Result<(), Error> {
        let mut changes = self.0.snapshots.clone();
        tokio::time::timeout(
            Duration::from_millis(self.0.policy.operation_timeout_ms),
            async {
                loop {
                    match self.0.state.load(Ordering::Acquire) {
                        READY => return Ok(()),
                        ENDED => return Err(Error::Unavailable),
                        _ => {}
                    }
                    changes.changed().await.map_err(|_| Error::Unavailable)?;
                }
            },
        )
        .await
        .map_err(|_| Error::Unavailable)?
    }
    pub(crate) fn available(&self) -> bool {
        self.0.state.load(Ordering::Acquire) == READY
    }
    fn provider(&self) -> Lease {
        Lease {
            group: self.clone(),
            owner: Owner::new(),
        }
    }
    pub(crate) fn worker(
        &self,
        watching: bool,
    ) -> Result<Box<dyn ClipboardBackend>, crate::DesktopError> {
        if !self.available() {
            return Err(desktop_error(Error::Unavailable));
        }
        let seen = self
            .0
            .snapshots
            .borrow()
            .as_ref()
            .map(|snapshot| snapshot.generation);
        Ok(Box::new(Legacy {
            provider: self.provider(),
            seen,
            watching,
        }))
    }
}

/// The qualification binary uses the same queued owner with a private native
/// pasteboard. It does not read or replace the user's general selection.
#[cfg(feature = "viewer")]
pub(crate) async fn probe() -> Result<u32, Error> {
    let policy = Policy::default();
    let group = Group::spawn(policy.clone(), || Ok(Board::isolated()))?;
    group.ready().await?;
    let provider = group.provider();
    let context = Arc::new(());
    let retained = Arc::downgrade(&context);
    if !provider.owner.retain(context) {
        return Err(Error::Unavailable);
    }
    let before = provider.current().await?;
    let text = Text::new(
        bytes::Bytes::from_static("Привет 🖥️\n".as_bytes()),
        &policy.limits,
    )?;
    let expected = text.encoded().clone();
    let published = provider
        .publish(before.generation, Publication::Text(text))
        .await?;
    let Value::Text(read) = provider
        .read(published.generation, Format::TextUtf8)
        .await?
    else {
        return Err(Error::Unsupported);
    };
    if read.encoded() != &expected {
        return Err(Error::Publication);
    }
    let changed = provider.changed(before.generation).await?;
    if changed.generation != published.generation || !changed.owned {
        return Err(Error::Publication);
    }
    if !matches!(
        provider
            .publish(before.generation, Publication::Text(read))
            .await,
        Err(Error::Changed)
    ) || provider.current().await?.generation != published.generation
    {
        return Err(Error::Publication);
    }
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|_| Error::Unavailable)?;
        writer
            .write_image_data(&[255, 40, 20, 128])
            .map_err(|_| Error::Unavailable)?;
        writer.finish().map_err(|_| Error::Unavailable)?;
    }
    let image = super::super::image::Png::new(bytes::Bytes::from(encoded), &policy.limits)?;
    let expected = image.encoded().clone();
    let published = provider
        .publish(published.generation, Publication::Image(image))
        .await?;
    let Value::Image(read) = provider
        .read(published.generation, Format::ImagePng)
        .await?
    else {
        return Err(Error::Unsupported);
    };
    if read.encoded() != &expected {
        return Err(Error::Publication);
    }
    provider.owner.drain().await;
    if !matches!(provider.current().await, Err(Error::Changed)) || retained.upgrade().is_none() {
        return Err(Error::Publication);
    }
    drop(provider);
    if retained.upgrade().is_some() {
        return Err(Error::Publication);
    }
    Ok(5)
}

fn execute(job: Job, board: &mut Board, policy: &Policy) {
    if job.running.closed() {
        return;
    }
    match job.command {
        Command::Current(reply) if !reply.is_closed() => {
            let _ = reply.send(board.snapshot(policy));
        }
        Command::Read {
            generation,
            format,
            reply,
        } if !reply.is_closed() => {
            let _ = reply.send(board.read(generation, format, policy));
        }
        Command::Publish {
            generation,
            value,
            reply,
        } if !reply.is_closed() => {
            let _ = reply.send(board.publish(generation, value, policy));
        }
        _ => {}
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
        body: bool,
        command: impl FnOnce(oneshot::Sender<Result<T, Error>>) -> Command + Send,
    ) -> Result<T, Error> {
        if !self.group.available() {
            return Err(Error::Unavailable);
        }
        let running = self.owner.begin().ok_or(Error::Changed)?;
        let body = if body {
            Some(
                self.group
                    .0
                    .bodies
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| Error::Unavailable)?,
            )
        } else {
            None
        };
        let (reply, response) = oneshot::channel();
        self.group
            .0
            .jobs
            .try_send(Message::Work(Job {
                command: command(reply),
                running,
                _core: self.group.0.clone(),
                _body: body,
            }))
            .map_err(|_| Error::Unavailable)?;
        let value = tokio::time::timeout(
            Duration::from_millis(self.group.0.policy.operation_timeout_ms),
            response,
        )
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(|_| Error::Unavailable)??;
        if self.owner.closed() {
            return Err(Error::Changed);
        }
        Ok(value)
    }
}
impl Provider for Lease {
    fn capabilities(&self) -> Capabilities {
        let formats = if self.group.available() {
            vec![Format::TextUtf8, Format::ImagePng]
        } else {
            Vec::new()
        };
        Capabilities {
            receive: formats.clone(),
            offer: formats,
            shortcut: ShortcutModifier::Command,
            limits: self.group.0.policy.limits.clone(),
        }
    }
    fn current(&self) -> Operation<'_, Snapshot> {
        Box::pin(self.call(false, Command::Current))
    }
    fn read(&self, generation: u64, format: Format) -> Operation<'_, Value> {
        Box::pin(self.call(true, move |reply| Command::Read {
            generation,
            format,
            reply,
        }))
    }
    fn publish(&self, generation: u64, value: Publication) -> Operation<'_, Snapshot> {
        Box::pin(self.call(true, move |reply| Command::Publish {
            generation,
            value,
            reply,
        }))
    }
    fn changed(&self, generation: u64) -> Operation<'_, Snapshot> {
        Box::pin(async move {
            struct Watching(Arc<AtomicUsize>);
            impl Drop for Watching {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _watching = Watching(self.group.0.watchers.clone());
            self.group.0.watchers.fetch_add(1, Ordering::AcqRel);
            let _ = self.group.0.jobs.try_send(Message::Wake);
            let mut changes = self.group.0.snapshots.clone();
            loop {
                if self.owner.closed() {
                    return Err(Error::Changed);
                }
                if !self.group.available() {
                    return Err(Error::Unavailable);
                }
                if let Some(snapshot) = changes.borrow_and_update().clone()
                    && snapshot.generation != generation
                {
                    return Ok(snapshot);
                }
                tokio::select! { _ = self.owner.closed_signal() => return Err(Error::Changed),
                result = changes.changed() => result.map_err(|_| Error::Unavailable)?, }
            }
        })
    }
}

struct Legacy {
    provider: Lease,
    seen: Option<u64>,
    watching: bool,
}
fn desktop_error(error: Error) -> crate::DesktopError {
    crate::DesktopError::Input(error.to_string())
}
impl ClipboardBackend for Legacy {
    fn retain(&mut self, context: NativeLifetime) -> bool {
        self.provider.owner.retain(context)
    }
    fn close(&mut self) -> ClipboardFuture<'_, ()> {
        Box::pin(self.provider.owner.drain())
    }
    fn publish(&mut self, text: String) -> ClipboardFuture<'_, Result<(), crate::DesktopError>> {
        Box::pin(async move {
            let current = self.provider.current().await.map_err(desktop_error)?;
            let text = Text::new(
                bytes::Bytes::from(text),
                &self.provider.group.0.policy.limits,
            )
            .map_err(desktop_error)?;
            self.provider
                .publish(current.generation, Publication::Text(text))
                .await
                .map_err(desktop_error)?;
            Ok(())
        })
    }
    fn next_change(&mut self) -> ClipboardFuture<'_, Option<String>> {
        Box::pin(async move {
            if !self.watching {
                self.provider.owner.closed_signal().await;
                return None;
            }
            if self.seen.is_none() {
                self.seen = Some(self.provider.current().await.ok()?.generation);
            }
            loop {
                let snapshot = self.provider.changed(self.seen?).await.ok()?;
                self.seen = Some(snapshot.generation);
                if snapshot.owned
                    || !snapshot
                        .representations
                        .iter()
                        .any(|r| r.format == Format::TextUtf8)
                {
                    continue;
                }
                match self
                    .provider
                    .read(snapshot.generation, Format::TextUtf8)
                    .await
                {
                    Ok(Value::Text(text)) => return Some(text.as_str().to_owned()),
                    Err(Error::Unavailable) => return None,
                    _ => {}
                }
            }
        })
    }
}
