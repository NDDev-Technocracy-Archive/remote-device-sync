//! Refusals belong to admitted incoming bodies, before metadata is published.
use super::*;
use std::{collections::HashMap, sync::Weak};
use tokio::sync::watch;

#[derive(Default)]
pub(super) struct Refusals(Mutex<HashMap<TransferId, Weak<Refusal>>>);
impl Refusals {
    pub fn track(self: &Arc<Self>, id: TransferId) -> Result<Arc<Refusal>, Error> {
        let mut pending = self.0.lock().map_err(|_| Error::Closed)?;
        if pending.contains_key(&id) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let (state, _) = watch::channel(None);
        let refusal = Arc::new(Refusal {
            id,
            state,
            pending: Arc::downgrade(self),
        });
        pending.insert(id, Arc::downgrade(&refusal));
        Ok(refusal)
    }
    pub fn signal(&self, id: TransferId, code: Failure) -> Result<bool, Error> {
        // Drop the map lock before upgrading/dropping a slot: its destructor
        // removes that same entry after the body's last native owner drains.
        let slot = self.0.lock().map_err(|_| Error::Closed)?.get(&id).cloned();
        Ok(slot.and_then(|slot| slot.upgrade()).is_some_and(|slot| {
            slot.state.send_if_modified(|state| {
                if state.is_some() {
                    return false;
                }
                *state = Some(code);
                true
            })
        }))
    }
}
pub(super) struct Refusal {
    id: TransferId,
    state: watch::Sender<Option<Failure>>,
    pending: Weak<Refusals>,
}
impl Drop for Refusal {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.upgrade()
            && let Ok(mut pending) = pending.0.lock()
        {
            pending.remove(&self.id);
        }
    }
}
impl Refusal {
    pub fn current(&self) -> Option<Failure> {
        *self.state.borrow()
    }
    pub async fn wait(&self) -> Failure {
        let mut changes = self.state.subscribe();
        loop {
            let code = *changes.borrow_and_update();
            if let Some(code) = code {
                return code;
            }
            if changes.changed().await.is_err() {
                return Failure::Canceled;
            }
        }
    }
    pub fn reader(
        self: &Arc<Self>,
        inner: Pin<Box<dyn AsyncRead + Send>>,
    ) -> Pin<Box<dyn AsyncRead + Send>> {
        Box::pin(Reader {
            inner,
            refusal: self.clone(),
        })
    }
}
struct Reader {
    inner: Pin<Box<dyn AsyncRead + Send>>,
    refusal: Arc<Refusal>,
}
impl AsyncRead for Reader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(code) = this.refusal.current() {
            return Poll::Ready(Err(io::Error::other(Error::PeerRefused(code))));
        }
        this.inner.as_mut().poll_read(cx, buffer)
    }
}
