//! Prepared macOS screen ownership, independent of the transport/session owner.

use super::{geometry::Geometry, latest::Slot, producer, sck_native};
use crate::{DesktopError, DesktopSource, FrameProducer, InputSink};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore, oneshot};

/// ScreenCaptureKit capture and CGEvent input. Opening never requests OS
/// permissions: the local application must obtain them explicitly beforehand.
pub struct MacDesktop {
    geometries: Vec<Geometry>,
    slots: Vec<Arc<Slot>>,
    input: Arc<crate::input::macos::Group>,
    producers: Arc<Semaphore>,
    stop: Arc<AtomicBool>,
    task: Mutex<Option<tokio::task::JoinHandle<Result<(), DesktopError>>>>,
}

impl MacDesktop {
    /// Explicit local setup only. Remote requests never invoke these OS prompts.
    pub fn request_permission() -> Result<(), DesktopError> {
        use objc2_core_graphics::{CGRequestPostEventAccess, CGRequestScreenCaptureAccess};
        let screen = CGRequestScreenCaptureAccess();
        let input = CGRequestPostEventAccess();
        if screen && input {
            Ok(())
        } else {
            Err(sck_native::error())
        }
    }

    pub async fn open_with_cancel(
        cancelled: impl Future<Output = ()>,
    ) -> Result<Self, DesktopError> {
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
        tokio::pin!(cancelled);
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
        Ok(Self {
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
        let g = *self
            .geometries
            .iter()
            .find(|g| g.id == display)
            .ok_or_else(sck_native::error)?;
        let slot = self
            .slots
            .iter()
            .find(|slot| slot.index == display)
            .ok_or_else(sck_native::error)?;
        if slot.extent()? != Some((g.width, g.height)) {
            return Err(sck_native::error());
        }
        Ok((g, slot.clone()))
    }
}

impl Drop for MacDesktop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl DesktopSource for MacDesktop {
    fn capabilities(&self) -> Result<rds_core::DesktopCaps, DesktopError> {
        let displays = self
            .geometries
            .iter()
            .map(|g| self.selected(g.id).map(|(g, _)| g.info()))
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
