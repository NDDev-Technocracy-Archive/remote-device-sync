//! Local screen consent has its own lifetime, independent of outgoing sessions.

#[cfg(all(target_os = "linux", feature = "portal"))]
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rds_desktop::{DesktopError, DesktopSource, FrameProducer, InputSink};
use tokio::sync::watch;

#[derive(Default)]
struct Slot(RwLock<Option<Arc<dyn DesktopSource>>>);

fn unavailable() -> DesktopError {
    DesktopError::Capture("local desktop permission is not ready or was withdrawn".into())
}

impl Slot {
    fn get(&self) -> Result<Arc<dyn DesktopSource>, DesktopError> {
        self.0
            .read()
            .map_err(|_| unavailable())?
            .clone()
            .ok_or_else(unavailable)
    }

    fn set(&self, source: Option<Arc<dyn DesktopSource>>) {
        if let Ok(mut current) = self.0.write() {
            *current = source;
        }
    }
}

impl DesktopSource for Slot {
    fn capabilities(&self) -> Result<rds_core::DesktopCaps, DesktopError> {
        self.get()?.capabilities()
    }

    fn producer(
        &self,
        display: u32,
        interval: Duration,
        height: Option<u32>,
        extent: Option<(u32, u32)>,
    ) -> Result<Box<dyn FrameProducer>, DesktopError> {
        self.get()?.producer(display, interval, height, extent)
    }

    fn input(&self, display: u32, extent: (u32, u32)) -> Result<Box<dyn InputSink>, DesktopError> {
        self.get()?.input(display, extent)
    }

    fn clipboard_supported(&self) -> bool {
        self.get().is_ok_and(|source| source.clipboard_supported())
    }

    fn clipboard(
        &self,
        display: u32,
        watching: bool,
    ) -> Result<Box<dyn rds_desktop::clipboard::ClipboardBackend>, DesktopError> {
        self.get()?.clipboard(display, watching)
    }
}

async fn cancelled(mut stop: watch::Receiver<bool>) {
    while !*stop.borrow_and_update() {
        if stop.changed().await.is_err() {
            break;
        }
    }
}

pub(super) struct Host {
    source: Arc<Slot>,
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<Result<(), DesktopError>>>,
}

trait Owner: DesktopSource {
    fn stopped(&self) -> impl Future<Output = Result<(), DesktopError>> + Send;
    fn close(&self) -> impl Future<Output = Result<(), DesktopError>> + Send;
}

#[cfg(all(target_os = "linux", feature = "portal"))]
impl Owner for rds_desktop::WaylandDesktop {
    fn stopped(&self) -> impl Future<Output = Result<(), DesktopError>> + Send {
        Self::stopped(self)
    }
    async fn close(&self) -> Result<(), DesktopError> {
        Self::close(self).await;
        Ok(())
    }
}

#[cfg(all(target_os = "macos", feature = "desktop"))]
impl Owner for rds_desktop::MacDesktop {
    fn stopped(&self) -> impl Future<Output = Result<(), DesktopError>> + Send {
        Self::stopped(self)
    }
    fn close(&self) -> impl Future<Output = Result<(), DesktopError>> + Send {
        Self::close(self)
    }
}

impl Host {
    #[cfg(all(target_os = "linux", feature = "portal"))]
    pub fn wayland(path: PathBuf, refresh: bool) -> Self {
        Self::start("Wayland", move |stop, first| {
            let path = path.clone();
            async move {
                if refresh && first {
                    rds_desktop::WaylandDesktop::request_permission(&path, cancelled(stop)).await
                } else {
                    rds_desktop::WaylandDesktop::open_with_cancel(&path, cancelled(stop)).await
                }
            }
        })
    }

    #[cfg(all(target_os = "macos", feature = "desktop"))]
    pub fn macos() -> Self {
        Self::start("macOS", |stop, _first| {
            rds_desktop::MacDesktop::open_with_cancel(cancelled(stop))
        })
    }

