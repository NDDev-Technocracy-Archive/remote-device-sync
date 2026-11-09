//! In-memory admission and native drain for one original session or OS lease.
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;

pub(crate) struct Owner {
    closed: AtomicBool,
    active: AtomicUsize,
    changed: Notify,
    retained: OnceLock<super::NativeLifetime>,
}

impl Owner {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            closed: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            changed: Notify::new(),
            retained: OnceLock::new(),
        })
    }
    pub(crate) fn retain(&self, context: super::NativeLifetime) -> bool {
        if self.closed() || self.active.load(Ordering::Acquire) != 0 {
            return false;
        }
        match self.retained.set(context) {
            Ok(()) => true,
            Err(context) => self
                .retained
                .get()
                .is_some_and(|old| Arc::ptr_eq(old, &context)),
        }
    }
    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    pub(crate) fn begin(self: &Arc<Self>) -> Option<Running> {
        if self.closed() {
            return None;
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        let running = Running(self.clone());
        if self.closed() {
            return None;
        }
        Some(running)
    }
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
    pub(crate) async fn drain(&self) {
        self.close();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
    #[cfg(any(
        test,
        all(target_os = "linux", feature = "x11"),
        all(target_os = "macos", feature = "host")
    ))]
    pub(crate) async fn closed_signal(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.closed() {
                return;
            }
            changed.await;
        }
    }
}

pub(crate) struct Running(Arc<Owner>);
impl Running {
    pub(crate) fn closed(&self) -> bool {
        self.0.closed()
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.changed.notify_waiters();
        }
    }
}

#[cfg(any(
    all(test, target_os = "linux", feature = "x11"),
    all(target_os = "macos", feature = "host", feature = "viewer")
))]
pub(crate) mod probe;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closing_seals_admission_and_retains_the_original_context_through_native_drain() {
        let owner = Owner::new();
        let context = Arc::new(());
        let weak = Arc::downgrade(&context);
        assert!(owner.retain(context));
        assert!(!owner.retain(Arc::new(())));
        let running = owner.begin().unwrap();
        assert!(!running.closed());
        let waiter = owner.clone();
        let closed = tokio::spawn(async move { waiter.drain().await });
        owner.closed_signal().await;
        assert!(running.closed());
        assert!(owner.begin().is_none());
        assert!(!closed.is_finished());
        assert!(weak.upgrade().is_some());
        drop(owner);
        drop(running);
        closed.await.unwrap();
        assert!(weak.upgrade().is_none());
    }
}
