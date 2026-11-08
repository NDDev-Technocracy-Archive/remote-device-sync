//! Newest owned native image, shared by consented platform captures.
use crate::{DesktopError, RawFrame};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

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
    subscribers: Mutex<Vec<Weak<crate::producer_signal::Wake>>>,
}

impl Slot {
    pub(crate) fn new(index: u32, extent: Option<(u32, u32)>) -> Arc<Self> {
        Arc::new(Self {
            index,
            state: Mutex::new(State {
                extent,
                ..Default::default()
            }),
            subscribers: Mutex::new(Vec::new()),
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
        self.notify();
    }
    /// Native source admission already bounds live producers. Dead weak
    /// subscriptions are removed before a new producer can register.
    pub(crate) fn observe(
        &self,
        wake: &Arc<crate::producer_signal::Wake>,
    ) -> Result<(), DesktopError> {
        let mut subscribers = self.subscribers.lock().map_err(|_| failure())?;
        subscribers.retain(|item| item.strong_count() > 0);
        let weak = Arc::downgrade(wake);
        if !subscribers.iter().any(|item| item.ptr_eq(&weak)) {
            subscribers.push(weak);
        }
        Ok(())
    }
    pub(crate) fn notify(&self) {
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|weak| match weak.upgrade() {
                Some(wake) => {
                    wake.notify();
                    true
                }
                None => false,
            });
        }
    }
    pub(crate) fn current(&self) -> Result<Option<Arc<Image>>, DesktopError> {
        let state = self.state.lock().map_err(|_| failure())?;
        if state.failed {
            return Err(failure());
        }
        Ok(state.image.clone())
    }
    pub fn extent(&self) -> Result<Option<(u32, u32)>, DesktopError> {
        let state = self.state.lock().map_err(|_| failure())?;
        if state.failed {
            return Err(failure());
        }
        Ok(state.extent)
    }

    pub(crate) fn fail(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.failed = true;
            state.image = None;
        }
        self.notify();
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

    #[tokio::test]
    async fn the_native_image_and_failure_wake_the_original_subscriber_without_retaining_it() {
        let slot = Slot::new(0, Some((16, 16)));
        let controls = crate::ProducerControls::new(1);
        let token = controls.wake.snapshot().unwrap();
        slot.observe(&controls.wake).unwrap();
        let weak = Arc::downgrade(&controls.wake);
        let released = Arc::new(AtomicUsize::new(0));
        slot.publish(frame(255, &released));
        let wake = controls.wake.clone();
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            tokio::task::spawn_blocking(move || {
                wake.wait(token, Instant::now() + std::time::Duration::from_secs(5))
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(slot.current().unwrap().unwrap().frame.data[0], 255);
        let token = controls.wake.snapshot().unwrap();
        slot.fail();
        assert!(slot.current().is_err());
        assert_ne!(controls.wake.snapshot(), Some(token));
        drop(controls);
        assert!(weak.upgrade().is_none());
        slot.notify();
        assert!(slot.subscribers.lock().unwrap().is_empty());
        assert_eq!(released.load(Ordering::Acquire), 1);
    }

    #[test]
    fn an_inactive_monitor_retains_only_its_final_update_for_a_late_reader() {
        let slot = Slot::new(10, Some((16, 16)));
        let released = Arc::new(AtomicUsize::new(0));
        slot.publish(frame(0, &released));
        let final_update = Instant::now();
        slot.publish(frame(255, &released));
        assert_eq!(released.load(Ordering::Relaxed), 1);

        let image = slot.current().unwrap().unwrap();
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
        assert!(slot.current().is_err());
    }
}
