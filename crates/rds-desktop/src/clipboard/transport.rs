//! Requested bodies share the authenticated connection, never its input lane.
//! This owner grants no native access: the desktop first admits capabilities,
//! policy, offers and gestures. Request routes are claimed before metadata is
//! sent, and exist for one body only. No file cache or payload task is created.
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use bytes::Bytes;
use rds_core::{
    UniHello,
    clipboard::{Limits, PayloadKind, Receipt, Request, TransferHeader, TransferId},
};
use rds_net::{Connection, RecvStream, SendStream, UniStreams};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_util::sync::CancellationToken;

use super::{NativeLifetime, native, stream};
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
    #[error("clipboard body budget is occupied")]
    Busy,
    #[error("clipboard body owner is closed")]
    Closed,
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
            Self::Transport(error) => native::Error::Io(error),
        }
    }
}

/// One negotiated desktop's bulk owner. Each direction has its own budget:
/// two simultaneous requests must not prevent either peer from responding.
pub struct Streams {
    connection: Connection,
    desktop: [u8; 16],
    limits: Limits,
    next: AtomicU64,
    incoming: Arc<Semaphore>,
    outgoing: Arc<Semaphore>,
    stop: CancellationToken,
    original: NativeLifetime,
}

impl Streams {
    pub fn new(
        connection: Connection,
        desktop: [u8; 16],
        limits: Limits,
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
            limits,
            next: AtomicU64::new(1),
            incoming: Arc::new(Semaphore::new(parallel)),
            outgoing: Arc::new(Semaphore::new(parallel)),
            stop: CancellationToken::new(),
            original,
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

    /// The caller validates the selected offer before this admission, then
    /// sends `incoming.request()` over the desktop metadata control stream.
    /// IDs are never reused, including when a request is canceled before send.
    pub fn request(
        &self,
        offer: u64,
        kind: PayloadKind,
        bytes: Option<u64>,
        scope: CancellationToken,
    ) -> Result<Incoming, Error> {
        if scope.is_cancelled() {
            return Err(Error::Closed);
        }
        let permit = self
            .incoming
            .clone()
            .try_acquire_owned()
            .map_err(|_| self.admission_error())?;
        let sequence = self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Closed)?;
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
        Ok(Incoming {
            inbox,
            request,
            limits: self.limits.clone(),
            stop: self.stop.clone(),
            scope,
            lease: Arc::new(Lease {
                _permit: permit,
                _original: self.original.clone(),
            }),
        })
    }

    fn admission_error(&self) -> Error {
        if self.stop.is_cancelled() {
            Error::Closed
        } else {
            Error::Busy
        }
    }

    fn admit_response(
        &self,
        request: &Request,
        bytes: u64,
    ) -> Result<(TransferHeader, OwnedSemaphorePermit), Error> {
        let header = TransferHeader {
            id: request.id,
            kind: request.kind,
            bytes,
        };
        header.validate(request, &self.limits)?;
        if request.id.desktop != self.desktop {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let permit = self
            .outgoing
            .clone()
            .try_acquire_owned()
            .map_err(|_| self.admission_error())?;
        Ok((header, permit))
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

    /// The desktop owner has already admitted this exact peer request once.
    /// Native reads and representation validation precede sending the bytes.
    pub async fn send_bytes(&self, request: &Request, bytes: Bytes) -> Result<Receipt, Error> {
        if matches!(request.kind, PayloadKind::File(_)) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let (header, _permit) = self.admit_response(request, bytes.len() as u64)?;
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(Error::Closed),
            result = async {
                let mut sending = self.open(request.id).await?;
                let receipt = stream::send(&mut bytes.as_ref(), &mut sending.stream, &header, &self.limits).await?;
                sending.finish()?;
                Ok(receipt)
            } => result,
        }
    }

    pub async fn send_file(
        &self,
        request: &Request,
        source: &dyn native::Files,
    ) -> Result<Receipt, Error> {
        let PayloadKind::File(file) = request.kind else {
            return Err(rds_core::clipboard::Error::Identity.into());
        };
        source.selection().validate(&self.limits)?;
        let bytes = source
            .selection()
            .entries
            .iter()
            .find(|entry| entry.id == file)
            .ok_or(native::Error::File(rds_sync::selection::Error::Unknown))?
            .bytes;
        // Reserve before opening the native reader. Each actual source syscall
        // owns the reservation, even if its async sender has been canceled.
        let (header, permit) = self.admit_response(request, bytes)?;
        let lease = Arc::new(Lease {
            _permit: permit,
            _original: self.original.clone(),
        });
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(Error::Closed),
            result = async {
                let source = source.open(file, lease).await?;
                if source.bytes != header.bytes {
                    return Err(rds_core::clipboard::Error::Identity.into());
                }
                let mut sending = self.open(request.id).await?;
                let receipt = stream::send_file(source, &mut sending.stream, &header, &self.limits).await?;
                sending.finish()?;
                Ok(receipt)
            } => result,
        }
    }
}
impl Drop for Streams {
    fn drop(&mut self) {
        self.close();
    }
}

