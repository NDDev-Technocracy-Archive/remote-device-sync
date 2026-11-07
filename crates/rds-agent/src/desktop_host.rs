//! Local screen consent has its own lifetime, independent of outgoing sessions.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rds_desktop::{DesktopError, DesktopSource, FrameProducer, InputSink, WaylandDesktop};
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
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Host {
    pub fn wayland(path: PathBuf, refresh: bool) -> Self {
        let source = Arc::new(Slot::default());
        let current = source.clone();
        let (stop, stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let opening = if refresh {
                WaylandDesktop::request_permission(&path, cancelled(stopping.clone())).await
            } else {
                WaylandDesktop::open_with_cancel(&path, cancelled(stopping.clone())).await
            };
            match opening {
                Ok(backend) => {
                    let backend = Arc::new(backend);
                    current.set(Some(backend.clone()));
                    tracing::info!("Wayland screen hosting ready");
                    tokio::select! {
                        _ = cancelled(stopping.clone()) => {},
                        result = backend.stopped() => {
                            tracing::warn!(?result, "Wayland screen hosting stopped; local sessions remain available");
                        },
                    }
                    current.set(None);
                    backend.close().await;
                }
                Err(error) => {
                    tracing::warn!(%error, "Wayland screen hosting unavailable; local sessions remain available");
                }
            }
            // Revocation is final for this local preparation. Never open a new
            // consent dialog because a remote peer asks for another screen.
            cancelled(stopping).await;
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

    pub async fn close(mut self) -> Result<(), tokio::task::JoinError> {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.await?;
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

    struct Ready;
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

    #[test]
    fn pending_and_withdrawn_screen_never_fall_back_to_an_ambient_display() {
        let slot = Slot::default();
        assert!(slot.capabilities().is_err());
        assert!(
            slot.producer(7, Duration::from_millis(16), None, Some((640, 480)))
                .is_err()
        );
        slot.set(Some(Arc::new(Ready)));
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
