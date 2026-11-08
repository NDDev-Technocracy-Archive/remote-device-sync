//! One admitted remote selection, usable by a native promise or another peer.
use rds_core::clipboard::{Control, FileSelection, Limits, PayloadKind, TransferId};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

use super::{Destination, Error, FileRead, FileReceipt, Files, Operation};
use crate::clipboard::transport::{self, Incoming, Streams};

/// A selected source capability, not OS publication or arbitrary file access.
/// The desktop binds it to an admitted offer and cancels `scope` on withdrawal,
/// replacement or revocation. Its names never become remote filesystem paths.
pub struct RemoteFiles {
    offer: u64,
    selection: FileSelection,
    streams: Arc<Streams>,
    metadata: mpsc::Sender<Control>,
    scope: CancellationToken,
}
impl RemoteFiles {
    pub fn new(
        offer: u64,
        selection: FileSelection,
        streams: Arc<Streams>,
        metadata: mpsc::Sender<Control>,
        scope: CancellationToken,
    ) -> Result<Self, Error> {
        if offer == 0 {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        selection.validate(streams.limits())?;
        if scope.is_cancelled() {
            return Err(Error::Changed);
        }
        Ok(Self {
            offer,
            selection,
            streams,
            metadata,
            scope,
        })
    }

    fn request(&self, file: [u8; 16]) -> Result<(Incoming, Arc<Pending>), Error> {
        if self.scope.is_cancelled() {
            return Err(Error::Changed);
        }
        let entry = self
            .selection
            .entries
            .iter()
            .find(|entry| entry.id == file)
            .ok_or(rds_sync::selection::Error::Unknown)?;
        let incoming = self
            .streams
            .request(
                self.offer,
                PayloadKind::File(file),
                Some(entry.bytes),
                self.scope.clone(),
            )
            .map_err(transport::Error::into_native)?;
        let request = incoming.request().clone();
        // Claiming the route preceded metadata admission. A saturated/closed
        // queue cannot create unbounded waiting request jobs or read any body.
        self.metadata
            .try_send(Control::Request(request.clone()))
            .map_err(|_| Error::Unavailable)?;
        let pending = Arc::new(Pending {
            id: request.id,
            metadata: self.metadata.clone(),
            scope: self.scope.clone(),
            verified: AtomicBool::new(false),
        });
        Ok((incoming, pending))
    }
}
impl Files for RemoteFiles {
    fn selection(&self) -> &FileSelection {
        &self.selection
    }
    fn limits(&self) -> &Limits {
        self.streams.limits()
    }

    fn open(&self, file: [u8; 16]) -> Operation<'_, FileRead> {
        Box::pin(async move {
            let (incoming, pending) = self.request(file)?;
            let FileRead {
                bytes,
                reader,
                complete,
            } = incoming
                .file()
                .await
                .map_err(transport::Error::into_native)?;
            if pending.scope.is_cancelled() {
                return Err(Error::Changed);
            }
            Ok(FileRead {
                bytes,
                reader: Box::pin(HeldReader {
                    reader,
                    _pending: pending.clone(),
                }),
                complete: Box::pin(async move {
                    complete.await?;
                    pending.finish()
                }),
            })
        })
    }

    fn write(&self, file: [u8; 16], destination: Destination) -> Operation<'_, FileReceipt> {
        Box::pin(async move {
            let (incoming, pending) = self.request(file)?;
            // Share the transport verifier and positional native destination
            // writer: no staging, second body hash, or queued file-copy buffer.
            let receipt = incoming
                .write(destination)
                .await
                .map_err(transport::Error::into_native)?;
            if pending.scope.is_cancelled() {
                return Err(Error::Changed);
            }
            self.metadata
                .try_send(Control::Receipt(receipt))
                .map_err(|_| Error::Unavailable)?;
            pending.finish()?;
            Ok(FileReceipt {
                bytes: receipt.bytes,
                digest: receipt.digest,
            })
        })
    }
}

struct Pending {
    id: TransferId,
    metadata: mpsc::Sender<Control>,
    scope: CancellationToken,
    verified: AtomicBool,
}
impl Pending {
    fn finish(&self) -> Result<(), Error> {
        if self.scope.is_cancelled() {
            return Err(Error::Changed);
        }
        self.verified.store(true, Ordering::Release);
        Ok(())
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.verified.load(Ordering::Acquire)
            && self
                .metadata
                .try_send(Control::Cancel { id: self.id })
                .is_err()
        {
            // The dropped receive route/stream also stops the sender. This
            // advisory message cannot retain a canceled native operation.
            tracing::debug!("clipboard cancellation metadata unavailable");
        }
    }
}

struct HeldReader {
    // Release stream/body ownership before its cancellation-control owner.
    reader: Pin<Box<dyn AsyncRead + Send>>,
    _pending: Arc<Pending>,
}
impl AsyncRead for HeldReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.get_mut().reader.as_mut().poll_read(cx, buffer)
    }
}
