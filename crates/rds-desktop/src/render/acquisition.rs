//! One native surface operation per window, independent of the UI thread.
use crate::DesktopError;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tokio::{
    sync::{Semaphore, oneshot},
    task::{AbortHandle, JoinHandle},
};
use winit::event_loop::EventLoopProxy;

// Ended windows can still have a running native call. Retain its reservation
// until that call actually returns, before admitting replacement native work.
static NATIVE: Semaphore = Semaphore::const_new(super::workspace::MAX_TABS);

pub(super) struct Completed {
    // Release any acquired image before its original window/surface owner.
    pub texture: wgpu::CurrentSurfaceTexture,
    pub configuration_us: Option<u64>,
    pub acquisition_us: u64,
    _surface: Arc<wgpu::Surface<'static>>,
}

pub(super) struct Acquisition {
    pub epoch: u64,
    pub retired: bool,
    pub queued: Instant,
    receiver: oneshot::Receiver<Result<Completed, DesktopError>>,
    ready: Arc<AtomicBool>,
    task: JoinHandle<()>,
}
struct AbortQueued(AbortHandle);
impl Drop for AbortQueued {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn micros(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

async fn native<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, DesktopError> {
    let permit = NATIVE
        .acquire()
        .await
        .map_err(|_| DesktopError::Capture("surface worker budget closed".into()))?;
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    });
    let _abort_queued = AbortQueued(worker.abort_handle());
    worker
        .await
        .map_err(|_| DesktopError::Capture("native surface worker failed".into()))
}

impl Acquisition {
    pub fn start(
        surface: Arc<wgpu::Surface<'static>>,
        device: wgpu::Device,
        configuration: Option<wgpu::SurfaceConfiguration>,
        epoch: u64,
        proxy: EventLoopProxy<()>,
    ) -> Result<Self, DesktopError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            DesktopError::Capture("native renderer requires its worker runtime".into())
        })?;
        let queued = Instant::now();
        let (sender, receiver) = oneshot::channel();
        let ready = Arc::new(AtomicBool::new(false));
        let completed = ready.clone();
        let task = runtime.spawn(async move {
            let result = native(move || {
                let configuration_us = configuration.map(|config| {
                    let started = Instant::now();
                    surface.configure(&device, &config);
                    micros(started)
                });
                let started = Instant::now();
                let texture = surface.get_current_texture();
                Completed {
                    texture,
                    configuration_us,
                    acquisition_us: micros(started),
                    _surface: surface,
                }
            })
            .await;
            let _ = sender.send(result);
            completed.store(true, Ordering::Release);
            let _ = proxy.send_event(());
        });
        Ok(Self {
            epoch,
            retired: false,
            queued,
            receiver,
            ready,
            task,
        })
    }
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    pub fn take(&mut self) -> Result<Option<Completed>, DesktopError> {
        match self.receiver.try_recv() {
            Ok(result) => result.map(Some),
            Err(oneshot::error::TryRecvError::Empty) => Ok(None),
            Err(oneshot::error::TryRecvError::Closed) => Err(DesktopError::Capture(
                "surface worker ended without its result".into(),
            )),
        }
    }
}

impl Drop for Acquisition {
    fn drop(&mut self) {
        // Abort waiting/queued work. A running native closure owns its surface,
        // window, device and global reservation through actual completion.
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[derive(Debug)]
    struct Original {
        file: std::fs::File,
        dropped: Option<oneshot::Sender<()>>,
    }
    impl Drop for Original {
        fn drop(&mut self) {
            if let Some(done) = self.dropped.take() {
                let _ = done.send(());
            }
        }
    }
    #[tokio::test]
    async fn cancellation_drops_waiting_work_but_retains_a_running_native_owner_and_budget() {
        let path =
            std::env::temp_dir().join(format!("rds-surface-owner-{:032x}", rand::random::<u128>()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let all = NATIVE
            .acquire_many(super::super::workspace::MAX_TABS as u32)
            .await
            .unwrap();
        let (dropped, drained) = oneshot::channel();
        let original = Arc::new(Original {
            file: file.try_clone().unwrap(),
            dropped: Some(dropped),
        });
        let retained = Arc::downgrade(&original);
        let called = Arc::new(AtomicBool::new(false));
        let observed = called.clone();
        let (started, ready) = oneshot::channel();
        let waiting = tokio::spawn(async move {
            started.send(()).unwrap();
            native(move || {
                observed.store(true, Ordering::Release);
                original
            })
            .await
        });
        ready.await.unwrap();
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), drained)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.upgrade().is_none());
        assert!(!called.load(Ordering::Acquire));
        drop(all);

        let (dropped, drained) = oneshot::channel();
        let original = Arc::new(Original {
            file,
            dropped: Some(dropped),
        });
        let retained = Arc::downgrade(&original);
        let (release, held) = std::sync::mpsc::channel();
        let (started, ready) = oneshot::channel();
        let running = tokio::spawn(native(move || {
            assert!(original.file.metadata().unwrap().is_file());
            started.send(()).unwrap();
            held.recv_timeout(Duration::from_secs(3)).unwrap();
            assert_eq!(original.file.metadata().unwrap().len(), 0);
            original
        }));
        ready.await.unwrap();
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        assert!(retained.upgrade().is_some());
        assert_eq!(
            NATIVE.available_permits(),
            super::super::workspace::MAX_TABS - 1
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), drained)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.upgrade().is_none());
        assert_eq!(
            NATIVE.available_permits(),
            super::super::workspace::MAX_TABS
        );
        std::fs::remove_file(path).unwrap();
    }
}
