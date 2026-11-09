//! Requested bodies share the authenticated connection, never its input lane.
//! This owner grants no native access: the desktop first admits capabilities,
//! policy, offers and gestures. Request routes are claimed before metadata is
//! sent, and exist for one body only. No file cache or payload task is created.
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use bytes::Bytes;
use rds_core::{
    UniHello,
    clipboard::{
        Control, Failure, Limits, PayloadKind, Receipt, Request, Side, TransferHeader, TransferId,
    },
};
use rds_net::{Connection, RecvStream, SendStream, UniStreams};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
};
use tokio_util::sync::CancellationToken;

use super::{NativeLifetime, native, stream};
mod refusal;
use refusal::{Refusal, Refusals};
#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Contract(#[from] rds_core::clipboard::Error),
    #[error(transparent)]
    Body(#[from] stream::Error),
    #[error(transparent)]
    Native(#[from] native::Error),
    #[error("clipboard transfer admission is occupied")]
    Busy,
    #[error("clipboard body owner is closed")]
    Closed,
    #[error("peer refused clipboard body: {0:?}")]
    PeerRefused(Failure),
    #[error("clipboard body transport failed: {0}")]
    Transport(#[from] io::Error),
}
impl Error {
    pub(crate) fn into_native(self) -> native::Error {
        match self {
            Self::Contract(error) => native::Error::Contract(error),
            Self::Body(error) => error.into_native(),
            Self::Native(error) => error,
            Self::Closed => native::Error::Changed,
            Self::Busy => native::Error::Unavailable,
            Self::PeerRefused(code) => match code {
                Failure::Unavailable => native::Error::Unavailable,
                Failure::Unsupported => native::Error::Unsupported,
                Failure::Expired | Failure::Canceled => native::Error::Changed,
                Failure::TooLarge => native::Error::Contract(rds_core::clipboard::Error::TooLarge),
                Failure::NativePublication => native::Error::Publication,
                Failure::Integrity => native::Error::Integrity,
                Failure::InvalidRequest => {
                    native::Error::Contract(rds_core::clipboard::Error::Identity)
                }
                Failure::Io => native::Error::Io(io::Error::other(Self::PeerRefused(code))),
            },
            Self::Transport(error) => native::Error::Io(error),
        }
    }
}
fn body_error(error: stream::Error) -> Error {
    if let stream::Error::Io(io) = &error
        && let Some(Error::PeerRefused(code)) =
            io.get_ref().and_then(|error| error.downcast_ref::<Error>())
    {
        return Error::PeerRefused(*code);
    }
    Error::Body(error)
}

/// One negotiated desktop's bulk owner. Each direction has its own budget:
/// two simultaneous requests must not prevent either peer from responding.
pub struct Streams {
    connection: Connection,
    desktop: [u8; 16],
    side: Side,
    limits: Limits,
    metadata: mpsc::Sender<Control>,
    next: Mutex<u64>,
    incoming: Arc<Semaphore>,
    outgoing: Arc<Semaphore>,
    stop: CancellationToken,
    original: NativeLifetime,
    refusals: Arc<Refusals>,
}

impl Streams {
    pub fn new(
        connection: Connection,
        desktop: [u8; 16],
        side: Side,
        limits: Limits,
        metadata: mpsc::Sender<Control>,
        original: NativeLifetime,
    ) -> Result<Self, Error> {
        limits.validate()?;
        if desktop == [0; 16] {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let parallel = limits.parallel_transfers as usize;
        Ok(Self {
            connection,
            desktop,
            side,
            limits,
            metadata,
            next: Mutex::new(1),
            incoming: Arc::new(Semaphore::new(parallel)),
            outgoing: Arc::new(Semaphore::new(parallel)),
            stop: CancellationToken::new(),
            original,
            refusals: Arc::default(),
        })
    }

    /// Seal admission and stop active network I/O. Native jobs retain their
    /// original owner independently until their actual syscalls have drained.
    pub fn close(&self) {
        self.stop.cancel();
        self.incoming.close();
        self.outgoing.close();
    }
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Reserve metadata capacity, body credit and the receive route before
    /// publishing the request. Sequence allocation and queue commit share one
    /// synchronous critical section, so concurrent callers cannot reorder IDs.
    /// The selected offer must already be admitted by the desktop owner.
    pub fn request_into(
        &self,
        offer: u64,
        kind: PayloadKind,
        bytes: Option<u64>,
        scope: CancellationToken,
        original: NativeLifetime,
    ) -> Result<Incoming, Error> {
        if scope.is_cancelled() || self.stop.is_cancelled() {
            return Err(Error::Closed);
        }
        let metadata = self.metadata.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => Error::Busy,
            mpsc::error::TrySendError::Closed(_) => Error::Closed,
        })?;
        let permit = self
            .incoming
            .clone()
            .try_acquire_owned()
            .map_err(|_| self.admission_error())?;
        let mut next = self.next.lock().map_err(|_| Error::Closed)?;
        let sequence = self.side.request_id(*next)?;
        *next = next.checked_add(1).ok_or(Error::Closed)?;
        let request = Request {
            id: TransferId {
                desktop: self.desktop,
                offer,
                request: sequence,
            },
            kind,
            bytes,
        };
        request.validate(&self.limits)?;
        let inbox = self
            .connection
            .uni_streams(UniHello::ClipboardBody { id: request.id })
            .map_err(io::Error::other)?;
        let refusal = self.refusals.track(request.id)?;
        let incoming = Incoming {
            inbox,
            request: request.clone(),
            limits: self.limits.clone(),
            stop: self.stop.clone(),
            scope,
            refusal: refusal.clone(),
            lease: Arc::new(Lease {
                _permit: permit,
                _original: self.original.clone(),
                _operation_original: Some(original),
                _refusal: Some(refusal),
            }),
        };
        metadata.send(Control::Request(request));
        drop(next);
        Ok(incoming)
    }

    /// Route a peer's Failed metadata to this desktop's exact admitted body.
    /// An unknown/finished request is not a new authorization or a session fault.
    pub fn refused(&self, id: TransferId, code: Failure) -> Result<bool, Error> {
        id.validate()?;
        if id.desktop != self.desktop || !self.side.owns_request(id.request) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        self.refusals.signal(id, code)
    }

    fn admission_error(&self) -> Error {
        if self.stop.is_cancelled() {
            Error::Closed
        } else {
            Error::Busy
        }
    }

    pub(crate) fn send_metadata(&self, control: Control) -> Result<(), Error> {
        if self.stop.is_cancelled() {
            return Err(Error::Closed);
        }
        self.metadata
            .try_send(control)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::Busy,
                mpsc::error::TrySendError::Closed(_) => Error::Closed,
            })
    }

    /// Reserve before reading a native representation. The response's
    /// original owner accompanies each actual native operation.
    pub fn response(&self, request: &Request) -> Result<Response<'_>, Error> {
        request.validate(&self.limits)?;
        if request.id.desktop != self.desktop || !self.side.peer().owns_request(request.id.request)
        {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let permit = self
            .outgoing
            .clone()
            .try_acquire_owned()
            .map_err(|_| self.admission_error())?;
        Ok(Response {
            owner: self,
            request: request.clone(),
            lease: Arc::new(Lease {
                _permit: permit,
                _original: self.original.clone(),
                _operation_original: None,
                _refusal: None,
            }),
        })
    }

    async fn open(&self, id: TransferId) -> Result<Sending, Error> {
        let stream = self.connection.open_uni().await.map_err(io::Error::other)?;
        let mut sending = Sending {
            stream,
            finished: false,
        };
        // Lower than both control and media. Only metadata uses control class.
        sending.stream.set_priority(0).map_err(io::Error::other)?;
        rds_net::write_frame(&mut sending.stream, &UniHello::ClipboardBody { id }).await?;
        Ok(sending)
    }

    pub async fn send_file(
        &self,
        request: &Request,
        source: &dyn native::Files,
    ) -> Result<Receipt, Error> {
        self.response(request)?.send_file(source).await
    }
}
impl Drop for Streams {
    fn drop(&mut self) {
        self.close();
    }
}