struct Lease {
    _permit: OwnedSemaphorePermit,
    _original: NativeLifetime,
}

pub struct Incoming {
    inbox: UniStreams,
    request: Request,
    limits: Limits,
    stop: CancellationToken,
    scope: CancellationToken,
    lease: NativeLifetime,
}
impl Incoming {
    pub fn request(&self) -> &Request {
        &self.request
    }

    pub(crate) fn retaining(mut self, original: NativeLifetime) -> Self {
        self.lease = Arc::new((self.lease, original));
        self
    }

    async fn accept(&mut self) -> Result<Receiving, Error> {
        let stream = tokio::select! {
            biased;
            _ = self.stop.cancelled() => return Err(Error::Closed),
            _ = self.scope.cancelled() => return Err(Error::Closed),
            stream = self.inbox.recv() => stream.ok_or(Error::Closed)?,
        };
        Ok(Receiving {
            stream,
            stopped: Box::pin({
                let stop = self.stop.clone();
                let scope = self.scope.clone();
                async move {
                    tokio::select! { _ = stop.cancelled() => {}, _ = scope.cancelled() => {} }
                }
            }),
            _original: self.lease.clone(),
        })
    }

    pub async fn bytes(mut self) -> Result<(Bytes, Receipt), Error> {
        if matches!(self.request.kind, PayloadKind::File(_)) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let mut wire = self.accept().await?;
        // Stop accepting duplicate bodies before consuming this one.
        drop(self.inbox);
        let mut bytes = Vec::new();
        let receipt = stream::receive(&mut wire, &mut bytes, &self.request, &self.limits).await?;
        Ok((Bytes::from(bytes), receipt))
    }

    /// A downstream native destination or remote peer consumes this reader.
    /// Both reader and integrity proof retain its budget and original owner.
    pub async fn file(mut self) -> Result<native::FileRead, Error> {
        if !matches!(self.request.kind, PayloadKind::File(_)) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let wire = self.accept().await?;
        drop(self.inbox);
        Ok(stream::receive_reader(wire, &self.request, &self.limits, self.lease).await?)
    }

    pub async fn write(mut self, destination: native::Destination) -> Result<Receipt, Error> {
        if !matches!(self.request.kind, PayloadKind::File(_)) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        let mut wire = self.accept().await?;
        drop(self.inbox);
        let destination = destination.retaining(self.lease);
        Ok(stream::receive_file(&mut wire, destination, &self.request, &self.limits).await?)
    }
}

struct Receiving {
    stream: RecvStream,
    stopped: Pin<Box<dyn Future<Output = ()> + Send>>,
    _original: NativeLifetime,
}
impl AsyncRead for Receiving {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.stopped.as_mut().poll(cx).is_ready() {
            let _ = self.stream.stop(0u32.into());
            return Poll::Ready(Err(io::ErrorKind::Interrupted.into()));
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
