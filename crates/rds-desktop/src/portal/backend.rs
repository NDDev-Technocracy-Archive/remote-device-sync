//! A prepared consented desktop, owned by the local agent for its lifetime.

use super::{capture, input, producer, session, state};
use crate::{DesktopError, DesktopSource, FrameProducer, InputSink};
use rds_core::{Codec, DesktopCaps, DisplayInfo};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{Semaphore, oneshot, watch};

/// GNOME/KDE portal serving. Opening may present a LOCAL permission dialog.
/// Remote session requests only borrow this prepared backend. Closing/revoking
/// it seals all leases; it never silently falls back to Xwayland or reopens.
pub struct WaylandDesktop {
    slots: Vec<Arc<capture::Slot>>,
    input: Arc<input::Group>,
    clipboard: Option<Arc<super::clipboard::Group>>,
    producers: Arc<Semaphore>,
    closed: watch::Receiver<Option<StopReason>>,
    close: Mutex<Option<oneshot::Sender<()>>>,
    finished: watch::Receiver<bool>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    LocalStop,
    Revoked,
    NativeFailure,
}

async fn stopped(mut closed: watch::Receiver<Option<StopReason>>) -> Result<(), DesktopError> {
    loop {
        let reason = *closed.borrow_and_update();
        if let Some(reason) = reason {
            return if reason == StopReason::NativeFailure {
                Err(error())
            } else {
                Ok(())
            };
        }
        if closed.changed().await.is_err() {
            return Err(error());
        }
    }
}

