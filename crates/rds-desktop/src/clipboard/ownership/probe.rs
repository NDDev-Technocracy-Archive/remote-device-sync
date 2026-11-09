//! A native qualification gate at actual original-purpose destruction.
use super::super::{NativeLifetime, native::Error};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::oneshot, time::timeout};

pub(crate) struct Cleanup {
    entered: oneshot::Receiver<()>,
    release: Option<std::sync::mpsc::Sender<()>>,
    completed: oneshot::Receiver<bool>,
}
struct Original {
    entered: Option<oneshot::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    completed: Option<oneshot::Sender<bool>>,
}
impl Drop for Original {
    fn drop(&mut self) {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        let released = self
            .release
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(Duration::from_secs(10))
            .is_ok();
        if let Some(completed) = self.completed.take() {
            let _ = completed.send(released);
        }
    }
}
impl Cleanup {
    pub(crate) fn held() -> (NativeLifetime, Self) {
        let (entered, started) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let (completed, finished) = oneshot::channel();
        (
            Arc::new(Original {
                entered: Some(entered),
                release: Mutex::new(blocked),
                completed: Some(completed),
            }),
            Self {
                entered: started,
                release: Some(release),
                completed: finished,
            },
        )
    }
    pub(crate) async fn entered(&mut self) -> Result<(), Error> {
        timeout(Duration::from_secs(10), &mut self.entered)
            .await
            .map_err(|_| Error::Unavailable)?
            .map_err(|_| Error::Unavailable)
    }
    pub(crate) async fn release(mut self) -> Result<(), Error> {
        self.release
            .take()
            .ok_or(Error::Unavailable)?
            .send(())
            .map_err(|_| Error::Unavailable)?;
        let released = timeout(Duration::from_secs(10), &mut self.completed)
            .await
            .map_err(|_| Error::Unavailable)?
            .map_err(|_| Error::Unavailable)?;
        if released {
            Ok(())
        } else {
            Err(Error::Unavailable)
        }
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}
