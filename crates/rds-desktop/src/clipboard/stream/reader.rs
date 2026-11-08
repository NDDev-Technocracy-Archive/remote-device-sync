//! Verified remote files remain readers, without a staging file or pump task.
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
};

use rds_core::clipboard::{Limits, PayloadKind, Request, TransferHeader};
use tokio::io::{AsyncRead, ReadBuf};

use super::Error;
use crate::clipboard::{NativeLifetime, native};

/// Read and admit the bounded header before exposing a remote file capability.
/// The reader emits only its selected body. Its completion verifies the exact
/// trailer/EOF, so forwarding cannot turn a failed upstream into valid data.
/// The original desktop/transfer owner remains held by both halves through I/O.
pub async fn receive_reader<R: AsyncRead + Unpin + Send + 'static>(
    mut wire: R,
    expected: &Request,
    limits: &Limits,
    original: NativeLifetime,
) -> Result<native::FileRead, Error> {
    expected.validate(limits)?;
    if !matches!(expected.kind, PayloadKind::File(_)) {
        return Err(rds_core::clipboard::Error::Identity.into());
    }
    let header: TransferHeader = rds_net::read_frame(&mut wire).await?;
    header.validate(expected, limits)?;
    let (reader, proof) = body(wire, header.bytes, Some(original));
    Ok(native::FileRead {
        bytes: header.bytes,
        reader: Box::pin(reader),
        complete: Box::pin(async move {
            proof.await.map(|_| ()).map_err(|error| match error {
                Error::Io(error) => native::Error::Io(error),
                Error::Length | Error::Integrity => native::Error::Integrity,
                Error::Contract(error) => native::Error::Contract(error),
                Error::Source(error) => error,
            })
        }),
    })
}

pub(super) fn body<R>(
    wire: R,
    bytes: u64,
    original: Option<NativeLifetime>,
) -> (Reader<R>, Proof<R>) {
    let state = Arc::new(Mutex::new(State {
        wire,
        remaining: bytes,
        hash: blake3::Hasher::new(),
        trailer: [0; 32],
        trailer_read: 0,
        verdict: Verdict::Pending,
        _original: original,
    }));
    (Reader(state.clone()), Proof(state))
}

#[derive(Clone, Copy)]
enum Failure {
    Io(io::ErrorKind),
    Length,
    Integrity,
}
impl Failure {
    fn stream(self) -> Error {
        match self {
            Self::Io(kind) => Error::Io(io::Error::from(kind)),
            Self::Length => Error::Length,
            Self::Integrity => Error::Integrity,
        }
    }
    fn io(self) -> io::Error {
        match self {
            Self::Io(kind) => io::Error::from(kind),
            Self::Length => io::Error::new(io::ErrorKind::InvalidData, "clipboard body length"),
            Self::Integrity => {
                io::Error::new(io::ErrorKind::InvalidData, "clipboard source integrity")
            }
        }
    }
}
#[derive(Clone, Copy)]
enum Verdict {
    Pending,
    Verified,
    Failed(Failure),
}
struct State<R> {
    // Drop the stream before releasing its original caller on every path.
    wire: R,
    remaining: u64,
    hash: blake3::Hasher,
    trailer: [u8; 32],
    trailer_read: usize,
    verdict: Verdict,
    _original: Option<NativeLifetime>,
}
impl<R: AsyncRead + Unpin> State<R> {
    fn failed(&mut self, failure: Failure) -> Poll<Result<(), Failure>> {
        self.verdict = Verdict::Failed(failure);
        Poll::Ready(Err(failure))
    }
    fn finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Failure>> {
        match self.verdict {
            Verdict::Verified => return Poll::Ready(Ok(())),
            Verdict::Failed(failure) => return Poll::Ready(Err(failure)),
            Verdict::Pending => {}
        }
        if self.remaining != 0 {
            return self.failed(Failure::Length);
        }
        while self.trailer_read < self.trailer.len() {
            let mut buffer = ReadBuf::new(&mut self.trailer[self.trailer_read..]);
            if let Err(error) = ready!(Pin::new(&mut self.wire).poll_read(cx, &mut buffer)) {
                return self.failed(Failure::Io(error.kind()));
            }
            let length = buffer.filled().len();
            if length == 0 {
                return self.failed(Failure::Length);
            }
            self.trailer_read += length;
        }
        if self.hash.finalize().as_bytes() != &self.trailer {
            return self.failed(Failure::Integrity);
        }
        let mut byte = [0; 1];
        let mut tail = ReadBuf::new(&mut byte);
        if let Err(error) = ready!(Pin::new(&mut self.wire).poll_read(cx, &mut tail)) {
            return self.failed(Failure::Io(error.kind()));
        }
        if !tail.filled().is_empty() {
            return self.failed(Failure::Length);
        }
        self.verdict = Verdict::Verified;
        Poll::Ready(Ok(()))
    }
    fn read(&mut self, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.remaining == 0 {
            return self.finish(cx).map(|result| result.map_err(Failure::io));
        }
        if let Verdict::Failed(failure) = self.verdict {
            return Poll::Ready(Err(failure.io()));
        }
        let maximum = self.remaining.min(buffer.remaining() as u64) as usize;
        // Initialize through the parent before lending storage. Updating only
        // a temporary ReadBuf's initialized count would lose that fact.
        buffer.initialize_unfilled_to(maximum);
        let mut bounded = buffer.take(maximum);
        if let Err(error) = ready!(Pin::new(&mut self.wire).poll_read(cx, &mut bounded)) {
            return self
                .failed(Failure::Io(error.kind()))
                .map(|result| result.map_err(Failure::io));
        }
        let length = bounded.filled().len();
        if length == 0 {
            return self
                .failed(Failure::Length)
                .map(|result| result.map_err(Failure::io));
        }
        self.hash.update(bounded.filled());
        buffer.advance(length);
        self.remaining -= length as u64;
        Poll::Ready(Ok(()))
    }
}
pub(super) struct Reader<R>(Arc<Mutex<State<R>>>);
impl<R: AsyncRead + Unpin> AsyncRead for Reader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let Ok(mut state) = self.0.lock() else {
            return Poll::Ready(Err(io::Error::other("clipboard reader unavailable")));
        };
        state.read(cx, buffer)
    }
}

pub(super) struct Proof<R>(Arc<Mutex<State<R>>>);
impl<R: AsyncRead + Unpin> Future for Proof<R> {
    type Output = Result<[u8; 32], Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Ok(mut state) = self.0.lock() else {
            return Poll::Ready(Err(io::Error::other("clipboard reader unavailable").into()));
        };
        ready!(state.finish(cx)).map_err(Failure::stream)?;
        Poll::Ready(Ok(state.trailer))
    }
}
