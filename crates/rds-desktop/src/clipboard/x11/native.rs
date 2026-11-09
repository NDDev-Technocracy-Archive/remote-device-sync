//! Event-driven X11 text/PNG provider over the existing ICCCM selection edge.
//! A prepared physical owner is distinct from each admitted clipboard lease.
use super::*;
use crate::clipboard::{
    ClipboardFuture, NativeLifetime,
    native::{Error, Operation, Policy, Provider, Publication, Snapshot, Text, Value},
    ownership::{Owner, Running},
};
use rds_core::clipboard::{Capabilities, Format, Representation, ShortcutModifier};
use std::sync::{Mutex, Weak, atomic::AtomicU8, mpsc as queue};
use tokio::sync::{OwnedSemaphorePermit, watch};

const STARTING: u8 = 0;
const READY: u8 = 1;
const ENDED: u8 = 2;
const CLOSING: u8 = 3;
static PHYSICAL: Semaphore = Semaphore::const_new(1);
static POOL: OnceLock<Mutex<Weak<Core>>> = OnceLock::new();

struct Core {
    jobs: queue::SyncSender<Job>,
    wake: Mutex<UnixStream>,
    generations: watch::Receiver<u64>,
    state: Arc<AtomicU8>,
    bodies: Arc<Semaphore>,
    policy: Policy,
    original: NativeLifetime,
}
struct Job {
    command: Command,
    _original: Option<NativeLifetime>,
    _body: Option<OwnedSemaphorePermit>,
    _core: Arc<Core>,
    // Acknowledge drain only after the original purpose and body credit drop.
    running: Running,
}
enum Command {
    Current(oneshot::Sender<Result<Snapshot, Error>>),
    Read {
        generation: u64,
        format: Format,
        reply: oneshot::Sender<Result<Value, Error>>,
    },
    Publish {
        generation: u64,
        value: Publication,
        reply: oneshot::Sender<Result<Snapshot, Error>>,
    },
}

/// One prepared X-server clipboard edge. Clones share its physical owner;
/// creating a lease performs no native I/O and grants no additional OS access.
#[derive(Clone)]
pub struct Prepared(Arc<Core>);
impl Prepared {
    pub fn provider(&self, original: NativeLifetime) -> Result<Arc<dyn Provider>, Error> {
        if self.0.state.load(Ordering::Acquire) != READY {
            return Err(Error::Unavailable);
        }
        let owner = Owner::new();
        if !owner.retain(original) {
            return Err(Error::Changed);
        }
        Ok(Arc::new(Lease {
            prepared: self.clone(),
            owner,
        }))
    }
    /// Seal the physical edge and drain its real native work. Closing one
    /// session uses `Provider::close` instead; this ends all of this edge.
    pub fn close(&self) -> ClipboardFuture<'_, ()> {
        let _ = self
            .0
            .state
            .compare_exchange(READY, CLOSING, Ordering::AcqRel, Ordering::Acquire);
        self.0.bodies.close();
        if let Ok(mut wake) = self.0.wake.lock() {
            let _ = wake.write(&[1]);
        }
        Box::pin(async move {
            let mut changed = self.0.generations.clone();
            while self.0.state.load(Ordering::Acquire) != ENDED {
                if changed.changed().await.is_err() {
                    break;
                }
            }
        })
    }
}

