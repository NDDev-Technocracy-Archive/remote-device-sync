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
    ClipboardFuture, NativeLifetime,
    native::{Error, Operation, Policy, Provider, Publication, Snapshot, Value},
    ownership::{Owner, Running},
};

#[cfg(all(feature = "viewer", feature = "host"))]
use crate::clipboard::native::Text;

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
    _operation_original: Option<NativeLifetime>,
    _body: Option<OwnedSemaphorePermit>,
    _core: Arc<Core>,
    // Acknowledge drain only after the original purpose and body credit drop.
    running: Running,
}
enum Command {
    Current(oneshot::Sender<Result<Snapshot, Error>>),
    Read {
        generation: u64,
        format: Format,
        authority: Option<NativeLifetime>,
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
    fn spawn(
        policy: Policy,
        board: impl FnOnce(Arc<rds_sync::selection::DescriptorBudget>) -> Result<Board, Error>
        + Send
        + 'static,
    ) -> Result<Self, Error> {
        policy.validate()?;
        let files = policy.selection_budget()?;
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
            bodies: Arc::new(Semaphore::new(policy.native_body_operations)),
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
            let mut board = match objc2::rc::autoreleasepool(|_| board(files)) {
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
    pub(crate) fn native(&self, original: NativeLifetime) -> Result<Arc<dyn Provider>, Error> {
        if !self.available() {
            return Err(Error::Unavailable);
        }
        let provider = self.provider();
        if !provider.owner.retain(original) {
            return Err(Error::Changed);
        }
        Ok(Arc::new(provider))
    }
    #[cfg(feature = "host")]
    pub(crate) fn worker(
        &self,
        watching: bool,
    ) -> Result<Box<dyn crate::clipboard::ClipboardBackend>, crate::DesktopError> {
        if !self.available() {
            return Err(desktop_error(Error::Unavailable));
        }
        let seen = self
            .0
            .snapshots
            .borrow()
            .as_ref()
            .map(|snapshot| snapshot.generation);
        let provider = self.provider();
        let owner = provider.owner.clone();
        Ok(Box::new(crate::clipboard::native::Legacy::new(
            Arc::new(provider),
            owner,
            watching,
            seen,
        )))
    }
}

/// The qualification binary uses the same queued owner with a private native
/// pasteboard. It does not read or replace the user's general selection.
#[cfg(all(feature = "viewer", feature = "host"))]
struct Paused {
    release: mpsc::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), Error>>,
}
#[cfg(all(feature = "viewer", feature = "host"))]
impl Paused {
    async fn new() -> Result<Self, Error> {
        let (entered, blocked) = oneshot::channel();
        let (release, gate) = mpsc::channel();
        let task = tokio::task::spawn_blocking(move || {
            let _access = super::ACCESS.lock().map_err(|_| Error::Unavailable)?;
            let _ = entered.send(());
            gate.recv_timeout(Duration::from_secs(10))
                .map_err(|_| Error::Unavailable)
        });
        blocked.await.map_err(|_| Error::Unavailable)?;
        Ok(Self { release, task })
    }
    async fn finish(self) -> Result<(), Error> {
        self.release.send(()).map_err(|_| Error::Unavailable)?;
        self.task.await.map_err(|_| Error::Unavailable)?
    }
}
#[cfg(all(feature = "viewer", feature = "host"))]
struct GatedRead {
    provider: Arc<dyn Provider>,
    group: Group,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    reads: AtomicUsize,
}
#[cfg(all(feature = "viewer", feature = "host"))]
impl Provider for GatedRead {
    fn close(&self) -> ClipboardFuture<'_, ()> {
        self.provider.close()
    }
    fn capabilities(&self) -> Capabilities {
        self.provider.capabilities()
    }
    fn current(&self) -> Operation<'_, Snapshot> {
        self.provider.current()
    }
    fn changed(&self, after: u64) -> Operation<'_, Snapshot> {
        self.provider.changed(after)
    }
    fn publish(
        &self,
        generation: u64,
        value: Publication,
        original: NativeLifetime,
    ) -> Operation<'_, Snapshot> {
        self.provider.publish(generation, value, original)
    }
    fn read(
        &self,
        generation: u64,
        format: Format,
        original: NativeLifetime,
    ) -> Operation<'_, Value> {
        Box::pin(async move {
            if self.reads.fetch_add(1, Ordering::AcqRel) != 0 {
                return Err(Error::Publication);
            }
            // Metadata may need its own asynchronous refresh. Gate only the
            // actual read, after that phase has completed, on the real edge.
            let pause = Paused::new().await?;
            let mut reading = self.provider.read(generation, format, original);
            if !matches!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(reading.as_mut().poll(cx))).await,
                std::task::Poll::Pending
            ) || self.group.0.bodies.available_permits()
                != self.group.0.policy.native_body_operations - 1
            {
                return Err(Error::Publication);
            }
            self.entered.notify_one();
            self.release.notified().await;
            pause.finish().await?;
            reading.await
        })
    }
}
#[cfg(all(feature = "viewer", feature = "host"))]
pub(crate) async fn probe() -> Result<u32, Error> {
    let file_cases = super::files::probe().await?;
    let policy = Policy::shipped()?;
    let (named, name) = oneshot::channel();
    let group = Group::spawn(policy.clone(), move |files| {
        let board = Board::isolated(files);
        let _ = named.send(board.board.name().to_string());
        Ok(board)
    })?;
    group.ready().await?;
    let name = name.await.map_err(|_| Error::Unavailable)?;
    let context = Arc::new(());
    let retained = Arc::downgrade(&context);
    let provider = group.native(context)?;
    let before = provider.current().await?;
    let text = Text::new(
        bytes::Bytes::from_static("Привет 🖥️\n".as_bytes()),
        &policy.limits,
    )?;
    let expected = text.encoded().clone();
    let published = provider
        .publish(before.generation, Publication::Text(text), Arc::new(()))
        .await?;
    let Value::Text(read) = provider
        .read(published.generation, Format::TextUtf8, Arc::new(()))
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
            .publish(before.generation, Publication::Text(read), Arc::new(()))
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
        .publish(
            published.generation,
            Publication::Image(image),
            Arc::new(()),
        )
        .await?;
    let Value::Image(read) = provider
        .read(published.generation, Format::ImagePng, Arc::new(()))
        .await?
    else {
        return Err(Error::Unsupported);
    };
    if read.encoded() != &expected {
        return Err(Error::Publication);
    }
    let mut legacy = group.worker(true).map_err(|_| Error::Unavailable)?;
    if !legacy.retain(Arc::new(())) {
        return Err(Error::Publication);
    }
    legacy
        .publish("legacy native publication".into())
        .await
        .map_err(|_| Error::Unavailable)?;
    let own = provider.current().await?;
    let Value::Text(text) = provider
        .read(own.generation, Format::TextUtf8, Arc::new(()))
        .await?
    else {
        return Err(Error::Unsupported);
    };
    if text.as_str() != "legacy native publication" {
        return Err(Error::Publication);
    }
    let pause = Paused::new().await?;
    let mut unchanged = provider.changed(own.generation);
    if !matches!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(unchanged.as_mut().poll(cx))).await,
        std::task::Poll::Pending
    ) {
        return Err(Error::Publication);
    }
    drop(unchanged);
    pause.finish().await?;
    legacy.close().await;
    drop(legacy);
    let inner = group.provider();
    let owner = inner.owner.clone();
    let gated = Arc::new(GatedRead {
        provider: Arc::new(inner),
        group: group.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        reads: AtomicUsize::new(0),
    });
    let mut legacy =
        crate::clipboard::native::Legacy::new(gated.clone(), owner, true, Some(own.generation));
    if !legacy.retain(Arc::new(())) {
        return Err(Error::Publication);
    }
    let mut observing = legacy.next_change();
    if !matches!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(observing.as_mut().poll(cx))).await,
        std::task::Poll::Pending
    ) {
        return Err(Error::Publication);
    }
    let mut external = Board {
        board: objc2_app_kit::NSPasteboard::pasteboardWithName(
            &objc2_foundation::NSString::from_str(&name),
        ),
        files: policy.selection_budget()?,
        published: None,
        isolated: false,
    };
    let expected = "copied while control input cancels its wait ✓";
    let before = external.generation()?;
    external.publish(
        before,
        Publication::Text(Text::new(
            bytes::Bytes::from_static(expected.as_bytes()),
            &policy.limits,
        )?),
        &policy,
    )?;
    tokio::select! {
        _ = gated.entered.notified() => {},
        _ = &mut observing => return Err(Error::Publication),
        _ = tokio::time::sleep(Duration::from_secs(10)) => return Err(Error::Unavailable),
    }
    drop(observing);
    gated.release.notify_one();
    let value = tokio::time::timeout(Duration::from_secs(10), legacy.next_change())
        .await
        .map_err(|_| Error::Unavailable)?
        .ok_or(Error::Unavailable)?;
    if value != expected || gated.reads.load(Ordering::Acquire) != 1 {
        return Err(Error::Publication);
    }
    legacy.close().await;
    drop(legacy);
    drop(gated);
    drop(external);
    cleanup_order(&group).await?;
    provider.close().await;
    if !matches!(provider.current().await, Err(Error::Changed)) || retained.upgrade().is_none() {
        return Err(Error::Publication);
    }
    drop(provider);
    if retained.upgrade().is_some() {
        return Err(Error::Publication);
    }
    Ok(9 + file_cases)
}

