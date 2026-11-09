//! The text wire observes one native provider. Control-select cancellation
//! must not discard an admitted native read or repeat the same OS operation.
use super::{Error, Provider, Publication, Snapshot, Text, Value};
use crate::{
    DesktopError,
    clipboard::{ClipboardBackend, ClipboardFuture, NativeLifetime, ownership::Owner},
};
use rds_core::clipboard::{Format, Limits};
use std::{future::Future, pin::Pin, sync::Arc};

type Owned<T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send>>;
enum Observation {
    Snapshot {
        initial: bool,
        future: Owned<Snapshot>,
    },
    Read {
        generation: u64,
        future: Owned<Value>,
    },
}
enum Finished {
    Snapshot(bool, Result<Snapshot, Error>),
    Read(u64, Result<Value, Error>),
}
pub(crate) struct Legacy {
    observation: Option<Observation>,
    provider: Arc<dyn Provider>,
    owner: Arc<Owner>,
    limits: Limits,
    seen: Option<u64>,
    watching: bool,
}
impl Legacy {
    pub(crate) fn new(
        provider: Arc<dyn Provider>,
        owner: Arc<Owner>,
        watching: bool,
        seen: Option<u64>,
    ) -> Self {
        let limits = provider.capabilities().limits;
        Self {
            observation: None,
            provider,
            owner,
            limits,
            seen,
            watching,
        }
    }
    fn observe(&mut self) {
        let provider = self.provider.clone();
        let seen = self.seen;
        self.observation = Some(Observation::Snapshot {
            initial: seen.is_none(),
            future: Box::pin(async move {
                match seen {
                    Some(seen) => provider.changed(seen).await,
                    None => provider.current().await,
                }
            }),
        });
    }
}
impl Drop for Legacy {
    fn drop(&mut self) {
        self.owner.close();
        self.observation.take();
    }
}
fn desktop(error: Error) -> DesktopError {
    DesktopError::Input(error.to_string())
}
impl ClipboardBackend for Legacy {
    fn retain(&mut self, original: NativeLifetime) -> bool {
        self.owner.retain(original)
    }
    fn close(&mut self) -> ClipboardFuture<'_, ()> {
        self.owner.close();
        // Drop only our waiter. Any actual native job retains its own running
        // guard and original purpose, which Provider::close must still drain.
        self.observation.take();
        self.provider.close()
    }
    fn publish(&mut self, text: String) -> ClipboardFuture<'_, Result<(), DesktopError>> {
        Box::pin(async move {
            // A new explicit publication supersedes an undelivered observation;
            // its actual native job still retains its original through drain.
            self.observation.take();
            let snapshot = self.provider.current().await.map_err(desktop)?;
            let text = Text::new(bytes::Bytes::from(text), &self.limits).map_err(desktop)?;
            let original: NativeLifetime =
                Arc::new(self.owner.begin().ok_or_else(|| desktop(Error::Changed))?);
            let published = self
                .provider
                .publish(snapshot.generation, Publication::Text(text), original)
                .await
                .map_err(desktop)?;
            self.seen = Some(published.generation);
            Ok(())
        })
    }
    fn next_change(&mut self) -> ClipboardFuture<'_, Option<String>> {
        Box::pin(async move {
            if !self.watching {
                self.owner.closed_signal().await;
                return None;
            }
            loop {
                if self.observation.is_none() {
                    self.observe();
                }
                let finished = match self.observation.as_mut()? {
                    Observation::Snapshot { initial, future } => {
                        Finished::Snapshot(*initial, future.await)
                    }
                    Observation::Read { generation, future } => {
                        Finished::Read(*generation, future.await)
                    }
                };
                self.observation = None;
                match finished {
                    Finished::Snapshot(initial, result) => {
                        let snapshot = result.ok()?;
                        if initial
                            || snapshot.owned
                            || !snapshot
                                .representations
                                .iter()
                                .any(|r| r.format == Format::TextUtf8)
                        {
                            self.seen = Some(snapshot.generation);
                            continue;
                        }
                        let generation = snapshot.generation;
                        let provider = self.provider.clone();
                        let owner = self.owner.clone();
                        self.observation = Some(Observation::Read {
                            generation,
                            future: Box::pin(async move {
                                let original: NativeLifetime =
                                    Arc::new(owner.begin().ok_or(Error::Changed)?);
                                provider.read(generation, Format::TextUtf8, original).await
                            }),
                        });
                    }
                    Finished::Read(generation, result) => {
                        self.seen = Some(generation);
                        match result {
                            Ok(Value::Text(text)) => return Some(text.as_str().to_owned()),
                            Err(Error::Unavailable) => return None,
                            _ => {}
                        }
                    }
                }
            }
        })
    }
}