pub(crate) async fn prepare(policy: Policy, original: NativeLifetime) -> Result<Prepared, Error> {
    policy.validate()?;
    let prepared = {
        let mut pool = POOL
            .get_or_init(|| Mutex::new(Weak::new()))
            .lock()
            .map_err(|_| Error::Unavailable)?;
        if let Some(core) = pool.upgrade() {
            if core.policy != policy || !Arc::ptr_eq(&core.original, &original) {
                return Err(Error::Settings);
            }
            Prepared(core)
        } else {
            let permit = PHYSICAL.try_acquire().map_err(|_| Error::Unavailable)?;
            let (wake, reader) = UnixStream::pair()?;
            wake.set_nonblocking(true)?;
            reader.set_nonblocking(true)?;
            let (jobs, receiver) = queue::sync_channel(policy.pending_operations);
            let (generations, changes) = watch::channel(0);
            let state = Arc::new(AtomicU8::new(STARTING));
            let core = Arc::new(Core {
                jobs,
                wake: Mutex::new(wake),
                generations: changes,
                state: state.clone(),
                bodies: Arc::new(Semaphore::new(policy.native_body_operations)),
                policy: policy.clone(),
                original: original.clone(),
            });
            let settings = policy.clone();
            tokio::runtime::Handle::try_current()
                .map_err(|_| Error::Unavailable)?
                .spawn_blocking(move || {
                    // Physical ownership outlives board/window/descriptor teardown,
                    // including cancellation while native initialization is running.
                    let _physical = original;
                    struct Exit {
                        state: Arc<AtomicU8>,
                        generations: watch::Sender<u64>,
                        permit: Option<tokio::sync::SemaphorePermit<'static>>,
                    }
                    impl Drop for Exit {
                        fn drop(&mut self) {
                            // Board and queued jobs have drained before this guard.
                            // Release physical admission before advertising ENDED.
                            self.permit.take();
                            self.state.store(ENDED, Ordering::Release);
                            self.generations.send_replace(0);
                        }
                    }
                    let exit = Exit {
                        state: state.clone(),
                        generations,
                        permit: Some(permit),
                    };
                    let result = serve(receiver, reader, &settings, &state, &exit.generations);
                    if let Err(error) = result {
                        tracing::debug!(%error, "native X11 clipboard owner ended");
                    }
                });
            // Do not retain a writer outside Core: last-owner drop must wake
            // the native poll with HUP even when no clipboard event arrives.
            *pool = Arc::downgrade(&core);
            Prepared(core)
        }
    };
    let mut changes = prepared.0.generations.clone();
    tokio::time::timeout(Duration::from_millis(policy.operation_timeout_ms), async {
        loop {
            match prepared.0.state.load(Ordering::Acquire) {
                READY => return Ok(()),
                ENDED => return Err(Error::Unavailable),
                _ => {}
            }
            changes.changed().await.map_err(|_| Error::Unavailable)?;
        }
    })
    .await
    .map_err(|_| Error::Unavailable)??;
    Ok(prepared)
}

fn serve(
    receiver: queue::Receiver<Job>,
    mut wake: UnixStream,
    policy: &Policy,
    state: &AtomicU8,
    generations: &watch::Sender<u64>,
) -> Result<(), Error> {
    let mut board = Board::new(policy)?;
    state.store(READY, Ordering::Release);
    generations.send_replace(board.clipboard.generation);
    loop {
        if state.load(Ordering::Acquire) == CLOSING {
            return Ok(());
        }
        board.refresh(policy)?;
        generations.send_if_modified(|generation| {
            if *generation == board.clipboard.generation {
                false
            } else {
                *generation = board.clipboard.generation;
                true
            }
        });
        loop {
            if state.load(Ordering::Acquire) == CLOSING {
                return Ok(());
            }
            match receiver.try_recv() {
                Ok(job) => {
                    if !job.running.closed() {
                        match job.command {
                            Command::Current(reply) if !reply.is_closed() => {
                                let _ = reply.send(board.snapshot(policy));
                            }
                            Command::Read {
                                generation,
                                format,
                                reply,
                            } if !reply.is_closed() => {
                                let _ = reply.send(board.read(generation, format, policy));
                            }
                            Command::Publish {
                                generation,
                                value,
                                reply,
                            } if !reply.is_closed() => {
                                let _ = reply.send(board.publish(generation, value, policy));
                            }
                            _ => {}
                        }
                    }
                    board.refresh(policy)?;
                    generations.send_if_modified(|generation| {
                        if *generation == board.clipboard.generation {
                            false
                        } else {
                            *generation = board.clipboard.generation;
                            true
                        }
                    });
                }
                Err(queue::TryRecvError::Disconnected) => return Ok(()),
                Err(queue::TryRecvError::Empty) => break,
            }
        }
        let mut fds = [
            rustix::event::PollFd::new(board.clipboard.conn.stream(), rustix::event::PollFlags::IN),
            rustix::event::PollFd::new(&wake, rustix::event::PollFlags::IN),
        ];
        let timeout = board.clipboard.poll_timeout();
        rustix::event::poll(&mut fds, timeout.as_ref()).map_err(std::io::Error::from)?;
        if fds.iter().any(|fd| {
            fd.revents().intersects(
                rustix::event::PollFlags::ERR
                    | rustix::event::PollFlags::HUP
                    | rustix::event::PollFlags::NVAL,
            )
        }) {
            return Ok(());
        }
        let mut bytes = [0; 64];
        while wake.read(&mut bytes).is_ok_and(|n| n > 0) {}
    }
}