fn error() -> DesktopError {
    DesktopError::Capture("Wayland desktop is not ready or was revoked".into())
}
impl WaylandDesktop {
    /// Store lives in a private 0700 directory; tokens are never logged.
    pub async fn open(path: &Path) -> Result<Self, DesktopError> {
        Self::open_with_cancel(path, std::future::pending()).await
    }
    /// Local preparation cancellation joins portal cleanup before returning.
    /// The caller must keep polling this future until it returns; aborting its
    /// task cannot synchronously join native or D-Bus cleanup.
    pub async fn open_with_cancel(
        path: &Path,
        cancelled: impl Future<Output = ()>,
    ) -> Result<Self, DesktopError> {
        Self::prepare(path, false, cancelled).await
    }
    /// Explicit local permission refresh. Retain identity history and the old
    /// state until new consent succeeds; remote sessions cannot invoke this.
    pub async fn request_permission(
        path: &Path,
        cancelled: impl Future<Output = ()>,
    ) -> Result<Self, DesktopError> {
        Self::prepare(path, true, cancelled).await
    }
    async fn prepare(
        path: &Path,
        fresh_permission: bool,
        cancelled: impl Future<Output = ()>,
    ) -> Result<Self, DesktopError> {
        tokio::pin!(cancelled);
        let path = path.to_owned();
        static FILE_WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let permit = FILE_WORKERS
            .get_or_init(|| Arc::new(Semaphore::new(4)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| error())?;
        let mut opening = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            state::Store::open(&path)
        });
        let mut store = tokio::select! {
            biased;
            _ = &mut cancelled => {
                // A started file worker cannot be aborted. Join it and drop
                // its private lock before returning from local cancellation.
                let _ = opening.await;
                return Err(error());
            },
            result = &mut opening => result.map_err(|_| error())??,
        };
        let token = if fresh_permission {
            None
        } else {
            store.token()
        };
        let granted = session::open(token, &mut cancelled).await?;
        tracing::info!(
            clipboard = granted.clipboard.is_some(),
            "Wayland portal capabilities prepared"
        );
        let clipboard = granted.clipboard.clone();
        let closing_clipboard = clipboard.clone();
        let mut sources = Vec::with_capacity(granted.monitors.len());
        let mut ids = BTreeMap::new();
        for monitor in &granted.monitors {
            let index = store.index(&monitor.stable_id)?;
            sources.push((index, monitor.node));
            ids.insert(index, monitor.mapping_id.clone());
        }
        // Start consumes the old token. Persist its replacement before native
        // setup can fail; the consent owner still closes every failure path.
        let token = granted.restore_token;
        let store = tokio::task::spawn_blocking(move || {
            store.save(token)?;
            Ok::<_, DesktopError>(store)
        })
        .await
        .map_err(|_| error())??;
        let mut capture = capture::Group::start(granted.video, sources)?;
        let slots = capture.slots.clone();
        let mappings = slots
            .iter()
            .map(|slot| {
                Ok((
                    slot.index,
                    (ids.remove(&slot.index).ok_or_else(error)?, slot.clone()),
                ))
            })
            .collect::<Result<_, DesktopError>>()?;
        let input = Arc::new(input::Group::start(granted.input, mappings)?);
        let closing_input = input.clone();
        let mut owner = granted.owner;
        let mut portal_closed = owner.closed.clone();
        let (close, stop) = oneshot::channel();
        let (closed_tx, closed) = watch::channel(None);
        let (finished_tx, finished) = watch::channel(false);
        tokio::spawn(async move {
            let _store = store;
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            tokio::pin!(stop);
            let reason = loop {
                tokio::select! {
                    biased;
                    _ = &mut stop => break StopReason::LocalStop,
                    changed = portal_closed.changed() => {
                        if changed.is_err() { break StopReason::NativeFailure; }
                        match *portal_closed.borrow() {
                            Some(session::Closed::Revoked) => break StopReason::Revoked,
                            Some(session::Closed::Requested) => break StopReason::LocalStop,
                            Some(session::Closed::Disconnected) => break StopReason::NativeFailure,
                            None => {},
                        }
                    },
                    _ = tick.tick() => {
                        if closing_input.stopped() {
                            tracing::warn!("prepared Wayland EIS input worker stopped");
                            break StopReason::NativeFailure;
                        }
                        if capture.slots.iter().any(|slot| slot.extent().is_err()) {
                            tracing::warn!("prepared Wayland PipeWire capture unavailable");
                            break StopReason::NativeFailure;
                        }
                    },
                }
            };
            tracing::info!(?reason, "prepared Wayland desktop stopped");
            closed_tx.send_replace(Some(reason));
            capture.stop();
            closing_input.stop();
            let (video, input) = tokio::join!(capture.close(), closing_input.close());
            if let Some(clipboard) = closing_clipboard {
                clipboard.close().await;
            }
            // Let our native peers drop before closing the portal normally.
            // A bounded join timeout still proceeds to portal revocation.
            owner.close().await;
            if video.is_err() || input.is_err() {
                tracing::warn!("Wayland native close did not complete cleanly");
            }
            // Completion is also the state-lock release boundary. A caller
            // may immediately restore a new session without retrying a lock.
            drop(_store);
            drop(capture);
            drop(closing_input);
            finished_tx.send_replace(true);
        });
        let backend = Self {
            slots,
            input,
            clipboard,
            producers: Arc::new(Semaphore::new(8)),
            closed,
            close: Mutex::new(Some(close)),
            finished,
        };
        let ready = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if backend.closed.borrow().is_some() {
                    return Err(error());
                }
                if backend
                    .input
                    .ready
                    .load(std::sync::atomic::Ordering::Acquire)
                    && backend
                        .slots
                        .iter()
                        .all(|slot| slot.extent().is_ok_and(|extent| extent.is_some()))
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let result = tokio::select! {
            ready = ready => ready.map_err(|_| error()).and_then(|result| result),
            _ = &mut cancelled => Err(error()),
        }
        .and_then(|()| backend.capabilities().map(|_| ()));
        if let Err(error) = result {
            backend.close().await;
            return Err(error);
        }
        Ok(backend)
    }
    /// Notify the local owner of a sealed backend. Native faults are failures;
    /// explicit consent revocation remains a clean stop, never an auto-reopen.
    pub async fn stopped(&self) -> Result<(), DesktopError> {
        stopped(self.closed.clone()).await
    }
    pub async fn close(&self) {
        self.request_close();
        let mut finished = self.finished.clone();
        while !*finished.borrow_and_update() {
            if finished.changed().await.is_err() {
                break;
            }
        }
    }
    fn request_close(&self) {
        if let Ok(mut close) = self.close.lock()
            && let Some(close) = close.take()
        {
            let _ = close.send(());
        }
    }
    fn slot(&self, display: u32) -> Result<Arc<capture::Slot>, DesktopError> {
        if self.closed.borrow().is_some() {
            return Err(error());
        }
        self.slots
            .iter()
            .find(|slot| slot.index == display)
            .cloned()
            .ok_or_else(error)
    }
}
impl Drop for WaylandDesktop {
    fn drop(&mut self) {
        self.request_close();
    }
}
impl DesktopSource for WaylandDesktop {
    fn capabilities(&self) -> Result<DesktopCaps, DesktopError> {
        if self.closed.borrow().is_some() {
            return Err(error());
        }
        let displays = self
            .slots
            .iter()
            .enumerate()
            .map(|(position, slot)| {
                let (width, height) = slot.extent()?.ok_or_else(error)?;
                Ok(DisplayInfo {
                    index: slot.index,
                    width,
                    height,
                    primary: position == 0,
                })
            })
            .collect::<Result<_, DesktopError>>()?;
        Ok(DesktopCaps {
            displays,
            codecs: vec![Codec::H264],
        })
    }
    fn producer(
        &self,
        display: u32,
        interval: Duration,
        height: Option<u32>,
        extent: Option<(u32, u32)>,
    ) -> Result<Box<dyn FrameProducer>, DesktopError> {
        let permit = self.producers.clone().try_acquire_owned().map_err(|_| {
            DesktopError::Capture("Wayland encoder session budget exhausted".into())
        })?;
        Ok(Box::new(producer::Producer::new(
            permit,
            self.slot(display)?,
            interval,
            height,
            extent,
        )?))
    }
    fn input(&self, display: u32, extent: (u32, u32)) -> Result<Box<dyn InputSink>, DesktopError> {
        if self.slot(display)?.extent()? != Some(extent) {
            return Err(error());
        }
        self.input.sink(display, extent)
    }
    fn clipboard_supported(&self) -> bool {
        self.closed.borrow().is_none()
            && self
                .clipboard
                .as_ref()
                .is_some_and(|group| group.available())
    }
    fn clipboard(
        &self,
        display: u32,
        watching: bool,
    ) -> Result<Box<dyn crate::clipboard::ClipboardBackend>, DesktopError> {
        self.slot(display)?;
        self.clipboard.as_ref().ok_or_else(error)?.worker(watching)
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[tokio::test]
    async fn sealed_backend_wakes_owner_and_keeps_revocation_distinct_from_native_failure() {
        for (reason, failure) in [
            (StopReason::NativeFailure, true),
            (StopReason::Revoked, false),
            (StopReason::LocalStop, false),
        ] {
            let (close, receiver) = watch::channel(None);
            let wait = stopped(receiver);
            tokio::pin!(wait);
            assert!(
                tokio::time::timeout(Duration::from_millis(5), &mut wait)
                    .await
                    .is_err()
            );
            close.send_replace(Some(reason));
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), wait)
                    .await
                    .unwrap()
                    .is_err(),
                failure
            );
        }
        let (sender, receiver) = watch::channel(None);
        drop(sender);
        assert!(stopped(receiver).await.is_err());
    }

    use std::os::unix::fs::PermissionsExt;
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[tokio::test]
    async fn cancelled_local_preparation_joins_file_worker_before_lock_reopen() {
        let directory = Directory(
            std::env::temp_dir().join(format!("rds-portal-cancel-{:032x}", rand::random::<u128>())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.0.join("permission.json");
        let result = WaylandDesktop::open_with_cancel(&path, std::future::ready(())).await;
        assert!(result.is_err());
        // No ambient portal is contacted. File work has actually returned,
        // rather than leaving a detached lock owner requiring polling/retry.
        let state = state::Store::open(&path).unwrap();
        assert!(state.token().is_none());
    }
    #[tokio::test]
    async fn cancelled_fresh_permission_preserves_existing_token_and_display_history() {
        let directory = Directory(std::env::temp_dir().join(format!(
            "rds-portal-fresh-cancel-{:032x}",
            rand::random::<u128>()
        )));
        std::fs::create_dir(&directory.0).unwrap();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.0.join("permission.json");
        let mut store = state::Store::open(&path).unwrap();
        assert_eq!(store.index("fixture-monitor").unwrap(), 0);
        store.save("retained-fixture-token".into()).unwrap();
        drop(store);
        assert!(
            WaylandDesktop::request_permission(&path, std::future::ready(()))
                .await
                .is_err()
        );
        let mut store = state::Store::open(&path).unwrap();
        assert_eq!(store.token(), Some("retained-fixture-token"));
        assert_eq!(store.index("fixture-monitor").unwrap(), 0);
    }
}
