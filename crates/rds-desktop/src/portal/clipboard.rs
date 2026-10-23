//! The consented portal session owns text selection and bounded nonblocking FD I/O.
use crate::{
    DesktopError,
    clipboard::{ClipboardBackend, ClipboardFuture, MAX_TEXT_BYTES},
};
use ashpd::desktop::{
    Session,
    clipboard::{Clipboard, SelectionOwnerChanged, SetSelectionOptions},
    remote_desktop::RemoteDesktop,
};
use ashpd::zbus::zvariant::OwnedObjectPath;
use futures_util::StreamExt;
use std::{
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::unix::AsyncFd,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
};

const TIMEOUT: Duration = Duration::from_secs(2);
const MIME: &str = "text/plain;charset=utf-8";

fn error() -> DesktopError {
    DesktopError::Input("Wayland clipboard unavailable or transfer refused".into())
}
fn mime(types: &[String]) -> Option<&str> {
    if types.len() > 64 || types.iter().any(|s| s.len() > 256) {
        return None;
    }
    [MIME, "text/plain", "UTF8_STRING"]
        .into_iter()
        .find(|wanted| types.iter().any(|s| s == wanted))
}
async fn bounded<T, E>(future: impl Future<Output = Result<T, E>>) -> Result<T, DesktopError> {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .map_err(|_| error())?
        .map_err(|_| error())
}
fn nonblocking(fd: OwnedFd) -> Result<AsyncFd<OwnedFd>, DesktopError> {
    let flags = rustix::fs::fcntl_getfl(&fd).map_err(|_| error())?;
    rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK).map_err(|_| error())?;
    // SAFETY: the OwnedFd keeps this valid descriptor open and unchanged for
    // AsyncFd's lifetime; transfers only borrow it through get_ref().
    #[allow(unsafe_code)]
    unsafe { AsyncFd::register(fd) }.map_err(|_| error())
}
async fn read_text(fd: OwnedFd) -> Result<String, DesktopError> {
    let fd = nonblocking(fd)?;
    let mut bytes = Vec::new();
    let reading = async {
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let mut ready = fd.readable().await.map_err(|_| error())?;
            match ready.try_io(|fd| {
                rustix::io::read(fd.get_ref(), &mut buffer).map_err(std::io::Error::from)
            }) {
                Ok(Ok(0)) => return String::from_utf8(bytes).map_err(|_| error()),
                Ok(Ok(n)) => {
                    if bytes.len() + n > MAX_TEXT_BYTES {
                        return Err(error());
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(Err(_)) => return Err(error()),
                Err(_) => {}
            }
        }
    };
    tokio::time::timeout(TIMEOUT, reading)
        .await
        .map_err(|_| error())?
}
async fn write_text(fd: OwnedFd, text: &str) -> Result<(), DesktopError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(error());
    }
    let fd = nonblocking(fd)?;
    let writing = async {
        let mut bytes = text.as_bytes();
        while !bytes.is_empty() {
            let mut ready = fd.writable().await.map_err(|_| error())?;
            match ready
                .try_io(|fd| rustix::io::write(fd.get_ref(), bytes).map_err(std::io::Error::from))
            {
                Ok(Ok(0)) => return Err(error()),
                Ok(Ok(n)) => bytes = &bytes[n..],
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(Err(_)) => return Err(error()),
                Err(_) => {}
            }
        }
        Ok(())
    };
    tokio::time::timeout(TIMEOUT, writing)
        .await
        .map_err(|_| error())?
}