fn edge(error: DesktopError) -> Error {
    tracing::debug!(%error, "native X11 clipboard operation failed");
    Error::Unavailable
}
struct Board {
    clipboard: Clipboard,
    png: Atom,
    snapshot: Option<Snapshot>,
}
impl Board {
    fn new(policy: &Policy) -> Result<Self, Error> {
        let mut clipboard = Clipboard::new_watching(0, true, policy).map_err(edge)?;
        if !clipboard.xfixes {
            return Err(Error::Unavailable);
        }
        clipboard.native = true;
        let png = clipboard
            .conn
            .intern_atom(false, b"image/png")
            .map_err(error)
            .and_then(|c| c.reply().map_err(error))
            .map_err(edge)?
            .atom;
        Ok(Self {
            clipboard,
            png,
            snapshot: None,
        })
    }
    fn refresh(&mut self, policy: &Policy) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_millis(policy.operation_timeout_ms);
        while self.clipboard.pump().map_err(edge)? {
            if Instant::now() >= deadline {
                return Err(Error::Unavailable);
            }
        }
        Ok(())
    }
    fn synchronize(&mut self, policy: &Policy) -> Result<(), Error> {
        // The reply fences older XFixes notifications on this connection. A
        // current/read/publication must not trust an event cache that has not
        // yet received another application's completed ownership change.
        let owner = self
            .clipboard
            .conn
            .get_selection_owner(self.clipboard.clipboard)
            .map_err(error)
            .and_then(|cookie| cookie.reply().map_err(error))
            .map_err(edge)?
            .owner;
        self.refresh(policy)?;
        if owner != self.clipboard.last_owner {
            self.clipboard.owner_changed(owner, 0).map_err(edge)?;
        }
        Ok(())
    }
    fn selected(
        &mut self,
        generation: u64,
        target: Atom,
        kind: Atom,
        format: u8,
        limit: usize,
        policy: &Policy,
    ) -> Result<Bytes, Error> {
        let deadline = Instant::now() + Duration::from_millis(policy.operation_timeout_ms);
        let result = (|| {
            self.clipboard
                .start_read(0, target, kind, format, limit, deadline)
                .map_err(edge)?;
            loop {
                self.refresh(policy)?;
                if self.clipboard.generation != generation {
                    break Err(Error::Changed);
                }
                if let Some((received, bytes)) = self.clipboard.external.take() {
                    break if received == target {
                        Ok(Bytes::from(bytes))
                    } else {
                        Err(Error::Changed)
                    };
                }
                if self.clipboard.incoming.is_none() {
                    break Err(Error::Unsupported);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break Err(Error::Unavailable);
                }
                let mut fds = [rustix::event::PollFd::new(
                    self.clipboard.conn.stream(),
                    rustix::event::PollFlags::IN,
                )];
                let timeout = rustix::event::Timespec {
                    tv_sec: remaining.as_secs() as _,
                    tv_nsec: remaining.subsec_nanos() as _,
                };
                if rustix::event::poll(&mut fds, Some(&timeout)).is_err() {
                    break Err(Error::Unavailable);
                }
            }
        })();
        self.clipboard.cancel_read();
        result
    }
    fn snapshot(&mut self, policy: &Policy) -> Result<Snapshot, Error> {
        self.synchronize(policy)?;
        let generation = self.clipboard.generation;
        if let Some(snapshot) = &self.snapshot
            && snapshot.generation == generation
        {
            return Ok(snapshot.clone());
        }
        let owned = self.clipboard.last_owner == self.clipboard.window;
        let mut representations = Vec::new();
        if owned {
            if let Some(format) = self.format(self.clipboard.published_kind) {
                representations.push(Representation {
                    format,
                    bytes: Some(self.clipboard.payload.len() as u64),
                });
            }
        } else if self.clipboard.last_owner != 0 {
            let atoms = self.selected(
                generation,
                self.clipboard.targets,
                AtomEnum::ATOM.into(),
                32,
                policy.native_types * 4,
                policy,
            )?;
            self.synchronize(policy)?;
            if generation != self.clipboard.generation {
                return Err(Error::Changed);
            }
            let (entries, remainder) = atoms.as_chunks::<4>();
            if !remainder.is_empty() {
                return Err(Error::Unsupported);
            }
            for format in [Format::TextUtf8, Format::ImagePng] {
                let target = self.target(format)?;
                if entries.iter().any(|v| u32::from_le_bytes(*v) == target) {
                    representations.push(Representation {
                        format,
                        bytes: None,
                    });
                }
            }
        }
        let snapshot = Snapshot {
            generation,
            owned,
            representations,
        };
        self.snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }
    fn target(&self, format: Format) -> Result<Atom, Error> {
        match format {
            Format::TextUtf8 => Ok(self.clipboard.utf8),
            Format::ImagePng => Ok(self.png),
            Format::Files => Err(Error::Unsupported),
        }
    }
    fn format(&self, atom: Atom) -> Option<Format> {
        if atom == self.clipboard.utf8 {
            Some(Format::TextUtf8)
        } else if atom == self.png {
            Some(Format::ImagePng)
        } else {
            None
        }
    }
    fn read(&mut self, generation: u64, format: Format, policy: &Policy) -> Result<Value, Error> {
        let snapshot = self.snapshot(policy)?;
        if generation != snapshot.generation {
            return Err(Error::Changed);
        }
        if !snapshot.representations.iter().any(|r| r.format == format) {
            return Err(Error::Unsupported);
        }
        let target = self.target(format)?;
        let bytes = if snapshot.owned {
            self.clipboard.payload.clone()
        } else {
            self.selected(
                generation,
                target,
                target,
                8,
                policy.limits.representation_bytes(format) as usize,
                policy,
            )?
        };
        self.synchronize(policy)?;
        if generation != self.clipboard.generation {
            return Err(Error::Changed);
        }
        match format {
            Format::TextUtf8 => Ok(Value::Text(Text::new(bytes, &policy.limits)?)),
            Format::ImagePng => Ok(Value::Image(crate::clipboard::image::Png::new(
                bytes,
                &policy.limits,
            )?)),
            Format::Files => Err(Error::Unsupported),
        }
    }
    fn publish(
        &mut self,
        generation: u64,
        value: Publication,
        policy: &Policy,
    ) -> Result<Snapshot, Error> {
        self.synchronize(policy)?;
        if generation != self.clipboard.generation {
            return Err(Error::Changed);
        }
        let (format, bytes) = match value {
            Publication::Text(text) => (Format::TextUtf8, text.encoded().clone()),
            Publication::Image(image) => (Format::ImagePng, image.encoded().clone()),
            Publication::Files(_) => return Err(Error::Unsupported),
        };
        policy.limits.validate_value(format, &bytes)?;
        if let Err(error) =
            self.clipboard
                .publish_bytes(self.target(format)?, bytes, Some(generation))
        {
            self.refresh(policy)?;
            return Err(if generation != self.clipboard.generation {
                Error::Changed
            } else {
                edge(error)
            });
        }
        let snapshot = self.snapshot(policy)?;
        if !snapshot.owned {
            return Err(Error::Changed);
        }
        Ok(snapshot)
    }
}

