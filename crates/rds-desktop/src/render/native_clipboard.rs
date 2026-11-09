//! Nonblocking viewer access to the shared native clipboard owner. The event
//! loop only observes metadata and polls completed operations; native work and
//! its original ownership stay on the existing asynchronous provider.
use std::{sync::Arc, time::Duration};

use rds_core::clipboard::Format;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
use winit::{
    event_loop::{EventLoopProxy, OwnedDisplayHandle},
    raw_window_handle::HasDisplayHandle,
};

use crate::clipboard::{
    NativeLifetime,
    native::{Error, Policy, Provider, Publication, Snapshot, Value},
};

#[cfg(any(target_os = "macos", all(target_os = "linux", feature = "x11")))]
use winit::raw_window_handle::RawDisplayHandle;

type Wake = Arc<dyn Fn() + Send + Sync>;

enum Backend {
    #[cfg(target_os = "macos")]
    AppKit,
    #[cfg(all(target_os = "linux", feature = "x11"))]
    X11,
    Unavailable,
}

#[derive(Clone)]
struct Ready {
    provider: Arc<dyn Provider>,
    snapshot: Snapshot,
}

pub(super) struct Clipboard {
    state: watch::Receiver<Option<Ready>>,
    updates: watch::Sender<Option<Ready>>,
    stop: CancellationToken,
    runtime: tokio::runtime::Handle,
    wake: Wake,
    original: NativeLifetime,
    jobs: Arc<tokio::sync::Semaphore>,
    deadline: Duration,
}