/// One admitted response. It is consumed by the single outgoing body and
/// stays alive through native read cancellation and actual I/O drain.
pub struct Response<'a> {
    owner: &'a Streams,
    request: Request,
    lease: NativeLifetime,
}
impl Response<'_> {
    pub fn original(&self) -> NativeLifetime {
        self.lease.clone()
    }
    fn header(&self, bytes: u64) -> Result<TransferHeader, Error> {
        let header = TransferHeader {
            id: self.request.id,
            kind: self.request.kind,
            bytes,
        };
        header.validate(&self.request, &self.owner.limits)?;
        Ok(header)
    }
    pub async fn send_bytes(self, bytes: Bytes) -> Result<Receipt, Error> {
        if matches!(self.request.kind, PayloadKind::File(_)) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let header = self.header(bytes.len() as u64)?;
        tokio::select! {
            biased;
            _ = self.owner.stop.cancelled() => Err(Error::Closed),
            result = async {
                let mut sending = self.owner.open(self.request.id).await?;
                let receipt = stream::send(&mut bytes.as_ref(), &mut sending.stream, &header, &self.owner.limits).await?;
                sending.finish()?;
                Ok(receipt)
            } => result,
        }
    }

    pub async fn send_file(self, source: &dyn native::Files) -> Result<Receipt, Error> {
        let PayloadKind::File(file) = self.request.kind else {
            return Err(rds_core::clipboard::Error::Identity.into());
        };
        source.selection().validate(&self.owner.limits)?;
        let bytes = source
            .selection()
            .entries
            .iter()
            .find(|entry| entry.id == file)
            .ok_or(native::Error::File(rds_sync::selection::Error::Unknown))?
            .bytes;
        // Reserve before opening the native reader. Each actual source syscall
        // owns the reservation, even if its async sender has been canceled.
        let header = self.header(bytes)?;
        tokio::select! {
            biased;
            _ = self.owner.stop.cancelled() => Err(Error::Closed),
            result = async {
                let source = source.open(file, self.original()).await?;
                if source.bytes != header.bytes {
                    return Err(rds_core::clipboard::Error::Identity.into());
                }
                let mut sending = self.owner.open(self.request.id).await?;
                let receipt = stream::send_file(source, &mut sending.stream, &header, &self.owner.limits).await?;
                sending.finish()?;
                Ok(receipt)
            } => result,
        }
    }
}