#[cfg(all(feature = "viewer", feature = "host"))]
async fn cleanup_order(group: &Group) -> Result<(), Error> {
    let provider = group.native(Arc::new(()))?;
    let current = provider.current().await?;
    let (original, mut cleanup) = crate::clipboard::ownership::probe::Cleanup::held();
    let Value::Text(_) = provider
        .read(current.generation, Format::TextUtf8, original)
        .await?
    else {
        return Err(Error::Publication);
    };
    cleanup.entered().await?;
    let mut closing = provider.close();
    let pending =
        std::future::poll_fn(|cx| std::task::Poll::Ready(closing.as_mut().poll(cx).is_pending()))
            .await;
    let held = group.0.bodies.available_permits() == group.0.policy.native_body_operations - 1;
    cleanup.release().await?;
    if !pending || !held {
        return Err(Error::Io(std::io::Error::other(
            "native clipboard acknowledged drain or released body credit before original-purpose cleanup",
        )));
    }
    closing.await;
    if group.0.bodies.available_permits() != group.0.policy.native_body_operations {
        return Err(Error::Publication);
    }
    Ok(())
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
            authority,
            reply,
        } if !reply.is_closed() => {
            let _ = reply.send(board.read(generation, format, policy, authority));
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
        original: Option<NativeLifetime>,
        command: impl FnOnce(oneshot::Sender<Result<T, Error>>) -> Command + Send,
    ) -> Result<T, Error> {
        if !self.group.available() {
            return Err(Error::Unavailable);
        }
        let running = self.owner.begin().ok_or(Error::Changed)?;
        let body = if original.is_some() {
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
                _operation_original: original,
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
    fn close(&self) -> ClipboardFuture<'_, ()> {
        self.owner.close();
        Box::pin(self.owner.drain())
    }
    fn capabilities(&self) -> Capabilities {
        let formats = if self.group.available() {
            vec![Format::TextUtf8, Format::ImagePng]
        } else {
            Vec::new()
        };
        Capabilities {
            receive: formats.clone(),
            offer: if self.group.available() {
                vec![Format::TextUtf8, Format::ImagePng, Format::Files]
            } else {
                Vec::new()
            },
            shortcut: ShortcutModifier::Command,
            limits: self.group.0.policy.limits.clone(),
        }
    }
    fn current(&self) -> Operation<'_, Snapshot> {
        Box::pin(self.call(None, Command::Current))
    }
    fn read(
        &self,
        generation: u64,
        format: Format,
        original: NativeLifetime,
    ) -> Operation<'_, Value> {
        Box::pin(async move {
            let authority: Option<NativeLifetime> = if format == Format::Files {
                Some(Arc::new(self.owner.begin().ok_or(Error::Changed)?))
            } else {
                None
            };
            self.call(Some(original), move |reply| Command::Read {
                generation,
                format,
                authority,
                reply,
            })
            .await
        })
    }
    fn publish(
        &self,
        generation: u64,
        value: Publication,
        original: NativeLifetime,
    ) -> Operation<'_, Snapshot> {
        Box::pin(self.call(Some(original), move |reply| Command::Publish {
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
                let snapshot = changes.borrow_and_update().clone();
                if snapshot.is_some_and(|snapshot| snapshot.generation != generation) {
                    // The sleeping metadata cache may predate current()/publish.
                    // Refresh on its native owner before disclosing a change.
                    let current = self.current().await?;
                    if current.generation != generation {
                        return Ok(current);
                    }
                }
                tokio::select! { _ = self.owner.closed_signal() => return Err(Error::Changed),
                result = changes.changed() => result.map_err(|_| Error::Unavailable)?, }
            }
        })
    }
}

#[cfg(feature = "host")]
fn desktop_error(error: Error) -> crate::DesktopError {
    crate::DesktopError::Input(error.to_string())
}
