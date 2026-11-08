//! Prepared macOS screen ownership, independent of the transport/session owner.

use super::{geometry::Geometry, latest::Slot, producer, sck_native};
use crate::{DesktopError, DesktopSource, FrameProducer, InputSink};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore, oneshot};

/// Read-only local OS grants. Inspecting these never opens a consent prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacPermissions {
    pub screen_recording: bool,
    pub accessibility: bool,
}

impl MacPermissions {
    fn ready(self) -> bool {
        self.screen_recording && self.accessibility
    }
}

async fn wait_for_permission(
    mut status: impl FnMut() -> MacPermissions,
    cancelled: impl Future<Output = ()>,
) -> Result<(), DesktopError> {
    tokio::pin!(cancelled);
    let mut previous = None;
    loop {
        let current = status();
        if current.ready() {
            return Ok(());
        }
        if previous != Some(current) {
            tracing::info!(?current, "waiting for local macOS screen permissions");
            previous = Some(current);
        }
        tokio::select! {
            biased;
            _ = &mut cancelled => return Err(sck_native::error()),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}

/// ScreenCaptureKit capture and CGEvent input. Opening never requests OS
/// permissions: the local application must obtain them explicitly beforehand.
pub struct MacDesktop {
    clipboard: Option<crate::clipboard::macos::Group>,
    geometries: Vec<Geometry>,
    slots: Vec<Arc<Slot>>,
    input: Arc<crate::input::macos::Group>,
    producers: Arc<Semaphore>,
    stop: Arc<AtomicBool>,
    task: Mutex<Option<tokio::task::JoinHandle<Result<(), DesktopError>>>>,
}

impl MacDesktop {
    pub fn permission_status() -> MacPermissions {
        MacPermissions {
            screen_recording: objc2_core_graphics::CGPreflightScreenCaptureAccess(),
            accessibility: crate::input::macos::available(),
        }
    }

    /// Explicit local setup only. Remote requests never invoke these OS prompts.
    pub fn request_permission() -> Result<(), DesktopError> {
        use objc2_core_graphics::{CGRequestPostEventAccess, CGRequestScreenCaptureAccess};
        let screen = CGRequestScreenCaptureAccess();
        let input = CGRequestPostEventAccess();
        if screen && input {
            Ok(())
        } else {
            Err(DesktopError::Capture(format!(
                "local macOS grants: Screen Recording={screen}, Accessibility={input}; enable the missing grant in Privacy & Security"
            )))
        }
    }

    pub async fn open_with_cancel(
        cancelled: impl Future<Output = ()>,
    ) -> Result<Self, DesktopError> {
        tokio::pin!(cancelled);
        // A resident manager can start before the user completes OS setup.
        // Observe later local grants without repeating either permission API
        // or allocating a native worker while access is missing.
        wait_for_permission(Self::permission_status, &mut cancelled).await?;
        let clipboard = match crate::clipboard::native::Policy::shipped()
            .and_then(crate::clipboard::macos::Group::start)
        {
            Ok(group) => Some(group),
            Err(error) => {
                tracing::warn!(%error,"native clipboard unavailable; screen preparation continues");
                None
            }
        };
        static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let permit = WORKERS
            .get_or_init(|| Arc::new(Semaphore::new(4)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| sck_native::error())?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let (ready, waiting) = oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            struct Exit(Arc<AtomicBool>);
            impl Drop for Exit {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let _exit = Exit(stopping.clone());
            sck_native::run(&stopping, ready)
        });
        let prepared = tokio::select! {
            biased;
            _ = &mut cancelled => None,
            ready = waiting => ready.ok().and_then(Result::ok),
        };
        let Some(prepared) = prepared else {
            stop.store(true, Ordering::Release);
            let _ = task.await;
            return Err(sck_native::error());
        };
        if let Some(group) = &clipboard {
            tokio::select! {
                biased;
                _ = &mut cancelled => {
                    stop.store(true, Ordering::Release);
                    let _ = task.await;
                    return Err(sck_native::error());
                },
                result = group.ready() => if let Err(error) = result {
                    tracing::warn!(%error,"native clipboard not ready; screen remains available");
                },
            }
        }
        Ok(Self {
            clipboard,
            geometries: prepared.geometries,
            slots: prepared.slots,
            input: prepared.input,
            producers: Arc::new(Semaphore::new(8)),
            stop,
            task: Mutex::new(Some(task)),
        })
    }

    pub async fn stopped(&self) -> Result<(), DesktopError> {
        while !self.stop.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(sck_native::error())
    }

    /// Return only after the native stop completions and queued callbacks drain.
    pub async fn close(&self) -> Result<(), DesktopError> {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.lock().await.take() {
            task.await.map_err(|_| sck_native::error())??;
        }
        Ok(())
    }

    fn selected(&self, display: u32) -> Result<(Geometry, Arc<Slot>), DesktopError> {
        if self.stop.load(Ordering::Acquire) {
            return Err(sck_native::error());
        }
        let (g, slot) = self
            .geometries
            .iter()
            .zip(&self.slots)
            .find(|(_, slot)| slot.index == display)
            .ok_or_else(sck_native::error)?;
        if slot.extent()? != Some((g.width, g.height)) {
            return Err(sck_native::error());
        }
        Ok((*g, slot.clone()))
    }
}

impl Drop for MacDesktop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl DesktopSource for MacDesktop {
    fn native_clipboard(
        &self,
        display: u32,
        original: crate::clipboard::NativeLifetime,
    ) -> Result<Arc<dyn crate::clipboard::native::Provider>, DesktopError> {
        self.selected(display)?;
        self.clipboard
            .as_ref()
            .ok_or_else(|| DesktopError::Input("native clipboard unavailable".into()))?
            .native(original)
            .map_err(|error| DesktopError::Input(error.to_string()))
    }
    fn clipboard_supported(&self) -> bool {
        self.clipboard
            .as_ref()
            .is_some_and(|group| group.available())
    }
    fn clipboard(
        &self,
        display: u32,
        watching: bool,
    ) -> Result<Box<dyn crate::clipboard::ClipboardBackend>, DesktopError> {
        self.selected(display)?;
        self.clipboard
            .as_ref()
            .ok_or_else(|| DesktopError::Input("native clipboard unavailable".into()))?
            .worker(watching)
    }
    fn capabilities(&self) -> Result<rds_core::DesktopCaps, DesktopError> {
        let displays = self
            .geometries
            .iter()
            .map(|g| self.selected(g.info().index).map(|(g, _)| g.info()))
            .collect::<Result<_, _>>()?;
        Ok(rds_core::DesktopCaps {
            displays,
            codecs: vec![rds_core::Codec::H264],
        })
    }
    fn producer(
        &self,
        display: u32,
        interval: Duration,
        height: Option<u32>,
        extent: Option<(u32, u32)>,
    ) -> Result<Box<dyn FrameProducer>, DesktopError> {
        let (_, slot) = self.selected(display)?;
        let permit = self
            .producers
            .clone()
            .try_acquire_owned()
            .map_err(|_| sck_native::error())?;
        Ok(Box::new(producer::Producer::new(
            permit, slot, interval, height, extent,
        )?))
    }
    fn input(&self, display: u32, extent: (u32, u32)) -> Result<Box<dyn InputSink>, DesktopError> {
        let (g, _) = self.selected(display)?;
        if extent != (g.width, g.height) {
            return Err(sck_native::error());
        }
        self.input.sink(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_late_local_grant_resumes_preparation_without_a_new_request() {
        let granted = Arc::new(AtomicBool::new(false));
        let observed = granted.clone();
        let pending = tokio::spawn(wait_for_permission(
            move || MacPermissions {
                screen_recording: true,
                accessibility: observed.load(Ordering::Acquire),
            },
            std::future::pending(),
        ));
        tokio::task::yield_now().await;
        assert!(!pending.is_finished());
        granted.store(true, Ordering::Release);
        tokio::time::advance(Duration::from_secs(1)).await;
        pending.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn missing_permission_keeps_preparation_cancelable() {
        wait_for_permission(
            || MacPermissions {
                screen_recording: false,
                accessibility: true,
            },
            std::future::ready(()),
        )
        .await
        .unwrap_err();
    }
}