#[derive(Clone, Default)]
struct Snapshot {
    text: Option<Arc<str>>,
    generation: u64,
    closed: bool,
}
fn invalidate_selection(changes: &watch::Sender<Snapshot>, generation: &mut u64) {
    *generation = generation.wrapping_add(1);
    // A lease which has not consumed the previous value must not receive it
    // after ownership moves, including after our own remote paste.
    changes.send_replace(Snapshot {
        generation: *generation,
        ..Default::default()
    });
}
struct Job {
    text: String,
    reply: oneshot::Sender<Result<(), DesktopError>>,
}
pub(super) struct Group {
    jobs: mpsc::Sender<Job>,
    changes: watch::Receiver<Snapshot>,
    leases: Arc<Semaphore>,
    live: Arc<AtomicBool>,
    watchers: Arc<AtomicUsize>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    finished: watch::Receiver<bool>,
}
impl Group {
    pub(super) async fn start(
        proxy: Clipboard,
        session: Arc<Session<RemoteDesktop>>,
    ) -> Result<Self, DesktopError> {
        let proxy = Arc::new(proxy);
        // Session's public wire serialization is its object path. Avoid
        // private ashpd APIs and never format session/token data for logs.
        let value = serde_json::to_value(session.as_ref()).map_err(|_| error())?;
        let session_path = value.as_str().ok_or_else(error)?.to_owned();
        // Subscribe before returning readiness; no backend/native object is
        // opened later by a remote clipboard request.
        let owners = bounded(proxy.receive_signal("SelectionOwnerChanged")).await?;
        let transfers = bounded(proxy.receive_signal("SelectionTransfer")).await?;
        let (jobs, mut requests) = mpsc::channel::<Job>(4);
        let (changes, receiver) = watch::channel(Snapshot::default());
        let (stop, stopped) = oneshot::channel();
        let (finished, joined) = watch::channel(false);
        let live = Arc::new(AtomicBool::new(true));
        let state = live.clone();
        let watchers = Arc::new(AtomicUsize::new(0));
        let watching = watchers.clone();
        tokio::spawn(async move {
            struct Seal {
                live: Arc<AtomicBool>,
                changes: watch::Sender<Snapshot>,
                finished: watch::Sender<bool>,
            }
            impl Drop for Seal {
                fn drop(&mut self) {
                    self.live.store(false, Ordering::Release);
                    self.changes.send_replace(Snapshot {
                        closed: true,
                        ..Default::default()
                    });
                    self.finished.send_replace(true);
                }
            }
            let _seal = Seal {
                live: state.clone(),
                changes: changes.clone(),
                finished,
            };
            tokio::pin!(stopped, owners, transfers);
            let mut pending = JoinSet::new();
            let mut offered: Option<Arc<str>> = None;
            let mut generation = 0u64;
            let mut reading = false;
            let mut wanted: Option<(u64, String)> = None;
            let mut writing = 0usize;
            let mut cleanup = tokio::time::interval(Duration::from_millis(250));
            // Signals describe selection ownership, not application text.
            // Never sample the clipboard on subscribe or log its contents.
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    message = transfers.next() => {
                        let Some(message) = message else { break; };
                        let Ok((path, requested, serial)) = message.body().deserialize::<(OwnedObjectPath, String, u32)>() else { continue; };
                        if path.as_str() != session_path { continue; }
                        let text = offered.clone().filter(|_| matches!(requested.as_str(), MIME | "text/plain" | "UTF8_STRING"));
                        if writing >= 4 || text.is_none() {
                            let _ = bounded(proxy.selection_write_done(&session, serial, false)).await;
                            continue;
                        }
                        writing += 1;
                        let proxy = proxy.clone(); let session = session.clone();
                        pending.spawn(async move {
                            let outcome = async {
                                let fd: OwnedFd = bounded(proxy.selection_write(&session, serial)).await?.into();
                                write_text(fd, &text.ok_or_else(error)?).await
                            }.await;
                            let _ = bounded(proxy.selection_write_done(&session, serial, outcome.is_ok())).await;
                            (generation, None, false)
                        });
                    },
                    message = owners.next() => {
                        let Some(message) = message else { break; };
                        let Ok((path, selection)) = message.body().deserialize::<(OwnedObjectPath, SelectionOwnerChanged)>() else { continue; };
                        if path.as_str() != session_path { continue; }
                        invalidate_selection(&changes, &mut generation);
                        wanted = None;
                        if selection.session_is_owner() == Some(true) { continue; }
                        // An external owner invalidates our advertised bytes.
                        offered = None;
                        if selection.session_is_owner() != Some(false) || watching.load(Ordering::Acquire) == 0 { continue; }
                        let Some(format) = mime(selection.mime_types()) else { continue; };
                        wanted = Some((generation, format.to_owned()));
                    },
                    result = pending.join_next(), if !pending.is_empty() => {
                        match result {
                            Some(Ok((seen, text, true))) => {
                                reading = false;
                                if seen == generation && watching.load(Ordering::Acquire) > 0 && let Some(text) = text {
                                    changes.send_replace(Snapshot { text: Some(text), generation, closed: false });
                                }
                            },
                            Some(Ok((_, _, false))) => writing = writing.saturating_sub(1),
                            Some(Err(_)) => break,
                            None => {},
                        }
                    },
                    _ = tokio::task::yield_now(), if wanted.is_some() && !reading => {
                        let Some((seen, format)) = wanted.take() else { continue; };
                        if watching.load(Ordering::Acquire) == 0 { continue; }
                        reading = true;
                        let proxy = proxy.clone(); let session = session.clone();
                        pending.spawn(async move {
                            let text = async {
                                let fd: OwnedFd = bounded(proxy.selection_read(&session, &format)).await?.into();
                                read_text(fd).await
                            }.await.ok().map(Arc::<str>::from);
                            (seen, text, true)
                        });
                    },
                    _ = cleanup.tick() => {
                        if watching.load(Ordering::Acquire) == 0 {
                            wanted = None;
                            if changes.borrow().text.is_some() {
                                changes.send_replace(Snapshot { generation, ..Default::default() });
                            }
                        }
                    },
                    job = requests.recv() => {
                        let Some(job) = job else { break; };
                        if job.reply.is_closed() { continue; }
                        if job.text.len() > MAX_TEXT_BYTES { let _ = job.reply.send(Err(error())); continue; }
                        let previous = offered.take();
                        offered = Some(Arc::from(job.text));
                        let result = bounded(proxy.set_selection(&session, SetSelectionOptions::default().set_mime_types(&[MIME, "text/plain"])) ).await;
                        if result.is_err() { offered = previous; }
                        let _ = job.reply.send(result);
                    },
                }
            }
            state.store(false, Ordering::Release);
            changes.send_replace(Snapshot {
                closed: true,
                ..Default::default()
            });
            pending.abort_all();
            while pending.join_next().await.is_some() {}
        });
        Ok(Self {
            jobs,
            changes: receiver,
            leases: Arc::new(Semaphore::new(8)),
            live,
            watchers,
            stop: Mutex::new(Some(stop)),
            finished: joined,
        })
    }
    pub(super) fn available(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
    pub(super) fn worker(&self, watching: bool) -> Result<Box<dyn ClipboardBackend>, DesktopError> {
        if !self.available() {
            return Err(error());
        }
        let permit = self
            .leases
            .clone()
            .try_acquire_owned()
            .map_err(|_| error())?;
        let mut changes = self.changes.clone();
        let seen = changes.borrow_and_update().generation;
        if watching {
            self.watchers.fetch_add(1, Ordering::AcqRel);
        }
        Ok(Box::new(Lease {
            jobs: self.jobs.clone(),
            changes,
            watching,
            seen,
            watchers: self.watchers.clone(),
            _permit: permit,
        }))
    }
    pub(super) async fn close(&self) {
        self.request_close();
        let mut finished = self.finished.clone();
        while !*finished.borrow_and_update() {
            if finished.changed().await.is_err() {
                break;
            }
        }
    }
    fn request_close(&self) {
        if let Ok(mut stop) = self.stop.lock()
            && let Some(stop) = stop.take()
        {
            let _ = stop.send(());
        }
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        self.request_close();
    }
}
struct Lease {
    jobs: mpsc::Sender<Job>,
    changes: watch::Receiver<Snapshot>,
    watching: bool,
    seen: u64,
    watchers: Arc<AtomicUsize>,
    _permit: OwnedSemaphorePermit,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if self.watching {
            self.watchers.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
impl ClipboardBackend for Lease {
    fn publish(&mut self, text: String) -> ClipboardFuture<'_, Result<(), DesktopError>> {
        Box::pin(async move {
            if self.changes.borrow().closed || text.len() > MAX_TEXT_BYTES {
                return Err(error());
            }
            let (reply, outcome) = oneshot::channel();
            self.jobs
                .try_send(Job { text, reply })
                .map_err(|_| error())?;
            tokio::time::timeout(TIMEOUT, outcome)
                .await
                .map_err(|_| error())?
                .map_err(|_| error())?
        })
    }
    fn next_change(&mut self) -> ClipboardFuture<'_, Option<String>> {
        Box::pin(async move {
            if !self.watching {
                return std::future::pending().await;
            }
            loop {
                self.changes.changed().await.ok()?;
                let snapshot = self.changes.borrow_and_update().clone();
                if snapshot.closed {
                    return None;
                }
                if snapshot.generation != self.seen
                    && let Some(text) = snapshot.text
                {
                    self.seen = snapshot.generation;
                    return Some(text.to_string());
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn ownership_change_discards_unread_text_but_delivers_its_later_read() {
        let (jobs, _) = mpsc::channel(4);
        let (changes, receiver) = watch::channel(Snapshot::default());
        let (finished, joined) = watch::channel(false);
        let group = Group {
            jobs,
            changes: receiver,
            leases: Arc::new(Semaphore::new(8)),
            live: Arc::new(AtomicBool::new(true)),
            watchers: Arc::new(AtomicUsize::new(0)),
            stop: Mutex::new(None),
            finished: joined,
        };
        let mut lease = group.worker(true).unwrap();
        changes.send_replace(Snapshot {
            text: Some(Arc::from("old")),
            generation: 1,
            closed: false,
        });
        let mut generation = 1;
        invalidate_selection(&changes, &mut generation);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), lease.next_change())
                .await
                .is_err()
        );
        changes.send_replace(Snapshot {
            text: Some(Arc::from("new")),
            generation,
            closed: false,
        });
        assert_eq!(lease.next_change().await.as_deref(), Some("new"));
        drop(finished);
    }
    #[tokio::test]
    async fn pipe_utf8_is_bounded_and_no_blocking_task_survives_timeout() {
        let (read, write) = std::os::unix::net::UnixStream::pair().unwrap();
        let value = "Привет\nclipboard ✓";
        let (text, result) = tokio::join!(read_text(read.into()), write_text(write.into(), value));
        assert!(result.is_ok());
        assert_eq!(text.unwrap(), value);
        let (read, write) = std::os::unix::net::UnixStream::pair().unwrap();
        let stalled = tokio::time::timeout(Duration::from_millis(20), read_text(read.into())).await;
        assert!(stalled.is_err());
        assert_eq!(
            rustix::io::write(&write, b"x"),
            Err(rustix::io::Errno::PIPE)
        );
    }
    #[test]
    fn only_bounded_utf8_text_formats_are_selected() {
        assert_eq!(mime(&["image/png".into(), MIME.into()]), Some(MIME));
        assert_eq!(mime(&["application/vnd.portal.filetransfer".into()]), None);
        assert_eq!(mime(&vec![MIME.into(); 65]), None);
    }

    #[tokio::test]
    async fn invalid_utf8_and_oversized_selections_are_refused() {
        let (read, write) = std::os::unix::net::UnixStream::pair().unwrap();
        rustix::io::write(&write, &[0xff]).unwrap();
        drop(write);
        assert!(read_text(read.into()).await.is_err());

        let (read, write) = std::os::unix::net::UnixStream::pair().unwrap();
        let oversized = "x".repeat(MAX_TEXT_BYTES + 1);
        let (result, _) = tokio::join!(read_text(read.into()), async {
            let fd = nonblocking(write.into()).unwrap();
            let mut bytes = oversized.as_bytes();
            while !bytes.is_empty() {
                let mut ready = fd.writable().await.unwrap();
                match ready.try_io(|fd| {
                    rustix::io::write(fd.get_ref(), bytes).map_err(std::io::Error::from)
                }) {
                    Ok(Ok(n)) if n > 0 => bytes = &bytes[n..],
                    Ok(_) => break,
                    Err(_) => {}
                }
            }
        });
        assert!(result.is_err());
        let (_, write) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(write_text(write.into(), &oversized).await.is_err());
    }
}