struct Lease {
    _operation_original: Option<NativeLifetime>,
    _original: NativeLifetime,
    _refusal: Option<Arc<Refusal>>,
    // Admission is released after the request's original owner is cleaned up.
    _permit: OwnedSemaphorePermit,
}

pub struct Incoming {
    inbox: UniStreams,
    request: Request,
    limits: Limits,
    stop: CancellationToken,
    scope: CancellationToken,
    refusal: Arc<Refusal>,
    lease: NativeLifetime,
}

/// Verified representation bytes keep their receiving admission through
/// parsing and publication. Native publication retains `original()` through
/// its actual syscall, even when its waiting future is canceled.
pub struct Received {
    pub bytes: Bytes,
    pub receipt: Receipt,
    lease: NativeLifetime,
    refusal: Arc<Refusal>,
}
impl Received {
    pub fn original(&self) -> NativeLifetime {
        self.lease.clone()
    }
    pub fn refusal(&self) -> Option<Failure> {
        self.refusal.current()
    }
    pub async fn refused(&self) -> Failure {
        self.refusal.wait().await
    }
}
impl Incoming {
    pub fn request(&self) -> &Request {
        &self.request
    }

    async fn accept(&mut self) -> Result<Receiving, Error> {
        let stream = tokio::select! {
            biased;
            _ = self.stop.cancelled() => return Err(Error::Closed),
            _ = self.scope.cancelled() => return Err(Error::Closed),
            code = self.refusal.wait() => return Err(Error::PeerRefused(code)),
            stream = self.inbox.recv() => stream.ok_or(Error::Closed)?,
        };
        Ok(Receiving {
            stream,
            stopped: Box::pin({
                let stop = self.stop.clone();
                let scope = self.scope.clone();
                let refusal = self.refusal.clone();
                async move {
                    tokio::select! { biased;
                        _ = stop.cancelled() => Stopped::Closed,
                        _ = scope.cancelled() => Stopped::Closed,
                        code = refusal.wait() => Stopped::Refused(code),
                    }
                }
            }),
            terminal: None,
            _original: self.lease.clone(),
        })
    }