    fn start<B, F>(
        backend: &'static str,
        mut prepare: impl FnMut(watch::Receiver<bool>, bool) -> F + Send + 'static,
    ) -> Self
    where
        B: Owner,
        F: Future<Output = Result<B, DesktopError>> + Send + 'static,
    {
        let source = Arc::new(Slot::default());
        let current = source.clone();
        let (stop, stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            const RECOVERY_DELAY: Duration = Duration::from_secs(1);
            let mut first = true;
            let mut closed = Ok(());
            while !*stopping.borrow() {
                let owner = match prepare(stopping.clone(), first).await {
                    Ok(owner) => Arc::new(owner),
                    Err(error) => {
                        // Failed/cancelled setup may reflect a denied local
                        // grant. Never loop permission requests after that.
                        tracing::warn!(backend, %error, "screen hosting unavailable; local sessions remain available");
                        break;
                    }
                };
                first = false;
                current.set(Some(owner.clone()));
                tracing::info!(backend, "screen hosting ready");
                let restore = tokio::select! {
                    biased;
                    _ = cancelled(stopping.clone()) => false,
                    result = owner.stopped() => {
                        tracing::warn!(backend, ?result, "screen hosting stopped; local sessions remain available");
                        result.is_err()
                    },
                };
                current.set(None);
                closed = owner.close().await;
                if let Err(error) = &closed {
                    tracing::warn!(backend, %error, "native screen cleanup failed");
                    break;
                }
                if !restore {
                    break;
                }
                // A native fault restores only after the old owner drains.
                // Explicit revocation is a clean stop; macOS preparation also
                // preflights grants before any native capture call. A refresh
                // request is never repeated by this recovery loop.
                tokio::select! {
                    biased;
                    _ = cancelled(stopping.clone()) => break,
                    _ = tokio::time::sleep(RECOVERY_DELAY) => {},
                }
            }
            current.set(None);
            cancelled(stopping).await;
            closed
        });
        Self {
            source,
            stop,
            task: Some(task),
        }
    }

    pub fn source(&self) -> Arc<dyn DesktopSource> {
        self.source.clone()
    }

    pub fn request_stop(&self) {
        self.source.set(None);
        self.stop.send_replace(true);
    }

    pub async fn close(mut self) -> anyhow::Result<()> {
        self.request_stop();
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Ready {
        fault: bool,
        closes: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl DesktopSource for Ready {
        fn capabilities(&self) -> Result<rds_core::DesktopCaps, DesktopError> {
            Ok(rds_core::DesktopCaps {
                displays: vec![rds_core::DisplayInfo {
                    index: 7,
                    width: 640,
                    height: 480,
                    primary: true,
                }],
                codecs: vec![rds_core::Codec::H264],
            })
        }
        fn producer(
            &self,
            _: u32,
            _: Duration,
            _: Option<u32>,
            _: Option<(u32, u32)>,
        ) -> Result<Box<dyn FrameProducer>, DesktopError> {
            Ok(Box::new(rds_desktop::NullProducer))
        }
        fn input(&self, _: u32, _: (u32, u32)) -> Result<Box<dyn InputSink>, DesktopError> {
            Err(unavailable())
        }
    }

    impl Owner for Ready {
        async fn stopped(&self) -> Result<(), DesktopError> {
            if self.fault {
                Err(unavailable())
            } else {
                std::future::pending().await
            }
        }
        async fn close(&self) -> Result<(), DesktopError> {
            self.closes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn native_fault_restores_only_after_the_previous_owner_drains() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let closes = Arc::new(AtomicUsize::new(0));
        let opened = Arc::new(tokio::sync::Notify::new());
        let closed = closes.clone();
        let ready = opened.clone();
        let host = Host::start("fixture", move |_stop, first| {
            let closes = closed.clone();
            if !first {
                assert_eq!(closes.load(Ordering::SeqCst), 1);
                ready.notify_one();
            }
            async move {
                Ok(Ready {
                    fault: first,
                    closes,
                })
            }
        });
        tokio::time::timeout(Duration::from_secs(5), opened.notified())
            .await
            .unwrap();
        assert_eq!(host.source().capabilities().unwrap().displays[0].index, 7);
        host.close().await.unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn pending_and_withdrawn_screen_never_fall_back_to_an_ambient_display() {
        let slot = Slot::default();
        assert!(slot.capabilities().is_err());
        assert!(
            slot.producer(7, Duration::from_millis(16), None, Some((640, 480)))
                .is_err()
        );
        slot.set(Some(Arc::new(Ready::default())));
        assert_eq!(slot.capabilities().unwrap().displays[0].index, 7);
        slot.set(None);
        assert!(slot.capabilities().is_err());
        assert!(slot.input(7, (640, 480)).is_err());
        assert!(!slot.clipboard_supported());
        assert!(slot.clipboard(7, true).is_err());
    }

    #[tokio::test]
    async fn shutdown_signal_is_retained_during_preparation_and_after_revocation() {
        let (stop, waiting) = watch::channel(false);
        stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), cancelled(waiting.clone()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), cancelled(waiting))
            .await
            .unwrap();
    }
}