struct Lease {
    prepared: Prepared,
    owner: Arc<Owner>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.owner.close();
    }
}
impl Lease {
    async fn call<T: Send>(
        &self,
        original: Option<NativeLifetime>,
        command: impl FnOnce(oneshot::Sender<Result<T, Error>>) -> Command + Send,
    ) -> Result<T, Error> {
        if self.prepared.0.state.load(Ordering::Acquire) != READY {
            return Err(Error::Unavailable);
        }
        let running = self.owner.begin().ok_or(Error::Changed)?;
        let body = if original.is_some() {
            Some(
                self.prepared
                    .0
                    .bodies
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| Error::Unavailable)?,
            )
        } else {
            None
        };
        let (reply, response) = oneshot::channel();
        {
            let mut wake = self
                .prepared
                .0
                .wake
                .lock()
                .map_err(|_| Error::Unavailable)?;
            self.prepared
                .0
                .jobs
                .try_send(Job {
                    command: command(reply),
                    running,
                    _core: self.prepared.0.clone(),
                    _body: body,
                    _original: original,
                })
                .map_err(|_| Error::Unavailable)?;
            if let Err(error) = wake.write(&[1])
                && error.kind() != std::io::ErrorKind::WouldBlock
            {
                return Err(Error::Unavailable);
            }
        }
        let value = tokio::time::timeout(
            Duration::from_millis(self.prepared.0.policy.operation_timeout_ms),
            response,
        )
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(|_| Error::Unavailable)??;
        if self.owner.closed() {
            return Err(Error::Changed);
        }
        Ok(value)
    }
}
impl Provider for Lease {
    fn close(&self) -> ClipboardFuture<'_, ()> {
        self.owner.close();
        Box::pin(self.owner.drain())
    }
    fn capabilities(&self) -> Capabilities {
        let formats = if self.prepared.0.state.load(Ordering::Acquire) == READY {
            vec![Format::TextUtf8, Format::ImagePng]
        } else {
            Vec::new()
        };
        Capabilities {
            receive: formats.clone(),
            offer: formats,
            shortcut: ShortcutModifier::Control,
            limits: self.prepared.0.policy.limits.clone(),
        }
    }
    fn current(&self) -> Operation<'_, Snapshot> {
        Box::pin(self.call(None, Command::Current))
    }
    fn read(
        &self,
        generation: u64,
        format: Format,
        original: NativeLifetime,
    ) -> Operation<'_, Value> {
        Box::pin(self.call(Some(original), move |reply| Command::Read {
            generation,
            format,
            reply,
        }))
    }
    fn publish(
        &self,
        generation: u64,
        value: Publication,
        original: NativeLifetime,
    ) -> Operation<'_, Snapshot> {
        Box::pin(self.call(Some(original), move |reply| Command::Publish {
            generation,
            value,
            reply,
        }))
    }
    fn changed(&self, after: u64) -> Operation<'_, Snapshot> {
        Box::pin(async move {
            let mut generations = self.prepared.0.generations.clone();
            loop {
                if self.owner.closed() {
                    return Err(Error::Changed);
                }
                if self.prepared.0.state.load(Ordering::Acquire) != READY {
                    return Err(Error::Unavailable);
                }
                if *generations.borrow_and_update() != after {
                    return self.current().await;
                }
                tokio::select! { _ = self.owner.closed_signal() => return Err(Error::Changed), result = generations.changed() => result.map_err(|_| Error::Unavailable)?, }
            }
        })
    }
}

#[cfg(test)]
mod tests;
