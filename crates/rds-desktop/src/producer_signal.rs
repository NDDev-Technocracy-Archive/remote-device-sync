//! Level-triggered producer steering and native damage share one sleeping owner.
use std::sync::{
    Arc, Condvar, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
#[cfg(any(
    test,
    all(target_os = "linux", feature = "x11"),
    all(target_os = "macos", feature = "host")
))]
use std::time::Instant;
#[cfg(all(target_os = "linux", feature = "x11"))]
pub(crate) mod polling;

#[derive(Default)]
struct State {
    generation: u64,
    closed: bool,
}
#[derive(Default)]
pub(crate) struct Wake {
    state: Mutex<State>,
    changed: Condvar,
    ended: Condvar,
    #[cfg(all(target_os = "linux", feature = "x11"))]
    polling: Mutex<Option<std::sync::Weak<polling::Pulse>>>,
}
impl Wake {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.closed = true;
            self.changed.notify_all();
            self.ended.notify_all();
            state
        })
    }
    #[cfg(any(
        test,
        all(target_os = "linux", feature = "x11"),
        all(target_os = "macos", feature = "host")
    ))]
    pub(crate) fn snapshot(&self) -> Option<u64> {
        let state = self.state();
        (!state.closed).then_some(state.generation)
    }
    #[cfg(all(target_os = "linux", feature = "x11"))]
    pub(crate) fn polling(&self) -> std::io::Result<Arc<polling::Pulse>> {
        let mut held = self
            .polling
            .lock()
            .map_err(|_| std::io::Error::other("native producer wake owner failed"))?;
        if let Some(pulse) = held.as_ref().and_then(std::sync::Weak::upgrade) {
            return Ok(pulse);
        }
        let pulse = Arc::new(polling::Pulse::new()?);
        *held = Some(Arc::downgrade(&pulse));
        Ok(pulse)
    }
    fn signal_poll(&self) {
        #[cfg(all(target_os = "linux", feature = "x11"))]
        if let Ok(held) = self.polling.lock()
            && let Some(pulse) = held.as_ref().and_then(std::sync::Weak::upgrade)
        {
            pulse.signal();
        }
    }
    pub(crate) fn notify(&self) {
        let mut state = self.state();
        match state.generation.checked_add(1) {
            Some(next) => state.generation = next,
            None => state.closed = true,
        }
        self.changed.notify_all();
        if state.closed {
            self.ended.notify_all();
        }
        self.signal_poll();
    }
    /// The caller sampled before reading image/control levels. A notification
    /// between those reads and this wait cannot disappear into a lost wake.
    #[cfg(any(
        test,
        all(target_os = "linux", feature = "portal"),
        all(target_os = "macos", feature = "host")
    ))]
    pub(crate) fn wait(&self, generation: u64, deadline: Instant) -> bool {
        let mut state = self.state();
        while state.generation == generation && !state.closed {
            let Some(wait) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            state = match self.changed.wait_timeout(state, wait) {
                Ok((state, _)) => state,
                Err(poisoned) => {
                    let (mut state, _) = poisoned.into_inner();
                    state.closed = true;
                    self.changed.notify_all();
                    self.ended.notify_all();
                    state
                }
            };
        }
        !state.closed
    }
    #[cfg(any(
        test,
        all(target_os = "linux", feature = "x11"),
        all(target_os = "macos", feature = "host")
    ))]
    pub(crate) fn until(&self, deadline: Instant) -> bool {
        let mut state = self.state();
        while !state.closed {
            let Some(wait) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            state = match self.ended.wait_timeout(state, wait) {
                Ok((state, _)) => state,
                Err(poisoned) => {
                    let (mut state, _) = poisoned.into_inner();
                    state.closed = true;
                    self.changed.notify_all();
                    self.ended.notify_all();
                    state
                }
            };
        }
        !state.closed
    }
    pub(crate) fn close(&self) {
        self.state().closed = true;
        self.changed.notify_all();
        self.ended.notify_all();
        self.signal_poll();
    }
}

/// A producer flag preserves atomic load/swap semantics and wakes its original
/// producer whenever work is requested. Construct controls with ProducerControls.
pub struct ProducerSignal {
    value: AtomicBool,
    wake: Arc<Wake>,
}
impl ProducerSignal {
    pub(crate) fn new(value: bool, wake: Arc<Wake>) -> Arc<Self> {
        Arc::new(Self {
            value: AtomicBool::new(value),
            wake,
        })
    }
    pub fn load(&self, order: Ordering) -> bool {
        self.value.load(order)
    }
    pub fn store(&self, value: bool, order: Ordering) {
        self.value.store(value, order);
        if value {
            self.wake.notify();
        }
    }
    pub fn swap(&self, value: bool, order: Ordering) -> bool {
        let before = self.value.swap(value, order);
        if value {
            self.wake.notify();
        }
        before
    }
}

pub(crate) struct Stop(pub Arc<Wake>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[tokio::test]
    async fn requested_work_survives_the_gap_before_wait_and_cadence_waits_only_for_stop() {
        let controls = crate::ProducerControls::new(1);
        controls.idr.store(false, Ordering::Release);
        for flag in [&controls.idr, &controls.input_refresh_pending] {
            let generation = controls.wake.snapshot().unwrap();
            flag.store(true, Ordering::Release);
            assert!(flag.swap(false, Ordering::AcqRel));
            let wake = controls.wake.clone();
            tokio::time::timeout(
                Duration::from_millis(250),
                tokio::task::spawn_blocking(move || {
                    wake.wait(generation, Instant::now() + Duration::from_secs(5))
                }),
            )
            .await
            .unwrap()
            .unwrap();
        }
        let wake = controls.wake.clone();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let waiting = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            wake.until(Instant::now() + Duration::from_secs(5))
        });
        ready.await.unwrap();
        controls.idr.store(true, Ordering::Release);
        assert!(
            !waiting.is_finished(),
            "damage/steering must not bypass frame cadence"
        );
        controls.wake.close();
        assert!(
            !tokio::time::timeout(Duration::from_millis(250), waiting)
                .await
                .unwrap()
                .unwrap()
        );
    }
}
