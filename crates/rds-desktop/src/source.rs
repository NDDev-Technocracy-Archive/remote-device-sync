//! Serving backend ownership stays in the agent, below transport/policy APIs.

use crate::{DesktopError, FrameProducer, InputSink};
use std::time::Duration;

#[cfg(all(target_os = "linux", feature = "portal"))]
pub(crate) const NATIVE_DRAIN_DIAGNOSTIC: Duration = Duration::from_secs(5);

#[cfg(all(target_os = "linux", feature = "portal"))]
pub(crate) async fn drain_native(
    mut task: tokio::task::JoinHandle<Result<(), DesktopError>>,
    backend: &'static str,
) -> Result<(), DesktopError> {
    let result = match tokio::time::timeout(NATIVE_DRAIN_DIAGNOSTIC, &mut task).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                backend,
                "native shutdown still running; retaining its owner"
            );
            task.await
        }
    };
    result
        .map_err(|_| DesktopError::Capture(format!("{backend} native worker ended unexpectedly")))?
}

/// One already-prepared desktop backend. Inventory must be an in-memory read;
/// native capture/encoder initialization runs on the serving capture worker.
/// Consent is local and precedes serving; remote requests cannot open a portal.
pub trait DesktopSource: Send + Sync + 'static {
    fn capabilities(&self) -> Result<rds_core::DesktopCaps, DesktopError>;
    fn producer(
        &self,
        display: u32,
        interval: Duration,
        height: Option<u32>,
        extent: Option<(u32, u32)>,
    ) -> Result<Box<dyn FrameProducer>, DesktopError>;
    /// Return a prepared in-memory lease; this method must not perform native I/O.
    fn input(&self, display: u32, extent: (u32, u32)) -> Result<Box<dyn InputSink>, DesktopError>;
    /// An absent native clipboard must never probe an unrelated backend.
    fn clipboard_supported(&self) -> bool {
        false
    }
    /// Create an in-memory clipboard lease; never opens consent or a different
    /// platform backend. Watching starts after this lease's initial snapshot.
    fn clipboard(
        &self,
        _display: u32,
        _watching: bool,
    ) -> Result<Box<dyn crate::clipboard::ClipboardBackend>, DesktopError> {
        Err(DesktopError::Input("clipboard backend unavailable".into()))
    }
}

#[cfg(all(test, target_os = "linux", feature = "portal"))]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[tokio::test(start_paused = true)]
    async fn a_slow_native_call_remains_owned_after_the_shutdown_diagnostic() {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, waiting) = std::sync::mpsc::channel();
        let ended = Arc::new(AtomicBool::new(false));
        let finished = ended.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _ = entered.send(());
            waiting.recv_timeout(Duration::from_secs(10)).unwrap();
            finished.store(true, Ordering::Release);
            Ok(())
        });
        started.await.unwrap();
        let closing = drain_native(task, "fixture");
        tokio::pin!(closing);
        assert!(
            tokio::time::timeout(
                NATIVE_DRAIN_DIAGNOSTIC + Duration::from_secs(1),
                &mut closing
            )
            .await
            .is_err()
        );
        assert!(!ended.load(Ordering::Acquire));
        tokio::time::resume();
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), closing)
            .await
            .unwrap()
            .unwrap();
        assert!(ended.load(Ordering::Acquire));
    }
}
