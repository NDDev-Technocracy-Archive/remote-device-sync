//! Newest owned native image, shared by consented platform captures.
use crate::{DesktopError, RawFrame};
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
        })
    }

    #[cfg(all(test, target_os = "linux", feature = "portal"))]
    pub(crate) fn fixture(index: u32, extent: (u32, u32)) -> Arc<Self> {
        Self::new(index, Some(extent))
    }

    /// Keep the last complete native update even before a producer exists.
    /// A variable-rate source need not emit another buffer when a viewer joins.
    /// Replacing one owned image retains the native buffer/byte budget; encoding
    /// remains owned by each admitted producer rather than this capture slot.
    pub fn publish(&self, frame: RawFrame) {
        if let Ok(mut state) = self.state.lock()
            && !state.failed
        {
            state.generation = state.generation.saturating_add(1);
            state.image = Some(Arc::new(Image {
                frame,
                received: Instant::now(),
                generation: state.generation,
            }));
        }
        self.changed.notify_all();
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

fn failure() -> DesktopError {
    DesktopError::Capture("native monitor unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Pixels {
        bytes: Vec<u8>,
        released: Arc<AtomicUsize>,
    }
    impl AsRef<[u8]> for Pixels {
        fn as_ref(&self) -> &[u8] {
            &self.bytes
        }
    }
    impl Drop for Pixels {
        fn drop(&mut self) {
            self.released.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn frame(value: u8, released: &Arc<AtomicUsize>) -> RawFrame {
        RawFrame {
            width: 16,
            height: 16,
            stride: 64,
            data: bytes::Bytes::from_owner(Pixels {
                bytes: [value, value, value, 255].repeat(16 * 16),
                released: released.clone(),
            }),
        }
    }

    #[test]
    fn an_inactive_monitor_retains_only_its_final_update_for_a_late_reader() {
        let slot = Slot::new(10, Some((16, 16)));
        let released = Arc::new(AtomicUsize::new(0));
        slot.publish(frame(0, &released));
        let final_update = Instant::now();
        slot.publish(frame(255, &released));
        assert_eq!(released.load(Ordering::Relaxed), 1);

        let image = slot.image(Duration::ZERO).unwrap();
        assert_eq!(slot.index, 10);
        assert_eq!(slot.extent().unwrap(), Some((16, 16)));
        assert_eq!(&image.frame.data[..4], &[255; 4]);
        assert_eq!(image.generation, 2);
        assert!(image.received >= final_update);
        drop(image);
        drop(slot);
        assert_eq!(released.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_closed_owner_releases_pixels_and_cannot_publish_a_late_completion() {
        let slot = Slot::new(10, Some((16, 16)));
        let released = Arc::new(AtomicUsize::new(0));
        slot.publish(frame(0, &released));
        slot.fail();
        assert_eq!(released.load(Ordering::Relaxed), 1);

        slot.publish(frame(255, &released));
        assert_eq!(released.load(Ordering::Relaxed), 2);
        assert!(slot.extent().is_err());
        assert!(slot.image(Duration::ZERO).is_err());
    }
}
