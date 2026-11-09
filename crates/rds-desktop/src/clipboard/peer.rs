//! One negotiated native desktop's symmetric metadata and requested bodies.
//! This is not an authorization or an OS-provider factory. Prepare it under
//! the original admitted desktop before its handshake acknowledgement; start
//! only after both peers explicitly accepted the same capabilities.
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use rds_core::{
    UniHello,
    clipboard::{
        Capabilities, Control, Failure, Format, Offer, PayloadKind, Receipt, Request,
        ShortcutModifier, Side, TransferId,
    },
};
use rds_net::{Connection, RecvStream, SendStream, UniStreams, read_frame, write_frame};
use tokio::{
    sync::{mpsc, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

use super::{
    NativeLifetime,
    native::{self, PeerPolicy, Provider, Snapshot, Value},
    transport::{self, Incoming, Streams},
};

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Contract(#[from] rds_core::clipboard::Error),
    #[error(transparent)]
    Native(#[from] native::Error),
    #[error(transparent)]
    Body(#[from] transport::Error),
    #[error("native clipboard metadata I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("native clipboard peer is closed")]
    Closed,
    #[error("native clipboard task ended unexpectedly")]
    Task,
}

/// Implemented representations, reduced again by explicit desktop admission.
/// Selected-file publication/lifetime negotiation is not implemented here yet.
pub fn capabilities(provider: &dyn Provider) -> Result<Capabilities, Error> {
    let mut caps = provider.capabilities();
    caps.validate()?;
    caps.receive.retain(|format| *format != Format::Files);
    caps.offer.retain(|format| *format != Format::Files);
    Ok(caps)
}

pub struct Setup {
    pub connection: Connection,
    pub desktop: [u8; 16],
    pub side: Side,
    pub provider: Arc<dyn Provider>,
    pub admitted: Capabilities,
    pub peer: Capabilities,
    pub policy: PeerPolicy,
    pub original: NativeLifetime,
}

/// Owns the exact receive route before either side sends metadata. Never reuse
/// this desktop ID on the connection, including after a failed native channel.
pub struct Prepared {
    connection: Connection,
    inbox: UniStreams,
    provider: Arc<dyn Provider>,
    policy: PeerPolicy,
    shared: Arc<Shared>,
    outgoing: mpsc::Receiver<Control>,
    offers: watch::Sender<Option<Offer>>,
}
struct Shared {
    desktop: [u8; 16],
    side: Side,
    caps: Capabilities,
    peer_shortcut: ShortcutModifier,
    offers: watch::Receiver<Option<Offer>>,
    streams: Arc<Streams>,
    stop: CancellationToken,
}
impl Shared {
    fn seal(&self) {
        self.stop.cancel();
        self.streams.close();
    }
    fn send(&self, control: Control) -> Result<(), Error> {
        self.streams.send_metadata(control).map_err(Error::from)
    }
    fn identity(&self, id: TransferId) -> Result<(), Error> {
        id.validate()?;
        if id.desktop != self.desktop {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Handle(Arc<Shared>);
impl Handle {
    pub fn capabilities(&self) -> &Capabilities {
        &self.0.caps
    }
    pub fn peer_shortcut(&self) -> ShortcutModifier {
        self.0.peer_shortcut
    }
    pub fn offers(&self) -> watch::Receiver<Option<Offer>> {
        self.0.offers.clone()
    }
    pub fn closed(&self) -> bool {
        self.0.stop.is_cancelled()
    }
    /// The caller supplies the exact gesture/paste purpose before admission.
    /// Offers alone never cause a content read or native publication.
    pub fn request(
        &self,
        offer: u64,
        format: Format,
        scope: CancellationToken,
        original: NativeLifetime,
    ) -> Result<Requested, Error> {
        if self.closed() {
            return Err(Error::Closed);
        }
        if !self.0.caps.receive.contains(&format) {
            return Err(rds_core::clipboard::Error::UnsupportedFormat.into());
        }
        let selected = self
            .0
            .offers
            .borrow()
            .clone()
            .filter(|selected| selected.id == offer)
            .ok_or(rds_core::clipboard::Error::Identity)?;
        let representation = selected
            .representations
            .iter()
            .find(|value| value.format == format)
            .ok_or(rds_core::clipboard::Error::UnsupportedFormat)?;
        let incoming = self.0.streams.request_into(
            offer,
            PayloadKind::Representation(format),
            representation.bytes,
            scope.clone(),
            original,
        )?;
        let pending = Arc::new(Pending {
            id: incoming.request().id,
            shared: self.0.clone(),
            receipt: Mutex::new(0),
            scope,
        });
        Ok(Requested {
            incoming,
            pending,
            format,
        })
    }
    /// Cancel only a request made by this side of the admitted desktop.
    pub fn cancel(&self, id: TransferId) -> Result<(), Error> {
        self.0.identity(id)?;
        if !self.0.side.owns_request(id.request) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        self.0.send(Control::Cancel { id })
    }
}

struct Pending {
    id: TransferId,
    shared: Arc<Shared>,
    receipt: Mutex<u8>,
    scope: CancellationToken,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if *self
            .receipt
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            == 0
            && self.shared.send(Control::Cancel { id: self.id }).is_err()
        {
            tracing::debug!("native clipboard cancellation metadata unavailable");
            // A full/closed queue cannot strand the peer's native source read.
            // Closing this clipboard channel drains its peer without touching
            // the admitted desktop's independent input/media connection.
            self.shared.seal();
        }
    }
}
pub struct Requested {
    incoming: Incoming,
    pending: Arc<Pending>,
    format: Format,
}
impl Requested {
    pub fn request(&self) -> &Request {
        self.incoming.request()
    }
    pub async fn bytes(self) -> Result<Received, Error> {
        let body = self.incoming.bytes().await?;
        Ok(Received {
            body,
            pending: self.pending,
            format: self.format,
        })
    }
}
/// One verified body retains cancellation and body admission through actual
/// native publication. Metadata receipts do not prove application consumption.
pub struct Received {
    body: transport::Received,
    pending: Arc<Pending>,
    format: Format,
}
/// Validated bytes and the exact request owner to retain through publication.
/// Pass `received.original()` to the native provider before moving `value`.
pub struct PreparedPublication {
    pub value: native::Publication,
    pub received: Received,
}
impl Received {
    pub fn bytes(&self) -> &bytes::Bytes {
        &self.body.bytes
    }
    pub fn receipt(&self) -> Receipt {
        self.body.receipt
    }
    pub fn refusal(&self) -> Option<Failure> {
        self.body.refusal()
    }
    pub async fn refused(&self) -> Failure {
        self.body.refused().await
    }
    pub fn original(&self) -> NativeLifetime {
        Arc::new((self.body.original(), self.pending.clone()))
    }
    fn available(&self) -> Result<(), Error> {
        if self.pending.scope.is_cancelled() || self.pending.shared.stop.is_cancelled() {
            return Err(Error::Closed);
        }
        if let Some(code) = self.refusal() {
            return Err(transport::Error::PeerRefused(code).into());
        }
        Ok(())
    }
    /// Consume this body's sole preparation admission. Complete PNG decoding
    /// stays off the UI/async executor, retaining the body and original owner
    /// even if the waiting future is dropped while the blocking work runs.
    pub async fn publication(self) -> Result<PreparedPublication, Error> {
        self.available()?;
        let prepared = tokio::task::spawn_blocking(move || {
            let limits = &self.pending.shared.caps.limits;
            let value = match self.format {
                Format::TextUtf8 => {
                    native::Publication::Text(native::Text::new(self.body.bytes.clone(), limits)?)
                }
                Format::ImagePng => native::Publication::Image(
                    super::image::Png::new(self.body.bytes.clone(), limits)
                        .map_err(native::Error::from)?,
                ),
                Format::Files => return Err(native::Error::Unsupported),
            };
            Ok(PreparedPublication {
                value,
                received: self,
            })
        })
        .await
        .map_err(|_| Error::Task)??;
        prepared.received.available()?;
        Ok(prepared)
    }
    pub fn acknowledge(&self, published: bool) -> Result<bool, Error> {
        self.available()?;
        let mut state = self.pending.receipt.lock().map_err(|_| Error::Closed)?;
        let next = if published { 2 } else { 1 };
        if *state >= next {
            return Ok(false);
        }
        let mut receipt = self.body.receipt;
        receipt.published = published;
        self.pending.shared.send(Control::Receipt(receipt))?;
        *state = next;
        Ok(true)
    }
}

pub struct Session {
    shared: Arc<Shared>,
    provider: Arc<dyn Provider>,
    task: Option<JoinHandle<Result<(), Error>>>,
}
impl Session {
    pub fn handle(&self) -> Handle {
        Handle(self.shared.clone())
    }
    fn seal(&self) {
        self.shared.seal();
        // Concrete native providers seal synchronously, then return a drain
        // future. The owning task independently awaits that actual cleanup.
        drop(self.provider.close());
    }
    pub async fn close(mut self) -> Result<(), Error> {
        self.seal();
        self.task
            .take()
            .ok_or(Error::Closed)?
            .await
            .map_err(|_| Error::Task)?
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.seal();
    }
}

impl Prepared {
    pub fn new(setup: Setup) -> Result<Self, Error> {
        setup.policy.validate()?;
        setup.admitted.validate()?;
        setup.peer.validate()?;
        let native = capabilities(setup.provider.as_ref())?;
        if setup.admitted.shortcut != native.shortcut
            || setup
                .admitted
                .receive
                .iter()
                .any(|format| !native.receive.contains(format))
            || setup
                .admitted
                .offer
                .iter()
                .any(|format| !native.offer.contains(format))
            || setup.admitted.limits.intersect(&native.limits)? != setup.admitted.limits
            || setup.desktop == [0; 16]
        {
            return Err(rds_core::clipboard::Error::UnsupportedFormat.into());
        }
        let caps = setup.admitted.negotiate(&setup.peer)?;
        let inbox = setup
            .connection
            .uni_streams(UniHello::ClipboardMetadata { id: setup.desktop })
            .map_err(io::Error::other)?;
        let (metadata, outgoing) = mpsc::channel(setup.policy.metadata_messages);
        let streams = Arc::new(Streams::new(
            setup.connection.clone(),
            setup.desktop,
            setup.side,
            caps.limits.clone(),
            metadata,
            setup.original,
        )?);
        let (offers, changes) = watch::channel(None);
        Ok(Self {
            connection: setup.connection,
            inbox,
            provider: setup.provider,
            policy: setup.policy,
            shared: Arc::new(Shared {
                desktop: setup.desktop,
                side: setup.side,
                caps,
                peer_shortcut: setup.peer.shortcut,
                offers: changes,
                streams,
                stop: CancellationToken::new(),
            }),
            outgoing,
            offers,
        })
    }
    pub fn start(self) -> Result<Session, Error> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::Task)?;
        let shared = self.shared.clone();
        let provider = self.provider.clone();
        let task = runtime.spawn(async move {
            let (incoming, controls) = mpsc::channel(self.policy.metadata_messages);
            let mut io = JoinSet::new();
            io.spawn(read_metadata(
                self.inbox,
                incoming,
                self.policy.startup_timeout_ms,
            ));
            io.spawn(write_metadata(
                self.connection,
                self.shared.desktop,
                self.outgoing,
                self.policy.clone(),
            ));
            let mut actor = Actor {
                shared: self.shared,
                provider: self.provider,
                offers: self.offers,
                local: None,
                generation: 0,
                next_offer: 1,
                last_offer: 0,
                last_request: 0,
                jobs: JoinSet::new(),
                cancels: HashMap::new(),
                io,
            };
            let result = actor.run(controls).await;
            actor.shared.seal();
            drop(actor.provider.close());
            actor.io.abort_all();
            actor.jobs.abort_all();
            while actor.io.join_next().await.is_some() {}
            while actor.jobs.join_next().await.is_some() {}
            actor.cancels.clear();
            actor.offers.send_replace(None);
            actor.provider.close().await;
            result
        });
        Ok(Session {
            shared,
            provider,
            task: Some(task),
        })
    }
}

struct Actor {
    shared: Arc<Shared>,
    provider: Arc<dyn Provider>,
    offers: watch::Sender<Option<Offer>>,
    local: Option<Offer>,
    generation: u64,
    next_offer: u64,
    last_offer: u64,
    last_request: u64,
    jobs: JoinSet<(TransferId, Result<(), Failure>)>,
    cancels: HashMap<TransferId, CancellationToken>,
    io: JoinSet<Result<(), Error>>,
}
impl Actor {
    fn advertise(&mut self, snapshot: Snapshot) -> Result<(), Error> {
        self.generation = snapshot.generation;
        let representations = if snapshot.owned {
            Vec::new()
        } else {
            snapshot
                .representations
                .into_iter()
                .filter(|value| {
                    self.shared.caps.offer.contains(&value.format)
                        && value.bytes.is_none_or(|bytes| {
                            bytes <= self.shared.caps.limits.representation_bytes(value.format)
                        })
                })
                .collect::<Vec<_>>()
        };
        if representations.is_empty() {
            if let Some(old) = self.local.take() {
                self.shared.send(Control::Withdraw { offer: old.id })?;
            }
            return Ok(());
        }
        let offer = Offer {
            id: self.next_offer,
            generation: snapshot.generation,
            representations,
        };
        offer.validate(&self.shared.caps.offer, &self.shared.caps.limits)?;
        self.next_offer = self
            .next_offer
            .checked_add(1)
            .ok_or(rds_core::clipboard::Error::Identity)?;
        self.shared.send(Control::Offer(offer.clone()))?;
        self.local = Some(offer);
        Ok(())
    }
    async fn run(&mut self, mut controls: mpsc::Receiver<Control>) -> Result<(), Error> {
        if !self.shared.caps.offer.is_empty() {
            let snapshot = tokio::select! { biased;
                _ = self.shared.stop.cancelled() => return Ok(()),
                result = self.io.join_next() => return Err(metadata_ended(result)),
                snapshot = self.provider.current() => snapshot?,
            };
            self.advertise(snapshot)?;
        }
        loop {
            tokio::select! { biased;
                _ = self.shared.stop.cancelled() => return Ok(()),
                result = self.io.join_next() => return Err(metadata_ended(result)),
                result = self.jobs.join_next(), if !self.jobs.is_empty() => {
                    let (id, result) = result.ok_or(Error::Task)?.map_err(|_| Error::Task)?;
                    self.cancels.remove(&id);
                    if let Err(code) = result { self.shared.send(Control::Failed { id, code })?; }
                },
                control = controls.recv() => self.control(control.ok_or(Error::Closed)?)?,
                snapshot = self.provider.changed(self.generation), if !self.shared.caps.offer.is_empty() => {
                    self.advertise(snapshot?)?;
                },
            }
        }
    }
    fn control(&mut self, control: Control) -> Result<(), Error> {
        match control {
            Control::Offer(offer) => {
                offer.validate(&self.shared.caps.receive, &self.shared.caps.limits)?;
                if offer.id <= self.last_offer {
                    return Err(rds_core::clipboard::Error::Identity.into());
                }
                self.last_offer = offer.id;
                self.offers.send_replace(Some(offer));
            }
            Control::Withdraw { offer } => {
                if offer == 0 || offer > self.last_offer {
                    return Err(rds_core::clipboard::Error::Identity.into());
                }
                if self
                    .offers
                    .borrow()
                    .as_ref()
                    .is_some_and(|current| current.id == offer)
                {
                    self.offers.send_replace(None);
                }
            }
            Control::Request(request) => self.request(request)?,
            Control::Failed { id, code } => {
                self.shared.identity(id)?;
                if self.shared.side.owns_request(id.request) {
                    self.shared.streams.refused(id, code)?;
                } else if let Some(cancel) = self.cancels.get(&id) {
                    cancel.cancel();
                }
            }
            Control::Cancel { id } => {
                self.shared.identity(id)?;
                if !self.shared.side.peer().owns_request(id.request) {
                    return Err(rds_core::clipboard::Error::Identity.into());
                }
                if let Some(cancel) = self.cancels.get(&id) {
                    cancel.cancel();
                }
            }
            Control::Receipt(receipt) => {
                self.shared.identity(receipt.id)?;
                if !self.shared.side.peer().owns_request(receipt.id.request) {
                    return Err(rds_core::clipboard::Error::Identity.into());
                }
                // A receipt is observation, never admission for another read.
                tracing::trace!(id=?receipt.id, published=receipt.published, "native clipboard receipt observed");
            }
        }
        Ok(())
    }
    fn request(&mut self, request: Request) -> Result<(), Error> {
        self.shared.identity(request.id)?;
        if !self.shared.side.peer().owns_request(request.id.request) {
            return Err(rds_core::clipboard::Error::Identity.into());
        }
        request.validate(&self.shared.caps.limits)?;
        let refused = |code| {
            self.shared.send(Control::Failed {
                id: request.id,
                code,
            })
        };
        if request.id.request <= self.last_request {
            return refused(Failure::InvalidRequest);
        }
        self.last_request = request.id.request;
        let Some(offer) = &self.local else {
            return refused(Failure::Expired);
        };
        if request.id.offer != offer.id {
            return refused(Failure::Expired);
        }
        let PayloadKind::Representation(format) = request.kind else {
            return refused(Failure::Unsupported);
        };
        let Some(representation) = offer
            .representations
            .iter()
            .find(|value| value.format == format)
        else {
            return refused(Failure::Unsupported);
        };
        if request.bytes != representation.bytes {
            return refused(Failure::InvalidRequest);
        }
        if self.jobs.len() >= self.shared.caps.limits.parallel_transfers as usize {
            return refused(Failure::Unavailable);
        }
        let cancel = CancellationToken::new();
        self.cancels.insert(request.id, cancel.clone());
        let generation = offer.generation;
        let shared = self.shared.clone();
        let provider = self.provider.clone();
        self.jobs.spawn(async move {
            let id = request.id;
            let result = tokio::select! { biased;
                _ = shared.stop.cancelled() => Err(Failure::Canceled),
                _ = cancel.cancelled() => Err(Failure::Canceled),
                result = respond(&shared.streams, provider.as_ref(), generation, format, &request) => result,
            };
            (id, result)
        });
        Ok(())
    }
}

fn metadata_ended(result: Option<Result<Result<(), Error>, tokio::task::JoinError>>) -> Error {
    match result {
        Some(Ok(Err(error))) => error,
        Some(Ok(Ok(()))) | None => Error::Closed,
        Some(Err(_)) => Error::Task,
    }
}

async fn respond(
    streams: &Streams,
    provider: &dyn Provider,
    generation: u64,
    format: Format,
    request: &Request,
) -> Result<(), Failure> {
    let response = streams.response(request).map_err(body_failure)?;
    let value = provider
        .read(generation, format, response.original())
        .await
        .map_err(native_failure)?;
    if value.format() != format {
        return Err(Failure::Unsupported);
    }
    let bytes = match value {
        Value::Text(text) => text.encoded().clone(),
        Value::Image(image) => image.encoded().clone(),
        Value::Files(_) => return Err(Failure::Unsupported),
    };
    response.send_bytes(bytes).await.map_err(body_failure)?;
    Ok(())
}
fn native_failure(error: native::Error) -> Failure {
    match error {
        native::Error::Changed => Failure::Expired,
        native::Error::Unsupported => Failure::Unsupported,
        native::Error::Publication => Failure::NativePublication,
        native::Error::Integrity | native::Error::Image(_) => Failure::Integrity,
        native::Error::Contract(rds_core::clipboard::Error::TooLarge) => Failure::TooLarge,
        native::Error::Contract(_) => Failure::InvalidRequest,
        native::Error::Settings | native::Error::Unavailable => Failure::Unavailable,
        native::Error::File(_) | native::Error::Io(_) | native::Error::Configuration(_) => {
            Failure::Io
        }
    }
}
fn body_failure(error: transport::Error) -> Failure {
    match error {
        transport::Error::Busy => Failure::Unavailable,
        transport::Error::Closed => Failure::Canceled,
        transport::Error::PeerRefused(code) => code,
        transport::Error::Native(error) => native_failure(error),
        transport::Error::Contract(rds_core::clipboard::Error::TooLarge) => Failure::TooLarge,
        transport::Error::Contract(_) => Failure::InvalidRequest,
        transport::Error::Body(_) => Failure::Integrity,
        transport::Error::Transport(_) => Failure::Io,
    }
}

struct Sender(SendStream);
impl Drop for Sender {
    fn drop(&mut self) {
        let _ = self.0.reset(0u32.into());
    }
}
struct Receiver(RecvStream);
impl Drop for Receiver {
    fn drop(&mut self) {
        let _ = self.0.stop(0u32.into());
    }
}
async fn write_metadata(
    connection: Connection,
    desktop: [u8; 16],
    mut metadata: mpsc::Receiver<Control>,
    policy: PeerPolicy,
) -> Result<(), Error> {
    let mut wire = tokio::time::timeout(Duration::from_millis(policy.startup_timeout_ms), async {
        let mut wire = Sender(connection.open_uni().await.map_err(io::Error::other)?);
        wire.0
            .set_priority(rds_net::wire::CONTROL_STREAM_PRIORITY)
            .map_err(io::Error::other)?;
        write_frame(&mut wire.0, &UniHello::ClipboardMetadata { id: desktop }).await?;
        Ok::<_, Error>(wire)
    })
    .await
    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    while let Some(control) = metadata.recv().await {
        tokio::time::timeout(
            Duration::from_millis(policy.write_timeout_ms),
            write_frame(&mut wire.0, &control),
        )
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    }
    Ok(())
}
async fn read_metadata(
    mut inbox: UniStreams,
    controls: mpsc::Sender<Control>,
    startup: u64,
) -> Result<(), Error> {
    let wire = tokio::time::timeout(Duration::from_millis(startup), inbox.recv())
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
        .ok_or(Error::Closed)?;
    drop(inbox);
    let mut wire = Receiver(wire);
    loop {
        // This sole reader owns partial framing until the entire peer closes.
        let control = read_frame(&mut wire.0).await?;
        controls.send(control).await.map_err(|_| Error::Closed)?;
    }
}