    pub async fn bytes(mut self) -> Result<Received, Error> {
        if matches!(self.request.kind, PayloadKind::File(_)) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let mut wire = self.accept().await?;
        // Stop accepting duplicate bodies before consuming this one.
        drop(self.inbox);
        let mut bytes = Vec::new();
        let receipt = tokio::select! { biased;
            code = self.refusal.wait() => return Err(Error::PeerRefused(code)),
            result = stream::receive(&mut wire, &mut bytes, &self.request, &self.limits) => result.map_err(body_error)?,
        };
        if let Some(code) = self.refusal.current() {
            return Err(Error::PeerRefused(code));
        }
        Ok(Received {
            bytes: Bytes::from(bytes),
            receipt,
            lease: self.lease,
            refusal: self.refusal,
        })
    }

    /// A downstream native destination or remote peer consumes this reader.
    /// Both reader and integrity proof retain its budget and original owner.
    pub async fn file(mut self) -> Result<native::FileRead, Error> {
        if !matches!(self.request.kind, PayloadKind::File(_)) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let wire = self.accept().await?;
        drop(self.inbox);
        let incoming = stream::receive_reader(wire, &self.request, &self.limits, self.lease);
        let native::FileRead {
            bytes,
            reader,
            complete,
        } = tokio::select! { biased;
            code = self.refusal.wait() => return Err(Error::PeerRefused(code)),
            result = incoming => result.map_err(body_error)?,
        };
        let reader = self.refusal.reader(reader);
        let refusal = self.refusal;
        Ok(native::FileRead {
            bytes,
            reader,
            complete: Box::pin(async move {
                tokio::select! { biased;
                    code = refusal.wait() => Err(Error::PeerRefused(code).into_native()),
                    result = complete => result,
                }
            }),
        })
    }

    pub async fn write(mut self, destination: native::Destination) -> Result<Receipt, Error> {
        if !matches!(self.request.kind, PayloadKind::File(_)) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let mut wire = self.accept().await?;
        drop(self.inbox);
        let destination = destination.retaining(self.lease);
        let receipt = tokio::select! { biased;
            code = self.refusal.wait() => return Err(Error::PeerRefused(code)),
            result = stream::receive_file(&mut wire, destination, &self.request, &self.limits) => result.map_err(body_error)?,
        };
        if let Some(code) = self.refusal.current() {
            return Err(Error::PeerRefused(code));
        }
        Ok(receipt)
    }
}

struct Receiving {
    stream: RecvStream,
    stopped: Pin<Box<dyn Future<Output = Stopped> + Send>>,
    terminal: Option<Stopped>,
    _original: NativeLifetime,
}
#[derive(Clone, Copy)]
enum Stopped {
    Closed,
    Refused(Failure),
}
impl Stopped {
    fn error(self) -> io::Error {
        match self {
            Self::Closed => io::ErrorKind::Interrupted.into(),
            Self::Refused(code) => io::Error::other(Error::PeerRefused(code)),
        }
    }
}
impl AsyncRead for Receiving {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(reason) = self.terminal {
            return Poll::Ready(Err(reason.error()));
        }
        if let Poll::Ready(reason) = self.stopped.as_mut().poll(cx) {
            self.terminal = Some(reason);
            let _ = self.stream.stop(0u32.into());
            return Poll::Ready(Err(reason.error()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}

struct Sending {
    stream: SendStream,
    finished: bool,
}
impl Sending {
    fn finish(&mut self) -> Result<(), Error> {
        self.stream.finish().map_err(io::Error::other)?;
        self.finished = true;
        Ok(())
    }
}
impl Drop for Sending {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.stream.reset(0u32.into());
        }
    }
}
