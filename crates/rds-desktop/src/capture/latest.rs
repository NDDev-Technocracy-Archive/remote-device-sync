//! Newest owned native image, shared by consented platform captures.
use crate::{DesktopError, RawFrame};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) struct Image {
    pub frame: RawFrame,
    /// Local native ingress, not compositor scanout or physical presentation.
    pub received: Instant,
    pub generation: u64,
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) extent: Option<(u32, u32)>,
    pub(crate) image: Option<Arc<Image>>,
    pub(crate) failed: bool,
    pub(crate) generation: u64,
}

pub(crate) struct Slot {
    pub index: u32,
    pub(crate) state: Mutex<State>,
    pub(crate) changed: Condvar,
    pub(crate) readers: AtomicUsize,
}

impl Slot {
    pub(crate) fn new(index: u32, extent: Option<(u32, u32)>) -> Arc<Self> {
        Arc::new(Self {
            index,
            state: Mutex::new(State {
                extent,
                ..Default::default()
            }),
            changed: Condvar::new(),
            readers: AtomicUsize::new(0),
        })
    }

    #[cfg(all(test, target_os = "linux", feature = "portal"))]
    pub(crate) fn fixture(index: u32, extent: (u32, u32)) -> Arc<Self> {
        Arc::new(Self {
            index,
            state: Mutex::new(State {
                extent: Some(extent),
                ..Default::default()
            }),
            changed: Condvar::new(),
            readers: AtomicUsize::new(0),
        })
    }
    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        self.readers.fetch_add(1, Ordering::AcqRel);
        Subscription(self.clone())
    }
    pub fn extent(&self) -> Result<Option<(u32, u32)>, DesktopError> {
        let state = self.state.lock().map_err(|_| failure())?;
        if state.failed {
            return Err(failure());
        }
        Ok(state.extent)
    }

    pub fn image(&self, timeout: Duration) -> Result<Arc<Image>, DesktopError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().map_err(|_| failure())?;
        while state.image.is_none() && !state.failed {
            let wait = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(failure)?;
            state = self
                .changed
                .wait_timeout(state, wait)
                .map_err(|_| failure())?
                .0;
        }
        if state.failed {
            return Err(failure());
        }
        state.image.clone().ok_or_else(failure)
    }

    pub(crate) fn fail(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.failed = true;
            state.image = None;
        }
        self.changed.notify_all();
    }
}

pub(crate) struct Subscription(pub Arc<Slot>);
impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.readers.fetch_sub(1, Ordering::AcqRel);
    }
}

fn failure() -> DesktopError {
    DesktopError::Capture("native monitor unavailable".into())
}
