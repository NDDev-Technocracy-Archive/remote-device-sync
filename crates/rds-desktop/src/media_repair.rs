//! Session-local repair epochs and ownership of the independent-picture gate.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;

const ACTIVE: u64 = 1;
const KEY_PENDING: u64 = 2;
const FLAGS: u64 = ACTIVE | KEY_PENDING;
const NEXT_EPOCH: u64 = FLAGS + 1;

pub(crate) struct MediaRepair {
    // Epoch and gate flags change atomically: an old producer/receipt must
    // never install or release the gate belonging to a replacement epoch.
    state: AtomicU64,
    wake: watch::Sender<()>,
    pub(crate) requests: AtomicU64,
    pub(crate) coalesced: AtomicU64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Accepted,
    Coalesced,
    Exhausted,
}

impl MediaRepair {
    pub(crate) fn new() -> (Arc<Self>, watch::Receiver<()>) {
        let (wake, changes) = watch::channel(());
        (
            Arc::new(Self {
                state: AtomicU64::new(0),
                wake,
                requests: AtomicU64::new(0),
                coalesced: AtomicU64::new(0),
            }),
            changes,
        )
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state.load(Ordering::Acquire) & !FLAGS
    }

    pub(crate) fn key_pending(&self) -> bool {
        self.state.load(Ordering::Acquire) & KEY_PENDING != 0
    }

    pub(crate) fn active(&self) -> bool {
        self.state.load(Ordering::Acquire) & ACTIVE != 0
    }

    pub(crate) fn request(&self) -> Request {
        match self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                if state & ACTIVE != 0 {
                    return None;
                }
                (state & !FLAGS)
                    .checked_add(NEXT_EPOCH)
                    .map(|epoch| epoch | ACTIVE)
            }) {
            Ok(_) => {
                self.requests.fetch_add(1, Ordering::Relaxed);
                self.wake.send_replace(());
                Request::Accepted
            }
            Err(state) if state & ACTIVE != 0 => {
                self.coalesced.fetch_add(1, Ordering::Relaxed);
                Request::Coalesced
            }
            Err(_) => Request::Exhausted,
        }
    }

    pub(crate) async fn changed(
        &self,
        changes: &mut watch::Receiver<()>,
        observed: u64,
    ) -> Result<(), watch::error::RecvError> {
        loop {
            if self.generation() != observed {
                return Ok(());
            }
            // A sender may publish its wake after the writer already noticed
            // the atomic epoch. Retire that wake without canceling fresh work.
            changes.changed().await?;
        }
    }

    pub(crate) fn key(
        self: &Arc<Self>,
        generation: u64,
        idr: Arc<crate::ProducerSignal>,
    ) -> Option<KeyPermit> {
        self.state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & !FLAGS == generation && state & KEY_PENDING == 0)
                    .then_some(state | KEY_PENDING)
            })
            .ok()?;
        Some(KeyPermit {
            repair: self.clone(),
            generation,
            idr,
            confirmed: false,
        })
    }

    fn release_key(&self, generation: u64, confirmed: bool) {
        let _ = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                if state & !FLAGS != generation {
                    return None;
                }
                Some(state & !(KEY_PENDING | if confirmed { ACTIVE } else { 0 }))
            });
    }
}

pub(crate) struct KeyPermit {
    repair: Arc<MediaRepair>,
    generation: u64,
    idr: Arc<crate::ProducerSignal>,
    pub(crate) confirmed: bool,
}

impl Drop for KeyPermit {
    fn drop(&mut self) {
        if self.repair.generation() == self.generation {
            if !self.confirmed {
                // A fresh key taken from the queue can be dropped with a
                // canceled write. Rebuild it before admitting any successor.
                self.idr.store(true, Ordering::Release);
            }
            self.repair.release_key(self.generation, self.confirmed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_key_cannot_install_or_release_the_replacement_gate() {
        let (repair, _) = MediaRepair::new();
        let idr = crate::ProducerSignal::new(false, Arc::default());
        let mut old = repair.key(0, idr.clone()).unwrap();
        assert_eq!(repair.request(), Request::Accepted);
        let epoch = repair.generation();
        assert!(repair.key(0, idr.clone()).is_none());
        let mut fresh = repair.key(epoch, idr.clone()).unwrap();
        old.confirmed = true;
        drop(old);
        assert!(repair.key_pending());
        assert!(repair.active());
        assert!(!idr.load(Ordering::Acquire));
        fresh.confirmed = true;
        drop(fresh);
        assert!(!repair.key_pending());
        assert!(!repair.active());
    }

    #[test]
    fn canceled_fresh_key_releases_credit_but_keeps_repair_active() {
        let (repair, _) = MediaRepair::new();
        let idr = crate::ProducerSignal::new(false, Arc::default());
        assert_eq!(repair.request(), Request::Accepted);
        drop(repair.key(repair.generation(), idr.clone()).unwrap());
        assert!(!repair.key_pending());
        assert!(repair.active());
        assert!(idr.load(Ordering::Acquire));
        assert_eq!(repair.request(), Request::Coalesced);
        assert!(repair.key(repair.generation(), idr).is_some());
    }

    #[test]
    fn repeated_requests_preserve_an_active_recovery_picture() {
        let (repair, _) = MediaRepair::new();
        let idr = crate::ProducerSignal::new(false, Arc::default());
        assert_eq!(repair.request(), Request::Accepted);
        let epoch = repair.generation();
        let mut key = repair.key(epoch, idr.clone()).unwrap();
        for _ in 0..100 {
            assert_eq!(repair.request(), Request::Coalesced);
            assert_eq!(repair.generation(), epoch);
        }
        assert!(repair.key_pending());
        assert!(!idr.load(Ordering::Acquire));
        key.confirmed = true;
        drop(key);
        assert_eq!(repair.request(), Request::Accepted);
        assert!(repair.generation() > epoch);
    }
}
