//! X11 CLIPBOARD owner using ICCCM TARGETS/TIMESTAMP and INCR transfers.
//! No external clipboard helper. One worker/window per controlling session.
use super::ClipboardChange;
use crate::DesktopError;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, mpsc, oneshot};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{Event, xproto::*},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};
// Match the native launcher's bounded eight-window product limit. Running
// native calls retain the permit even when their async owner is cancelled.
static WORKERS: Semaphore = Semaphore::const_new(8);
struct Job {
    text: String,
    reply: oneshot::Sender<Result<(), DesktopError>>,
}
pub(crate) struct Worker {
    jobs: mpsc::Sender<Job>,
    changes: crate::mailbox::Receiver<ClipboardChange>,
    stopped: Arc<AtomicBool>,
    wake: Option<UnixStream>,
    initialized: Option<oneshot::Receiver<bool>>,
    task: tokio::task::JoinHandle<()>,
    joined: bool,
    lifetime: Arc<OnceLock<super::NativeLifetime>>,
}
impl Worker {
    pub(crate) fn new(display: u32) -> Self {
        Self::spawn(display, false)
    }
    pub(crate) fn watch(display: u32) -> Self {
        Self::spawn(display, true)
    }
    fn spawn(display: u32, watch: bool) -> Self {
        let lifetime = Arc::new(OnceLock::new());
        let retained = lifetime.clone();
        let (jobs, mut rx) = mpsc::channel::<Job>(1);
        let (changes, change_rx) = crate::mailbox::channel(1);
        let (initialized, ready) = oneshot::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let pair = UnixStream::pair()
            .and_then(|(a, b)| {
                a.set_nonblocking(true)?;
                b.set_nonblocking(true)?;
                Ok((a, b))
            })
            .ok();
        let (wake, receiver) = pair.map_or((None, None), |(a, b)| (Some(a), Some(b)));
        let task = tokio::task::spawn_blocking(move || {
            let _retained = retained;
            let Ok(_permit) = WORKERS.try_acquire() else {
                return;
            };
            let Some(mut wake) = receiver else {
                return;
            };
            let mut clipboard = match Clipboard::new_watching(display, watch) {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(%e, "clipboard worker unavailable");
                    return;
                }
            };
            let _ = initialized.send(true);
            while !stop.load(Ordering::Acquire) {
                match rx.try_recv() {
                    Ok(job) if !job.reply.is_closed() => {
                        let result = clipboard.publish(job.text);
                        let _ = job.reply.send(result);
                    }
                    Ok(_) => {}
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                    Err(mpsc::error::TryRecvError::Empty) => {}
                }
                if let Err(error) = clipboard.pump() {
                    tracing::warn!(%error, "clipboard selection worker ended");
                    break;
                }
                if let Some(text) = clipboard.external.take() {
                    changes.send(ClipboardChange { text });
                }
                let mut fds = [
                    rustix::event::PollFd::new(
                        clipboard.conn.stream(),
                        rustix::event::PollFlags::IN,
                    ),
                    rustix::event::PollFd::new(&wake, rustix::event::PollFlags::IN),
                ];
                // XFixes and jobs wake us immediately. The timer only expires
                // INCR state and supports the 4 Hz legacy owner fallback.
                let timeout = rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 250_000_000,
                };
                if rustix::event::poll(&mut fds, Some(&timeout)).is_err() {
                    break;
                }
                if fds.iter().any(|fd| {
                    fd.revents().intersects(
                        rustix::event::PollFlags::ERR
                            | rustix::event::PollFlags::HUP
                            | rustix::event::PollFlags::NVAL,
                    )
                }) {
                    break;
                }
                let mut bytes = [0; 64];
                while wake.read(&mut bytes).is_ok_and(|n| n > 0) {}
            }
        });
        Self {
            jobs,
            changes: change_rx,
            stopped,
            wake,
            initialized: Some(ready),
            task,
            joined: false,
            lifetime,
        }
    }
    pub(crate) async fn ready(&mut self) -> Result<(), DesktopError> {
        if let Some(ready) = self.initialized.take() {
            match tokio::time::timeout(Duration::from_secs(2), ready).await {
                Ok(Ok(true)) => {}
                _ => return Err(error("clipboard initialization unavailable")),
            }
        }
        Ok(())
    }
    pub(crate) async fn publish(&mut self, text: String) -> Result<(), DesktopError> {
        self.ready().await?;
        let (reply, rx) = oneshot::channel();
        self.jobs
            .send(Job { text, reply })
            .await
            .map_err(|_| error("clipboard worker unavailable"))?;
        if let Some(wake) = &mut self.wake {
            let _ = wake.write(&[1]);
        }
        rx.await
            .map_err(|_| error("clipboard worker unavailable"))?
    }
    pub(crate) async fn next_change(&mut self) -> Option<ClipboardChange> {
        self.changes.recv().await
    }
    pub(crate) fn retain(&mut self, context: super::NativeLifetime) -> bool {
        match self.lifetime.set(context) {
            Ok(()) => true,
            Err(context) => self
                .lifetime
                .get()
                .is_some_and(|old| Arc::ptr_eq(old, &context)),
        }
    }
    pub(crate) async fn close(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(wake) = &mut self.wake {
            let _ = wake.write(&[1]);
        }
        if !self.joined {
            let _ = (&mut self.task).await;
            self.joined = true;
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(wake) = &mut self.wake {
            let _ = wake.write(&[1]);
        }
        self.task.abort();
    }
}
fn error(e: impl std::fmt::Display) -> DesktopError {
    DesktopError::Input(format!("X11 clipboard: {e}"))
}
struct Transfer {
    window: Window,
    property: Atom,
    kind: Atom,
    bytes: Arc<[u8]>,
    offset: usize,
    started: Instant,
}
struct Clipboard {
    conn: RustConnection,
    window: Window,
    root: Window,
    clipboard: Atom,
    utf8: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    marker: Atom,
    property: Atom,
    time: u32,
    last_owner: Window,
    watching: bool,
    xfixes: bool,
    fallback_at: Instant,
    incoming: Option<Incoming>,
    external: Option<String>,
    text: Arc<[u8]>,
    transfers: Vec<Transfer>,
    chunk: usize,
}
struct Incoming {
    window: Window,
    timestamp: u32,
    started: Instant,
    increment: bool,
    bytes: Vec<u8>,
}
impl Clipboard {
    fn new_watching(display: u32, watching: bool) -> Result<Self, DesktopError> {
        let (conn, screen) = RustConnection::connect(None).map_err(error)?;
        let root = conn
            .setup()
            .roots
            .get(screen)
            .ok_or_else(|| error("display unavailable"))?
            .root;
        let _ = display;
        let window = conn.generate_id().map_err(error)?;
        conn.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(error)?
        .check()
        .map_err(error)?;
        let atom = |name: &[u8]| {
            conn.intern_atom(false, name)
                .map_err(error)?
                .reply()
                .map(|r| r.atom)
                .map_err(error)
        };
        let clipboard = atom(b"CLIPBOARD")?;
        let utf8 = atom(b"UTF8_STRING")?;
        let targets = atom(b"TARGETS")?;
        let timestamp = atom(b"TIMESTAMP")?;
        let incr = atom(b"INCR")?;
        let marker = atom(b"RDS_CLIPBOARD_TIME")?;
        let property = atom(b"RDS_CLIPBOARD_READ")?;
        let xfixes = watching
            && conn
                .extension_information(xfixes::X11_EXTENSION_NAME)
                .map_err(error)?
                .is_some()
            && conn
                .xfixes_query_version(5, 0)
                .map_err(error)?
                .reply()
                .map_err(error)?
                .major_version
                >= 2;
        if xfixes {
            conn.xfixes_select_selection_input(
                window,
                clipboard,
                xfixes::SelectionEventMask::SET_SELECTION_OWNER
                    | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        }
        conn.flush().map_err(error)?;
        let chunk = conn
            .maximum_request_bytes()
            .saturating_sub(128)
            .min(32 * 1024);
        let last_owner = conn
            .get_selection_owner(clipboard)
            .map_err(error)?
            .reply()
            .map_err(error)?
            .owner;
        Ok(Self {
            conn,
            window,
            root,
            clipboard,
            utf8,
            targets,
            timestamp,
            incr,
            marker,
            property,
            time: 0,
            last_owner,
            watching,
            xfixes,
            fallback_at: Instant::now(),
            incoming: None,
            external: None,
            text: Arc::from([]),
            transfers: Vec::new(),
            chunk,
        })
    }
    fn cancel_read(&mut self) {
        if let Some(read) = self.incoming.take() {
            let _ = self.conn.destroy_window(read.window);
        }
    }
    fn publish(&mut self, text: String) -> Result<(), DesktopError> {
        if text.len() > super::MAX_TEXT_BYTES {
            return Err(error("text exceeds 1 MiB"));
        }
        self.cancel_read();
        self.external = None;
        self.conn
            .delete_property(self.window, self.property)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.conn
            .change_property8(
                PropMode::REPLACE,
                self.window,
                self.marker,
                AtomEnum::INTEGER,
                &[0],
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if Instant::now() >= deadline {
                return Err(error("server timestamp unavailable"));
            }
            match self.conn.poll_for_event().map_err(error)? {
                Some(Event::PropertyNotify(e))
                    if e.window == self.window && e.atom == self.marker =>
                {
                    self.time = e.time;
                    break;
                }
                Some(event) => self.event(event)?,
                None => {
                    let mut fds = [rustix::event::PollFd::new(
                        self.conn.stream(),
                        rustix::event::PollFlags::IN,
                    )];
                    let timeout = rustix::event::Timespec {
                        tv_sec: 0,
                        tv_nsec: 10_000_000,
                    };
                    rustix::event::poll(&mut fds, Some(&timeout)).map_err(error)?;
                }
            }
        }
        self.conn
            .set_selection_owner(self.window, self.clipboard, self.time)
            .map_err(error)?
            .check()
            .map_err(error)?;
        if self
            .conn
            .get_selection_owner(self.clipboard)
            .map_err(error)?
            .reply()
            .map_err(error)?
            .owner
            != self.window
        {
            return Err(error("selection ownership refused"));
        }
        self.text = Arc::from(text.into_bytes());
        self.last_owner = self.window;
        tracing::info!(bytes = self.text.len(), "remote text clipboard published");
        Ok(())
    }

    fn owner_changed(&mut self, owner: Window, timestamp: u32) -> Result<(), DesktopError> {
        self.last_owner = owner;
        self.cancel_read();
        self.external = None;
        if !self.watching || owner == 0 || owner == self.window {
            return Ok(());
        }
        // A fresh requestor window separates old INCR writes from a new owner.
        // Destroying it cancels old writes without leaking a new Atom per copy.
        let window = self.conn.generate_id().map_err(error)?;
        self.conn
            .create_window(
                0,
                window,
                self.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.incoming = Some(Incoming {
            window,
            timestamp,
            started: Instant::now(),
            increment: false,
            bytes: Vec::new(),
        });
        self.conn
            .convert_selection(window, self.clipboard, self.utf8, self.property, timestamp)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.conn.flush().map_err(error)
    }
    fn selection_reply(&mut self, event: SelectionNotifyEvent) -> Result<(), DesktopError> {
        let Some(incoming) = &self.incoming else {
            return Ok(());
        };
        if event.requestor != incoming.window
            || event.selection != self.clipboard
            || event.target != self.utf8
            || (incoming.timestamp != 0 && event.time != incoming.timestamp)
        {
            return Ok(());
        }
        if event.property == 0 {
            self.cancel_read();
            return Ok(());
        }
        if event.property != self.property {
            return Ok(());
        }
        self.read_property(false)
    }
    fn read_property(&mut self, increment: bool) -> Result<(), DesktopError> {
        let Some(requestor) = self.incoming.as_ref().map(|read| read.window) else {
            return Ok(());
        };
        let part = self
            .conn
            .get_property(
                true,
                requestor,
                self.property,
                AtomEnum::ANY,
                0,
                (super::MAX_TEXT_BYTES as u32).div_ceil(4),
            )
            .map_err(error)?
            .reply()
            .map_err(error)?;
        if part.bytes_after != 0 {
            self.cancel_read();
            return Ok(());
        }
        if !increment && part.type_ == self.incr {
            let Some(incoming) = &mut self.incoming else {
                return Ok(());
            };
            let advertised = part.value32().and_then(|mut values| values.next());
            if part.format != 32 || advertised.is_none_or(|n| n as usize > super::MAX_TEXT_BYTES) {
                self.cancel_read();
            } else {
                incoming.increment = true;
            }
            self.conn.flush().map_err(error)?;
            return Ok(());
        }
        if increment && part.value.is_empty() {
            if let Some(done) = self.incoming.take() {
                let _ = self.conn.destroy_window(done.window);
                self.external = String::from_utf8(done.bytes).ok();
            }
            self.conn.flush().map_err(error)?;
            return Ok(());
        }
        let Some(incoming) = &mut self.incoming else {
            return Ok(());
        };
        if part.type_ != self.utf8
            || part.format != 8
            || incoming.bytes.len().saturating_add(part.value.len()) > super::MAX_TEXT_BYTES
        {
            self.cancel_read();
            return Ok(());
        }
        incoming.bytes.extend_from_slice(&part.value);
        if !increment && let Some(done) = self.incoming.take() {
            self.external = String::from_utf8(done.bytes).ok();
        }
        self.conn.flush().map_err(error)
    }
    fn pump(&mut self) -> Result<(), DesktopError> {
        if self
            .incoming
            .as_ref()
            .is_some_and(|read| read.started.elapsed() >= Duration::from_secs(2))
        {
            self.cancel_read();
        }
        if self.watching && !self.xfixes && self.fallback_at.elapsed() >= Duration::from_millis(250)
        {
            self.fallback_at = Instant::now();
            let owner = self
                .conn
                .get_selection_owner(self.clipboard)
                .map_err(error)?
                .reply()
                .map_err(error)?
                .owner;
            if owner != self.last_owner {
                self.owner_changed(owner, 0)?;
            }
        }
        self.transfers
            .retain(|t| t.started.elapsed() < Duration::from_secs(5));
        for _ in 0..64 {
            match self.conn.poll_for_event().map_err(error)? {
                Some(e) => self.event(e)?,
                None => break,
            }
        }
        self.conn.flush().map_err(error)
    }
    fn event(&mut self, event: Event) -> Result<(), DesktopError> {
        match event {
            Event::XfixesSelectionNotify(e) if e.selection == self.clipboard => {
                self.owner_changed(e.owner, e.selection_timestamp)
            }
            Event::SelectionNotify(e) => self.selection_reply(e),
            Event::PropertyNotify(e)
                if e.atom == self.property
                    && e.state == Property::NEW_VALUE
                    && self
                        .incoming
                        .as_ref()
                        .is_some_and(|read| read.window == e.window && read.increment) =>
            {
                self.read_property(true)
            }
            Event::SelectionRequest(e) => self.request(e),
            Event::PropertyNotify(e) if e.state == Property::DELETE => {
                let Some(index) = self
                    .transfers
                    .iter()
                    .position(|t| t.window == e.window && t.property == e.atom)
                else {
                    return Ok(());
                };
                let t = &mut self.transfers[index];
                let end = (t.offset + self.chunk).min(t.bytes.len());
                let result = self
                    .conn
                    .change_property8(
                        PropMode::REPLACE,
                        t.window,
                        t.property,
                        t.kind,
                        &t.bytes[t.offset..end],
                    )
                    .map_err(error)?
                    .check();
                let done = end == t.offset;
                t.offset = end;
                if result.is_err() || done {
                    self.transfers.swap_remove(index);
                }
                Ok(())
            }
            Event::SelectionClear(e) if e.selection == self.clipboard => {
                self.text = Arc::from([]);
                self.last_owner = 0;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    fn request(&mut self, e: SelectionRequestEvent) -> Result<(), DesktopError> {
        let property = if e.property == 0 {
            e.target
        } else {
            e.property
        };
        let mut accepted = false;
        if e.owner == self.window
            && e.selection == self.clipboard
            && (e.time == 0 || e.time.wrapping_sub(self.time) < u32::MAX / 2)
        {
            if e.target == self.targets {
                accepted = self
                    .conn
                    .change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        AtomEnum::ATOM,
                        &[
                            self.targets,
                            self.timestamp,
                            self.utf8,
                            AtomEnum::STRING.into(),
                        ],
                    )
                    .map_err(error)?
                    .check()
                    .is_ok();
            } else if e.target == self.timestamp {
                accepted = self
                    .conn
                    .change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        AtomEnum::INTEGER,
                        &[self.time],
                    )
                    .map_err(error)?
                    .check()
                    .is_ok();
            } else if e.target == self.utf8
                || (e.target == u32::from(AtomEnum::STRING) && self.text.is_ascii())
            {
                if self.text.len() <= self.chunk {
                    accepted = self
                        .conn
                        .change_property8(
                            PropMode::REPLACE,
                            e.requestor,
                            property,
                            e.target,
                            &self.text,
                        )
                        .map_err(error)?
                        .check()
                        .is_ok();
                } else if self.transfers.len() < 8
                    && !self
                        .transfers
                        .iter()
                        .any(|t| t.window == e.requestor && t.property == property)
                    && self
                        .conn
                        .change_window_attributes(
                            e.requestor,
                            &ChangeWindowAttributesAux::new()
                                .event_mask(EventMask::PROPERTY_CHANGE),
                        )
                        .map_err(error)?
                        .check()
                        .is_ok()
                {
                    accepted = self
                        .conn
                        .change_property32(
                            PropMode::REPLACE,
                            e.requestor,
                            property,
                            self.incr,
                            &[self.text.len() as u32],
                        )
                        .map_err(error)?
                        .check()
                        .is_ok();
                    if accepted {
                        self.transfers.push(Transfer {
                            window: e.requestor,
                            property,
                            kind: e.target,
                            bytes: self.text.clone(),
                            offset: 0,
                            started: Instant::now(),
                        });
                    }
                }
            }
        }
        let reply = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if accepted { property } else { 0 },
        };
        // A requestor may disappear; that is one failed transfer, not loss of
        // the session's selection owner. All retained transfers are bounded.
        let _ = self
            .conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, reply)
            .map_err(error)?
            .check();
        self.conn.flush().map_err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn receive(
        conn: &RustConnection,
        window: Window,
        selection: Atom,
        utf8: Atom,
        property: Atom,
    ) -> Vec<u8> {
        conn.convert_selection(window, selection, utf8, property, 0u32)
            .unwrap()
            .check()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(Instant::now() < deadline, "selection owner failed to reply");
            if let Some(Event::SelectionNotify(event)) = conn.poll_for_event().unwrap() {
                assert_ne!(event.property, 0);
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let incr = conn
            .intern_atom(false, b"INCR")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let first = conn
            .get_property(
                false,
                window,
                property,
                AtomEnum::ANY,
                0,
                super::super::MAX_TEXT_BYTES as u32,
            )
            .unwrap()
            .reply()
            .unwrap();
        if first.type_ != incr {
            return first.value;
        }
        while conn.poll_for_event().unwrap().is_some() {}
        conn.delete_property(window, property)
            .unwrap()
            .check()
            .unwrap();
        let mut result = Vec::new();
        loop {
            assert!(
                Instant::now() < deadline,
                "INCR transfer failed to terminate"
            );
            if let Some(Event::PropertyNotify(event)) = conn.poll_for_event().unwrap()
                && event.window == window
                && event.atom == property
                && event.state == Property::NEW_VALUE
            {
                let part = conn
                    .get_property(
                        true,
                        window,
                        property,
                        utf8,
                        0,
                        super::super::MAX_TEXT_BYTES as u32,
                    )
                    .unwrap()
                    .reply()
                    .unwrap();
                if part.value.is_empty() {
                    break;
                }
                result.extend_from_slice(&part.value);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        result
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a dedicated Xvfb server; owns its clipboard"]
    async fn native_clipboard_serves_utf8_and_large_incr_without_a_helper() {
        let (conn, _) = RustConnection::connect(None).expect("dedicated X11 server required");
        let window = conn.generate_id().unwrap();
        conn.create_window(
            0,
            window,
            conn.setup().roots[0].root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap()
        .check()
        .unwrap();
        let atom = |name: &[u8]| conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
        let (selection, utf8, property) = (
            atom(b"CLIPBOARD"),
            atom(b"UTF8_STRING"),
            atom(b"RDS_TEST_CLIP"),
        );
        let mut owner = Worker::new(0);
        for text in ["Привет 🖥️\nHello".to_owned(), "Привет 🖥️\n".repeat(8000)]
        {
            owner.publish(text.clone()).await.unwrap();
            assert_eq!(
                receive(&conn, window, selection, utf8, property).await,
                text.as_bytes()
            );
        }
        drop(owner);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a dedicated Xvfb server; exercises native clipboard through real QUIC"]
    async fn bidirectional_clipboard_crosses_an_isolated_v5_session_while_media_waits() {
        use crate::{
            SessionConfig, SyntheticProducer,
            client::{DesktopSession, SessionOpts},
        };
        use rds_core::{
            ClipboardErrorCode, ClipboardFormat, Codec, DesktopCaps, DesktopControl, DesktopEvent,
            DesktopHello, HelloAck, StreamHello, UniHello,
        };
        use rds_net::{read_frame, write_frame};
        let config = || rds_net::EndpointConfig {
            backend: rds_net::Backend::Noq,
            bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
            discovery: false,
            ..Default::default()
        };
        let viewer = rds_net::bind_endpoint(config()).await.unwrap();
        let agent = rds_net::bind_endpoint(config()).await.unwrap();
        let serving = tokio::spawn({
            let agent = agent.clone();
            async move {
                let conn = agent.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = conn.accept_bi().await.unwrap();
                let StreamHello::DesktopV5 { session, hello, .. } =
                    read_frame(&mut recv).await.unwrap()
                else {
                    panic!("expected V5")
                };
                write_frame(
                    &mut send,
                    &HelloAck::DesktopV5(DesktopCaps {
                        displays: vec![],
                        codecs: vec![Codec::H264],
                    }),
                )
                .await
                .unwrap();
                crate::serve_desktop_with(
                    conn,
                    send,
                    recv,
                    hello,
                    SessionConfig {
                        reverse_clipboard: true,
                        payload_receipts: true,
                        frame_route: Some(UniHello::DesktopFrames { id: session }),
                        producer: Some(Box::new(SyntheticProducer::new(5, 32, 32, 64))),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
        });
        let conn = viewer.connect(agent.addr(), rds_core::ALPN).await.unwrap();
        let mut session = DesktopSession::connect_opts(
            &conn,
            DesktopHello {
                display: 0,
                max_fps: 5,
                codec: Codec::H264,
                input_acks: false,
            },
            SessionOpts {
                session: Some(rand::random()),
                reverse_clipboard: true,
                payload_receipts: true,
                relay_encoded: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let local = "copied on Mac 🖥️";
        session
            .send_control(DesktopControl::ClipboardChunk {
                id: 7,
                offset: 0,
                total: local.len() as u32,
                data: local.as_bytes().to_vec(),
            })
            .await
            .unwrap();
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(3), session.events.recv()).await.unwrap().unwrap(), DesktopEvent::ClipboardReady { id: 7, bytes } if bytes as usize == local.len())
        );
        let (observer, _) = RustConnection::connect(None).unwrap();
        let window = observer.generate_id().unwrap();
        observer
            .create_window(
                0,
                window,
                observer.setup().roots[0].root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .unwrap()
            .check()
            .unwrap();
        let atom = |name: &[u8]| {
            observer
                .intern_atom(false, name)
                .unwrap()
                .reply()
                .unwrap()
                .atom
        };
        assert_eq!(
            receive(
                &observer,
                window,
                atom(b"CLIPBOARD"),
                atom(b"UTF8_STRING"),
                atom(b"RDS_WIRE_TEST")
            )
            .await,
            local.as_bytes()
        );
        let remote = "copied remotely 🖥️\n".repeat(8000);
        let mut external = Worker::new(0);
        external.publish(remote.clone()).await.unwrap();
        let DesktopEvent::ClipboardOffer { id, format, bytes } =
            tokio::time::timeout(Duration::from_secs(3), session.events.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("no remote offer")
        };
        assert_eq!(format, ClipboardFormat::TextUtf8);
        assert_eq!(bytes as usize, remote.len());
        session
            .send_control(DesktopControl::ClipboardRequest {
                id: id.wrapping_add(1),
                format,
            })
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), session.events.recv())
                .await
                .unwrap()
                .unwrap(),
            DesktopEvent::ClipboardError {
                code: ClipboardErrorCode::InvalidRequest,
                ..
            }
        ));
        session
            .send_control(DesktopControl::ClipboardRequest { id, format })
            .await
            .unwrap();
        session.heartbeat().await.unwrap();
        let mut assembly = super::super::Assembly::default();
        let mut complete = None;
        let mut heartbeat = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while complete.is_none() || !heartbeat {
            let event = tokio::time::timeout_at(deadline, session.events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                DesktopEvent::ClipboardChunk {
                    id: got,
                    offset,
                    total,
                    data,
                } => {
                    assert_eq!(got, id);
                    if let Some(text) = assembly.push(got, offset, total, data).unwrap() {
                        complete = Some(text);
                    }
                }
                DesktopEvent::Heartbeat { .. } => heartbeat = true,
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(complete.as_deref(), Some(remote.as_str()));
        observer.destroy_window(window).unwrap().check().unwrap();
        drop(session);
        conn.close(0u32.into(), b"test complete");
        tokio::time::timeout(Duration::from_secs(3), serving)
            .await
            .unwrap()
            .unwrap();
        viewer.close().await;
        agent.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a dedicated Xvfb server; reads an external selection owner"]
    async fn native_clipboard_watcher_reads_external_owner_without_echoing_own_publish() {
        let mut external = Worker::new(0);
        let mut watcher = Worker::watch(0);
        watcher.ready().await.unwrap();
        for text in [
            "copied on the remote desktop".to_owned(),
            "repeat from the same owner".to_owned(),
            "Привет 🖥️\n".repeat(8000),
            String::new(),
        ] {
            external.publish(text.clone()).await.unwrap();
            let change = tokio::time::timeout(Duration::from_secs(3), watcher.next_change())
                .await
                .expect("external clipboard change was not observed")
                .expect("clipboard worker ended")
                .text;
            assert_eq!(change, text);
        }
        watcher
            .publish("published by this session".into())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), watcher.next_change())
                .await
                .is_err()
        );
        drop(watcher);
        drop(external);
    }
}
