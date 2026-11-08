//! One bounded input worker per controlling session. A platform syscall already
//! in progress cannot be cancelled, but dropped sessions discard queued work.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use rds_core::InputEvent;
use tokio::sync::{mpsc, oneshot};

use crate::{DesktopError, InputSink};

struct Job {
    event: InputEvent,
    reply: oneshot::Sender<Result<(), DesktopError>>,
}

pub(crate) struct InputWorker {
    jobs: Option<mpsc::Sender<Job>>,
    cancelled: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl InputWorker {
    pub(crate) fn new(
        sink: Option<Box<dyn InputSink>>,
        lifetime: Option<crate::clipboard::NativeLifetime>,
    ) -> Self {
        let (jobs, mut receiver) = mpsc::channel::<Job>(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let stopped = cancelled.clone();
        let parent = tracing::Span::current();
        let task = tokio::task::spawn_blocking(move || {
            let _native_lifetime = lifetime;
            let mut sink = sink;
            let _entered = parent.enter();
            while let Some(job) = receiver.blocking_recv() {
                if stopped.load(Ordering::Acquire) || job.reply.is_closed() {
                    break;
                }
                if sink.is_none() {
                    match super::probe_for_display(job.event.display_id) {
                        Ok(backend) => sink = Some(backend),
                        Err(error) => {
                            let _ = job.reply.send(Err(error));
                            break;
                        }
                    }
                }
                // Recheck after a potentially blocking probe. An in-flight
                // inject call cannot be rolled back or reported as cancelled.
                if stopped.load(Ordering::Acquire) || job.reply.is_closed() {
                    break;
                }
                if let Some(backend) = sink.as_mut() {
                    let result = backend.inject(&job.event);
                    let _ = job.reply.send(result);
                }
            }
        });
        Self {
            jobs: Some(jobs),
            cancelled,
            task,
        }
    }

    pub(crate) async fn inject(&mut self, event: InputEvent) -> Result<(), DesktopError> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .as_ref()
            .ok_or_else(unavailable)?
            .send(Job { event, reply })
            .await
            .map_err(|_| unavailable())?;
        result.await.map_err(|_| unavailable())?
    }
}

impl Drop for InputWorker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.jobs.take();
        // Aborts a worker still waiting for a blocking thread. A running one
        // observes cancellation/channel closure after its current syscall.
        self.task.abort();
    }
}

fn unavailable() -> DesktopError {
    DesktopError::Input("input worker unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct HeldSink {
        entered: Option<oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        stopped: Option<oneshot::Sender<()>>,
        calls: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }
    impl InputSink for HeldSink {
        fn inject(&mut self, _: &InputEvent) -> Result<(), DesktopError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
                // Bounded even if the assertion owner panics.
                let _ = self.release.recv_timeout(std::time::Duration::from_secs(3));
            }
            Ok(())
        }
    }
    impl Drop for HeldSink {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
            if let Some(stopped) = self.stopped.take() {
                let _ = stopped.send(());
            }
        }
    }

    struct OriginalContext {
        sink_dropped: Arc<AtomicBool>,
        released: Option<oneshot::Sender<()>>,
    }
    impl Drop for OriginalContext {
        fn drop(&mut self) {
            assert!(
                self.sink_dropped.load(Ordering::Acquire),
                "caller context released before native cleanup"
            );
            if let Some(released) = self.released.take() {
                let _ = released.send(());
            }
        }
    }

    #[tokio::test]
    async fn cancellation_discards_bounded_pending_input_and_releases_the_backend() {
        let (entered, ready) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (stopped, finished) = oneshot::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let (released_context, context_done) = oneshot::channel();
        let worker = InputWorker::new(
            Some(Box::new(HeldSink {
                entered: Some(entered),
                release: released,
                stopped: Some(stopped),
                calls: calls.clone(),
                dropped: dropped.clone(),
            })),
            Some(Arc::new(OriginalContext {
                sink_dropped: dropped,
                released: Some(released_context),
            })),
        );
        let event = InputEvent {
            seq: 1,
            event_ts_ms: 0,
            display_id: 0,
            kind: rds_core::InputKind::PointerMotion { dx: 1.0, dy: 0.0 },
        };
        let (first, first_reply) = oneshot::channel();
        worker
            .jobs
            .as_ref()
            .unwrap()
            .send(Job {
                event: event.clone(),
                reply: first,
            })
            .await
            .unwrap();
        // A blocking injection does not block this single-thread Tokio runtime.
        tokio::time::timeout(std::time::Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        let (pending, pending_reply) = oneshot::channel();
        worker
            .jobs
            .as_ref()
            .unwrap()
            .try_send(Job {
                event: event.clone(),
                reply: pending,
            })
            .unwrap();
        let (overflow, _) = oneshot::channel();
        assert!(matches!(
            worker.jobs.as_ref().unwrap().try_send(Job {
                event,
                reply: overflow
            }),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        drop(worker);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), finished)
            .await
            .unwrap()
            .unwrap();
        assert!(
            first_reply.await.unwrap().is_ok(),
            "an in-flight call may complete"
        );
        assert!(
            pending_reply.await.is_err(),
            "queued input must not execute"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::timeout(std::time::Duration::from_secs(2), context_done)
            .await
            .unwrap()
            .unwrap();
    }
}