/// Dropping a UI waiter cancels its asynchronous wait. The provider still owns
/// any native job already running, including its original context and credits.
pub(super) struct Pending<T> {
    reply: oneshot::Receiver<Result<T, Error>>,
    stop: CancellationToken,
}
impl<T> Pending<T> {
    pub(super) fn poll(&mut self) -> Option<Result<T, Error>> {
        match self.reply.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(Err(Error::Unavailable)),
        }
    }
}
impl<T> Drop for Pending<T> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Clipboard {
    pub(super) fn new(
        display: OwnedDisplayHandle,
        wake: EventLoopProxy<()>,
        original: NativeLifetime,
    ) -> Result<Arc<Self>, Error> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::Unavailable)?;
        let raw = display
            .display_handle()
            .map_err(|_| Error::Unavailable)?
            .as_raw();
        let backend = match raw {
            #[cfg(target_os = "macos")]
            RawDisplayHandle::AppKit(_) => Backend::AppKit,
            #[cfg(all(target_os = "linux", feature = "x11"))]
            RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_) => Backend::X11,
            _ => Backend::Unavailable,
        };
        let policy = Policy::shipped()?;
        let wake: Wake = Arc::new(move || {
            let _ = wake.send_event(());
        });
        Ok(Self::start(
            prepare(backend, policy.clone(), original.clone()),
            policy,
            original,
            wake,
            runtime,
        ))
    }

    fn start(
        prepared: impl Future<Output = Result<Arc<dyn Provider>, Error>> + Send + 'static,
        policy: Policy,
        original: NativeLifetime,
        wake: Wake,
        runtime: tokio::runtime::Handle,
    ) -> Arc<Self> {
        let deadline = Duration::from_millis(policy.operation_timeout_ms);
        let jobs = Arc::new(tokio::sync::Semaphore::new(policy.pending_operations));
        let (state, receiver) = watch::channel(None);
        let stop = CancellationToken::new();
        let owner = original.clone();
        let stopped = stop.clone();
        let notify = wake.clone();
        let updates = state.clone();
        runtime.spawn(async move {
            let provider = tokio::select! {
                biased;
                _ = stopped.cancelled() => return,
                result = prepared => match result {
                    Ok(provider) => provider,
                    Err(error) => {
                        tracing::info!(%error, "native viewer clipboard unavailable");
                        notify();
                        return;
                    }
                },
            };
            let snapshot = tokio::select! {
                biased;
                _ = stopped.cancelled() => None,
                result = provider.current() => result.ok(),
            };
            if let Some(snapshot) = snapshot {
                state.send_replace(Some(Ready {
                    provider: provider.clone(),
                    snapshot,
                }));
                notify();
                stopped.cancelled().await;
            }
            state.send_replace(None);
            notify();
            provider.close().await;
            drop(owner);
        });
        Arc::new(Self {
            state: receiver,
            updates,
            stop,
            runtime,
            wake,
            original,
            jobs,
            deadline,
        })
    }

    pub(super) fn generation(&self) -> Option<i64> {
        self.state
            .borrow()
            .as_ref()
            .and_then(|ready| ready.snapshot.generation.try_into().ok())
    }

    fn provider(&self) -> Result<Arc<dyn Provider>, Error> {
        self.state
            .borrow()
            .as_ref()
            .map(|ready| ready.provider.clone())
            .ok_or(Error::Unavailable)
    }

    pub(super) fn refresh(&self) -> Result<Pending<i64>, Error> {
        let provider = self.provider()?;
        let updates = self.updates.clone();
        self.operation(CancellationToken::new(), async move {
            let snapshot = provider.current().await?;
            let generation = snapshot.generation.try_into().map_err(|_| Error::Changed)?;
            updates.send_replace(Some(Ready { provider, snapshot }));
            Ok(generation)
        })
    }

    pub(super) fn read(&self, purpose: NativeLifetime) -> Result<Pending<Option<String>>, Error> {
        let provider = self.provider()?;
        let original: NativeLifetime = Arc::new((self.original.clone(), purpose));
        let updates = self.updates.clone();
        self.operation(CancellationToken::new(), async move {
            let snapshot = provider.current().await?;
            updates.send_replace(Some(Ready {
                provider: provider.clone(),
                snapshot: snapshot.clone(),
            }));
            if !snapshot
                .representations
                .iter()
                .any(|representation| representation.format == Format::TextUtf8)
            {
                return Ok(None);
            }
            match provider
                .read(snapshot.generation, Format::TextUtf8, original)
                .await?
            {
                Value::Text(text) => Ok(Some(text.as_str().to_owned())),
                _ => Err(Error::Unsupported),
            }
        })
    }

    pub(super) fn publish(
        &self,
        generation: i64,
        text: String,
        scope: CancellationToken,
        purpose: NativeLifetime,
    ) -> Result<Pending<i64>, Error> {
        let provider = self.provider()?;
        let generation = generation.try_into().map_err(|_| Error::Changed)?;
        let limits = provider.capabilities().limits;
        let original: NativeLifetime = Arc::new((self.original.clone(), purpose));
        let updates = self.updates.clone();
        self.operation(scope, async move {
            let text = crate::clipboard::native::Text::new(bytes::Bytes::from(text), &limits)?;
            let snapshot = provider
                .publish(generation, Publication::Text(text), original)
                .await?;
            let generation = snapshot.generation.try_into().map_err(|_| Error::Changed)?;
            updates.send_replace(Some(Ready { provider, snapshot }));
            Ok(generation)
        })
    }

    fn operation<T: Send + 'static>(
        &self,
        scope: CancellationToken,
        operation: impl Future<Output = Result<T, Error>> + Send + 'static,
    ) -> Result<Pending<T>, Error> {
        let permit = self
            .jobs
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Unavailable)?;
        let stop = self.stop.child_token();
        let canceled = stop.clone();
        let wake = self.wake.clone();
        let deadline = self.deadline;
        let (reply, receiver) = oneshot::channel();
        self.runtime.spawn(async move {
            let _permit = permit;
            tokio::select! {
                biased;
                _ = canceled.cancelled() => {},
                _ = scope.cancelled() => {},
                result = tokio::time::timeout(deadline, operation) => { let _ = reply.send(result.unwrap_or(Err(Error::Unavailable))); },
            }
            wake();
        });
        Ok(Pending {
            reply: receiver,
            stop,
        })
    }
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn prepare(
    backend: Backend,
    policy: Policy,
    original: NativeLifetime,
) -> Result<Arc<dyn Provider>, Error> {
    match backend {
        #[cfg(target_os = "macos")]
        Backend::AppKit => {
            let group = crate::clipboard::macos::Group::start(policy)?;
            group.ready().await?;
            group.native(original)
        }
        #[cfg(all(target_os = "linux", feature = "x11"))]
        Backend::X11 => {
            let prepared = crate::clipboard::native::prepare_x11(policy, original.clone()).await?;
            prepared.provider(original)
        }
        Backend::Unavailable => {
            let _ = (policy, original);
            Err(Error::Unavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::{
        ClipboardFuture,
        native::{Operation, Text},
    };
    use rds_core::clipboard::Capabilities;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::{Notify, Semaphore};

    struct Jobs {
        active: AtomicUsize,
        entered: Notify,
        release: Semaphore,
        done: Notify,
        sealed: Notify,
        drained: Notify,
        closed: AtomicBool,
        finished: AtomicBool,
    }
    impl Default for Jobs {
        fn default() -> Self {
            Self {
                active: AtomicUsize::new(0),
                entered: Notify::new(),
                release: Semaphore::new(0),
                done: Notify::new(),
                sealed: Notify::new(),
                drained: Notify::new(),
                closed: AtomicBool::new(false),
                finished: AtomicBool::new(false),
            }
        }
    }
    struct Board {
        jobs: Arc<Jobs>,
        current: AtomicUsize,
        reads: AtomicUsize,
        publications: AtomicUsize,
        watches: AtomicUsize,
    }
    impl Board {
        fn snapshot() -> Snapshot {
            Snapshot {
                generation: 1,
                owned: false,
                representations: vec![rds_core::clipboard::Representation {
                    format: Format::TextUtf8,
                    bytes: Some(4),
                }],
            }
        }
    }
    impl Provider for Board {
        fn close(&self) -> ClipboardFuture<'_, ()> {
            self.jobs.closed.store(true, Ordering::Release);
            self.jobs.sealed.notify_one();
            Box::pin(async {
                while self.jobs.active.load(Ordering::Acquire) != 0 {
                    self.jobs.done.notified().await;
                }
                self.jobs.finished.store(true, Ordering::Release);
                self.jobs.drained.notify_one();
            })
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                offer: vec![Format::TextUtf8],
                receive: vec![Format::TextUtf8],
                shortcut: rds_core::clipboard::ShortcutModifier::Control,
                limits: Policy::shipped().unwrap().limits,
            }
        }
        fn current(&self) -> Operation<'_, Snapshot> {
            self.current.fetch_add(1, Ordering::AcqRel);
            Box::pin(async { Ok(Self::snapshot()) })
        }
        fn read(&self, _: u64, _: Format, original: NativeLifetime) -> Operation<'_, Value> {
            Box::pin(async move {
                self.reads.fetch_add(1, Ordering::AcqRel);
                self.jobs.active.fetch_add(1, Ordering::AcqRel);
                let jobs = self.jobs.clone();
                let (reply, result) = oneshot::channel::<Result<Value, Error>>();
                tokio::spawn(async move {
                    jobs.entered.notify_one();
                    let permit = jobs.release.acquire().await.unwrap();
                    permit.forget();
                    if !reply.is_closed() {
                        let text = Text::new(
                            bytes::Bytes::from_static(b"text"),
                            &Policy::shipped().unwrap().limits,
                        )
                        .unwrap();
                        let _ = reply.send(Ok(Value::Text(text)));
                    }
                    // This represents an actual native job, independently of
                    // its canceled async waiter; the original drops first.
                    drop(original);
                    jobs.active.fetch_sub(1, Ordering::AcqRel);
                    jobs.done.notify_one();
                });
                result.await.map_err(|_| Error::Unavailable)?
            })
        }
        fn publish(&self, _: u64, _: Publication, _: NativeLifetime) -> Operation<'_, Snapshot> {
            self.publications.fetch_add(1, Ordering::AcqRel);
            Box::pin(async { Ok(Self::snapshot()) })
        }
        fn changed(&self, _: u64) -> Operation<'_, Snapshot> {
            self.watches.fetch_add(1, Ordering::AcqRel);
            Box::pin(std::future::pending())
        }
    }
    async fn fixture(original: NativeLifetime) -> (Arc<Clipboard>, Arc<Board>) {
        let board = Arc::new(Board {
            jobs: Arc::new(Jobs::default()),
            current: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            publications: AtomicUsize::new(0),
            watches: AtomicUsize::new(0),
        });
        let prepared: Arc<dyn Provider> = board.clone();
        let mut policy = Policy::shipped().unwrap();
        policy.pending_operations = 1;
        let clipboard = Clipboard::start(
            async move { Ok(prepared) },
            policy,
            original,
            Arc::new(|| {}),
            tokio::runtime::Handle::current(),
        );
        let mut ready = clipboard.state.clone();
        while ready.borrow().is_none() {
            ready.changed().await.unwrap();
        }
        (clipboard, board)
    }
    #[tokio::test]
    async fn ordinary_ui_ticks_only_observe_cached_metadata() {
        let (clipboard, board) = fixture(Arc::new(())).await;
        for _ in 0..1000 {
            assert_eq!(clipboard.generation(), Some(1));
        }
        assert_eq!(board.current.load(Ordering::Acquire), 1);
        assert_eq!(board.reads.load(Ordering::Acquire), 0);
        assert_eq!(board.publications.load(Ordering::Acquire), 0);
        assert_eq!(board.watches.load(Ordering::Acquire), 0);
    }
    #[tokio::test]
    async fn canceled_ui_waiter_keeps_the_real_native_job_and_originals_until_drain() {
        let root = Arc::new(());
        let root_weak = Arc::downgrade(&root);
        let (clipboard, board) = fixture(root).await;
        let purpose = Arc::new(());
        let purpose_weak = Arc::downgrade(&purpose);
        let mut pending = clipboard.read(purpose).unwrap();
        assert!(pending.poll().is_none());
        board.jobs.entered.notified().await;
        assert!(
            clipboard.read(Arc::new(())).is_err(),
            "UI admission must be bounded"
        );
        drop(pending);
        drop(clipboard);
        board.jobs.sealed.notified().await;
        assert!(!board.jobs.finished.load(Ordering::Acquire));
        assert!(root_weak.upgrade().is_some());
        assert!(purpose_weak.upgrade().is_some());
        board.jobs.release.add_permits(1);
        board.jobs.drained.notified().await;
        assert!(purpose_weak.upgrade().is_none());
        assert!(root_weak.upgrade().is_none());
    }
    #[tokio::test]
    async fn superseded_copy_cannot_admit_a_native_publication() {
        let (clipboard, board) = fixture(Arc::new(())).await;
        let scope = CancellationToken::new();
        let mut pending = clipboard
            .publish(1, "stale".into(), scope.clone(), Arc::new(()))
            .unwrap();
        scope.cancel();
        assert!((&mut pending.reply).await.is_err());
        assert_eq!(board.publications.load(Ordering::Acquire), 0);
    }
    #[cfg(all(target_os = "linux", feature = "x11"))]
    #[tokio::test]
    #[ignore = "requires a dedicated Xvfb server; owns its native clipboard"]
    async fn native_clipboard_facade_uses_the_actual_x11_owner() {
        let policy = Policy::shipped().unwrap();
        let original: NativeLifetime = Arc::new(());
        let prepared = crate::clipboard::native::prepare_x11(policy.clone(), original.clone())
            .await
            .unwrap();
        let provider = prepared.provider(original.clone()).unwrap();
        let clipboard = Clipboard::start(
            async move { Ok(provider) },
            policy,
            original,
            Arc::new(|| {}),
            tokio::runtime::Handle::current(),
        );
        let mut ready = clipboard.state.clone();
        while ready.borrow().is_none() {
            ready.changed().await.unwrap();
        }
        drop(ready);
        let text = "controlled native viewer text 🖥️";
        let mut publication = clipboard
            .publish(
                clipboard.generation().unwrap(),
                text.into(),
                CancellationToken::new(),
                Arc::new(()),
            )
            .unwrap();
        let generation = (&mut publication.reply).await.unwrap().unwrap();
        assert_eq!(clipboard.generation(), Some(generation));
        drop(publication);
        let mut read = clipboard.read(Arc::new(())).unwrap();
        assert_eq!(
            (&mut read.reply).await.unwrap().unwrap().as_deref(),
            Some(text)
        );
        drop(read);
        drop(clipboard);
        prepared.close().await;
    }
}
