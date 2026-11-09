//! Managed desktop channel: the session manager holds the remote
//! `DesktopSession` and relays encoded frames and control events over IPC.
//! The viewer decodes — the manager never links a codec.
//!
//! Wire shape mirrors the remote one: postcard `DesktopDown`/`DesktopUp`
//! messages via `write_frame`, and each `Frame` header followed by a
//! big-endian u32 length plus raw encoded payload (≤ `MAX_DESKTOP_PAYLOAD`).

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rds_core::local::{DesktopDown, DesktopUp, MAX_DESKTOP_PAYLOAD, SessionId};
use rds_core::{DesktopCaps, DesktopControl, FrameHeader, InputEvent, InputKind};
use rds_net::{read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid local desktop body")
}

/// Metadata-only observation of the IPC-to-network queue boundary. Never log
/// key codes, pointer coordinates, clipboard bodies or complete controls.
async fn forward_control(
    ctrl: &tokio::sync::mpsc::Sender<DesktopControl>,
    control: DesktopControl,
) -> io::Result<()> {
    let input_seq = match &control {
        DesktopControl::Input(event) => Some(event.seq),
        _ => None,
    };
    let started = Instant::now();
    if let Some(input_seq) = input_seq {
        tracing::trace!(target:"rds_desktop::input_timing", input_seq,
            "managed desktop input received from IPC");
    }
    ctrl.send(control)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "control closed"))?;
    if let Some(input_seq) = input_seq {
        tracing::trace!(target:"rds_desktop::input_timing", input_seq,
            queue_us=started.elapsed().as_micros(), "managed desktop input queued for network");
    }
    Ok(())
}

/// Write one encoded payload with its u32 length prefix.
async fn write_payload<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "payload too large"))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await
}

/// A slow video writer cannot hold remote input acknowledgements or heartbeat
/// echoes. All retained legs end with the desktop owner or event subscriber.
pub(super) async fn serve_separated(
    stream: &mut UnixStream,
    session: rds_desktop::client::DesktopSession,
    registration: super::desktop_events::Registration,
) -> io::Result<()> {
    let (reader, writer) = stream.split();
    serve_separated_io(reader, writer, session, registration).await
}

async fn serve_separated_io<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    mut session: rds_desktop::client::DesktopSession,
    mut registration: super::desktop_events::Registration,
) -> io::Result<()> {
    let ctrl = session.control_sender();
    let encoded = session.encoded.as_mut().ok_or_else(invalid)?;
    let up = async {
        loop {
            match read_frame::<_, DesktopUp>(&mut reader).await {
                Ok(DesktopUp::Control(DesktopControl::FrameReceived { .. })) => {
                    return Err(invalid());
                }
                Ok(DesktopUp::Control(control)) => forward_control(&ctrl, control).await?,
                Ok(DesktopUp::Finished) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    };
    tokio::pin!(up);
    // Keep this read alive across the attachment fence: canceling a pending
    // framed read and continuing it later would lose its partial parse state.
    let event_stop = registration.stop.clone();
    tokio::select! {
        result = &mut up => return result,
        result = registration.attached() => result?,
        _ = event_stop.cancelled() => return Ok(()),
    }
    let media = async {
        while let Some(frame) = encoded.recv().await {
            if frame.payload.len() > MAX_DESKTOP_PAYLOAD {
                return Err(invalid());
            }
            write_frame(
                &mut writer,
                &DesktopDown::Frame {
                    header: frame.header,
                },
            )
            .await?;
            write_payload(&mut writer, &frame.payload).await?;
        }
        Ok(())
    };
    let events = async {
        while let Some(event) = session.events.recv().await {
            registration.sender.try_send(event).map_err(|_| {
                // Metadata only; never discard an ACK and keep a healthy status.
                tracing::warn!("desktop event subscriber unavailable or full");
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "desktop event subscriber unavailable or full",
                )
            })?;
        }
        Ok(())
    };
    tokio::select! {
        biased;
        _ = registration.stop.cancelled() => Ok(()),
        result = up => result,
        result = events => result,
        result = media => result,
    }
}

/// Read one u32-length-prefixed encoded payload, bounded like the remote
/// frame reader.
async fn read_payload<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len as usize > MAX_DESKTOP_PAYLOAD {
        return Err(invalid());
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

/// Agent side: pump the pinned remote session's encoded frames and control
/// events down the socket, and viewer controls up, until either side ends.
/// Caller EOF, viewer `Finished`, or remote-session end all close the
/// channel; returning drops the session and aborts its remote legs.
pub(super) async fn serve(
    stream: &mut UnixStream,
    mut session: rds_desktop::client::DesktopSession,
) -> io::Result<()> {
    // A relay must be opened with `relay_encoded`; otherwise there is
    // nothing to forward — fail rather than park the channel forever.
    let ctrl = session.control_sender();
    if session.encoded.is_none() {
        return Err(invalid());
    }
    let (mut reader, mut writer) = stream.split();

    let up = async {
        loop {
            match read_frame::<_, DesktopUp>(&mut reader).await {
                Ok(DesktopUp::Control(DesktopControl::FrameReceived { .. })) => {
                    return Err(invalid());
                }
                Ok(DesktopUp::Control(control)) => forward_control(&ctrl, control).await?,
                Ok(DesktopUp::Finished) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    };
    // Borrows `session`'s receivers for the pump's lifetime; when `serve`
    // returns the session drops and aborts its remote legs.
    let down = async {
        let encoded = session.encoded.as_mut().unwrap();
        let events = &mut session.events;
        // Each tap's liveness is tracked separately: `recv` answers `None`
        // immediately and repeatedly once its senders are gone, so a shared
        // counter would end the pump while the surviving tap is still live.
        let (mut enc_open, mut ev_open) = (true, true);
        while enc_open || ev_open {
            tokio::select! {
                frame = encoded.recv(), if enc_open => match frame {
                    Some(f) => {
                        if f.payload.len() > MAX_DESKTOP_PAYLOAD {
                            return Err(invalid());
                        }
                        write_frame(&mut writer, &DesktopDown::Frame { header: f.header }).await?;
                        write_payload(&mut writer, &f.payload).await?;
                    }
                    None => enc_open = false,
                },
                event = events.recv(), if ev_open => match event {
                    Some(e) => write_frame(&mut writer, &DesktopDown::Event(e)).await?,
                    None => ev_open = false,
                },
            }
        }
        write_frame(&mut writer, &DesktopDown::Finished).await
    };
    tokio::select! {
        result = up => result,
        result = down => result,
    }
}

/// One relayed encoded frame as the viewer receives it.
#[derive(Debug)]
pub struct RelayedFrame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

/// One message off a managed desktop channel.
#[derive(Debug)]
pub enum ManagedMessage {
    /// One encoded frame — feed a `rds_desktop::client::RelayDecoder`.
    Frame(RelayedFrame),
    /// A control-plane event (input acks, heartbeat echoes).
    Event(rds_core::DesktopEvent),
}

/// Dedicated event receive queue; drop the owning desktop to abort its reader.
pub type ManagedEvents = tokio::sync::mpsc::Receiver<io::Result<rds_core::DesktopEvent>>;

/// Shared control-plane state behind `ManagedDesktop`/`ManagedControl`:
/// the socket's write half and the viewer-side sequence counters.
/// Serializing writes through the mutex keeps postcard frames atomic.
struct ControlState {
    writer: Mutex<OwnedWriteHalf>,
    display: u32,
    input_seq: AtomicU64,
    heartbeat_seq: AtomicU64,
    /// Base for locally-stamped event timestamps (the viewer's own clock).
    started: Instant,
}

impl ControlState {
    async fn write_control(&self, control: DesktopControl) -> io::Result<()> {
        let mut writer = self.writer.lock().await;
        write_frame(&mut *writer, &DesktopUp::Control(control)).await
    }
}

/// Cloneable control half of a managed desktop channel — the analogue of
/// `DesktopSession::control_sender`. Input tasks hold one while the owner
/// keeps receiving frames.
#[derive(Clone)]
pub struct ManagedControl {
    state: Arc<ControlState>,
}

impl ManagedControl {
    /// Send a fully-formed control message verbatim — callers that manage
    /// their own sequencing use this; everyone else prefers the typed
    /// helpers.
    pub async fn control(&self, control: DesktopControl) -> io::Result<()> {
        self.state.write_control(control).await
    }

    /// Queue one input event; sequence, timestamp and target display are
    /// filled in from channel state, mirroring `DesktopSession::send_input`.
    pub async fn send_input(&self, kind: InputKind) -> io::Result<u64> {
        let seq = self
            .state
            .input_seq
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |s| s.checked_add(1))
            .map_err(|_| io::Error::other("input sequence exhausted"))?;
        self.control(DesktopControl::Input(InputEvent {
            seq,
            event_ts_ms: self.state.started.elapsed().as_millis() as u64,
            display_id: self.state.display,
            kind,
        }))
        .await?;
        Ok(seq)
    }

    /// Liveness probe; the remote echoes it as `DesktopEvent::Heartbeat`.
    pub async fn heartbeat(&self) -> io::Result<u64> {
        let seq = self
            .state
            .heartbeat_seq
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |s| s.checked_add(1))
            .map_err(|_| io::Error::other("heartbeat sequence exhausted"))?;
        self.control(DesktopControl::Heartbeat {
            seq,
            ts_ms: self.state.started.elapsed().as_millis() as u64,
        })
        .await?;
        Ok(seq)
    }

    /// Ask the remote encoder for a fresh keyframe.
    pub async fn request_idr(&self) -> io::Result<()> {
        self.control(DesktopControl::RequestIdr).await
    }

    /// Request an encoder bitrate in bits per second.
    pub async fn set_bitrate(&self, bps: u32) -> io::Result<()> {
        self.control(DesktopControl::SetBitrate(bps)).await
    }
}

/// Client handle on a managed desktop channel. Owns the authenticated IPC
/// socket; `None` from `recv` means the remote session ended cleanly or the
/// manager went away.
pub struct ManagedDesktop {
    messages: tokio::sync::mpsc::Receiver<io::Result<Option<ManagedMessage>>>,
    reading: tokio::task::JoinHandle<()>,
    event_reading: Option<tokio::task::JoinHandle<()>>,
    state: Arc<ControlState>,
    /// The manager session this channel is pinned to.
    pub session: SessionId,
    /// Negotiated capabilities the remote agent reported.
    pub caps: DesktopCaps,
}

impl Drop for ManagedDesktop {
    fn drop(&mut self) {
        self.reading.abort();
        if let Some(reading) = &self.event_reading {
            reading.abort();
        }
    }
}

async fn read_message(reader: &mut OwnedReadHalf) -> io::Result<Option<ManagedMessage>> {
    match read_frame::<_, DesktopDown>(reader).await {
        Ok(DesktopDown::Frame { header }) => {
            let payload = read_payload(reader).await?;
            Ok(Some(ManagedMessage::Frame(RelayedFrame {
                header,
                payload,
            })))
        }
        Ok(DesktopDown::Event(event)) => Ok(Some(ManagedMessage::Event(event))),
        Ok(DesktopDown::Finished) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

impl std::fmt::Debug for ManagedDesktop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedDesktop")
            .field("session", &self.session)
            .field("caps", &self.caps)
            .finish_non_exhaustive()
    }
}

impl ManagedDesktop {
    pub(super) fn new(
        stream: UnixStream,
        session: SessionId,
        caps: DesktopCaps,
        display: u32,
    ) -> Self {
        let (mut reader, writer) = stream.into_split();
        // read_exact/framing is not cancellation safe. One owned reader
        // retains its parse state independently of a caller's select branch.
        // At most one message is queued and one is being read (bounded payload).
        let (tx, messages) = tokio::sync::mpsc::channel(1);
        let reading = tokio::spawn(async move {
            loop {
                let message = read_message(&mut reader).await;
                let done = !matches!(&message, Ok(Some(_)));
                if tx.send(message).await.is_err() || done {
                    break;
                }
            }
        });
        Self {
            messages,
            reading,
            event_reading: None,
            state: Arc::new(ControlState {
                writer: Mutex::new(writer),
                display,
                input_seq: AtomicU64::new(0),
                heartbeat_seq: AtomicU64::new(0),
                started: Instant::now(),
            }),
            session,
            caps,
        }
    }

    pub(super) fn separated(
        stream: UnixStream,
        mut events: UnixStream,
        session: SessionId,
        caps: DesktopCaps,
        display: u32,
    ) -> (Self, ManagedEvents) {
        let mut desktop = Self::new(stream, session, caps, display);
        let (sender, receiver) = tokio::sync::mpsc::channel(super::desktop_events::EVENT_CAPACITY);
        desktop.event_reading = Some(tokio::spawn(async move {
            // Own the whole event socket: dropping its write half would look
            // like caller departure to the same-UID event service.
            loop {
                let event = match read_frame::<_, DesktopDown>(&mut events).await {
                    Ok(DesktopDown::Event(event)) => Ok(event),
                    Ok(DesktopDown::Finished) => break,
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                    Ok(DesktopDown::Frame { .. }) => Err(invalid()),
                    Err(e) => Err(e),
                };
                let failed = event.is_err();
                if sender.send(event).await.is_err() || failed {
                    break;
                }
            }
        }));
        (desktop, receiver)
    }

    /// A cloneable control half for tasks that send while `recv` runs —
    /// mirroring `DesktopSession::control_sender`.
    pub fn control_handle(&self) -> ManagedControl {
        ManagedControl {
            state: Arc::clone(&self.state),
        }
    }

    /// Next manager→viewer message; `None` after the channel ends. EOF
    /// between messages means the manager went away — also `None` — while
    /// EOF inside a frame payload stays an error. Canceling this wait never
    /// discards a partially consumed header/body; Drop aborts the owned reader.
    pub async fn recv(&mut self) -> io::Result<Option<ManagedMessage>> {
        self.messages.recv().await.unwrap_or(Ok(None))
    }

    /// Shorthand for `control_handle().send_input(...)`.
    pub async fn send_input(&self, kind: InputKind) -> io::Result<u64> {
        self.control_handle().send_input(kind).await
    }

    /// Shorthand for `control_handle().heartbeat()`.
    pub async fn heartbeat(&self) -> io::Result<u64> {
        self.control_handle().heartbeat().await
    }

    /// Shorthand for `control_handle().request_idr()`.
    pub async fn request_idr(&self) -> io::Result<()> {
        self.control_handle().request_idr().await
    }

    /// Shorthand for `control_handle().set_bitrate(...)`.
    pub async fn set_bitrate(&self, bps: u32) -> io::Result<()> {
        self.control_handle().set_bitrate(bps).await
    }

    /// Announce a clean end to the manager; dropping without it is also a
    /// clean end (caller EOF). A `ManagedControl` that outlives this handle
    /// can still buffer writes until the manager closes its side, then
    /// sees an error.
    pub async fn finish(self) -> io::Result<()> {
        let mut writer = self.state.writer.lock().await;
        write_frame(&mut *writer, &DesktopUp::Finished).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(seq: u64) -> FrameHeader {
        FrameHeader {
            seq,
            capture_ts_ms: 1,
            encode_done_ts_ms: 2,
            send_ts_ms: 3,
            keyframe: seq == 0,
            codec: rds_core::Codec::H264,
            width: 640,
            height: 480,
        }
    }

    fn pair() -> (ManagedDesktop, UnixStream) {
        let (viewer, peer) = UnixStream::pair().unwrap();
        let caps = DesktopCaps {
            displays: vec![],
            codecs: vec![rds_core::Codec::H264],
        };
        (
            ManagedDesktop::new(viewer, SessionId([0x11; 16]), caps, 0),
            peer,
        )
    }

    #[tokio::test]
    async fn canceled_receive_preserves_partial_header_and_payload() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &DesktopDown::Frame { header: header(7) })
            .await
            .unwrap();
        let header_length = bytes.len();
        let body = vec![0xff; 257];
        write_payload(&mut bytes, &body).await.unwrap();
        for split in [
            1,
            3,
            4,
            header_length - 1,
            header_length + 1,
            header_length + 4 + 129,
        ] {
            let (mut channel, mut peer) = pair();
            peer.write_all(&bytes[..split]).await.unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(5), channel.recv())
                    .await
                    .is_err()
            );
            peer.write_all(&bytes[split..]).await.unwrap();
            let message = tokio::time::timeout(std::time::Duration::from_secs(1), channel.recv())
                .await
                .unwrap()
                .unwrap();
            let Some(ManagedMessage::Frame(frame)) = message else {
                panic!("canceled read lost the frame boundary");
            };
            assert_eq!(frame.header.seq, 7);
            assert_eq!(frame.payload, body);
        }
    }

    #[tokio::test]
    async fn payload_round_trip_and_bounds() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        write_payload(&mut a, b"encoded-payload").await.unwrap();
        assert_eq!(read_payload(&mut b).await.unwrap(), b"encoded-payload");
        write_payload(&mut a, &[]).await.unwrap();
        assert!(read_payload(&mut b).await.unwrap().is_empty());
        // A length prefix above the cap fails before the body is read.
        a.write_u32((MAX_DESKTOP_PAYLOAD + 1) as u32).await.unwrap();
        assert_eq!(
            read_payload(&mut b).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // A truncated payload is an error, not a partial frame.
        a.write_u32(8).await.unwrap();
        a.write_all(b"abc").await.unwrap();
        drop(a);
        assert!(read_payload(&mut b).await.is_err());
    }

    #[tokio::test]
    async fn recv_reads_frame_event_finished_and_eof() {
        let (mut channel, mut peer) = pair();
        let frame = header(0);
        write_frame(&mut peer, &DesktopDown::Frame { header: frame })
            .await
            .unwrap();
        write_payload(&mut peer, b"h264").await.unwrap();
        write_frame(
            &mut peer,
            &DesktopDown::Event(rds_core::DesktopEvent::Heartbeat { seq: 4, ts_ms: 9 }),
        )
        .await
        .unwrap();
        write_frame(&mut peer, &DesktopDown::Finished)
            .await
            .unwrap();
        match channel.recv().await.unwrap() {
            Some(ManagedMessage::Frame(f)) => {
                assert_eq!(f.header.seq, 0);
                assert_eq!(f.payload, b"h264");
            }
            other => panic!("expected frame, got {other:?}"),
        }
        match channel.recv().await.unwrap() {
            Some(ManagedMessage::Event(rds_core::DesktopEvent::Heartbeat { seq, .. })) => {
                assert_eq!(seq, 4)
            }
            other => panic!("expected event, got {other:?}"),
        }
        assert!(channel.recv().await.unwrap().is_none());
        // After Finished the peer is done; a bare EOF is also a clean end.
        let (mut channel, peer) = pair();
        drop(peer);
        assert!(channel.recv().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn recv_propagates_truncated_payload() {
        let (mut channel, mut peer) = pair();
        write_frame(&mut peer, &DesktopDown::Frame { header: header(1) })
            .await
            .unwrap();
        peer.write_u32(16).await.unwrap();
        peer.write_all(b"short").await.unwrap();
        drop(peer);
        // EOF inside a payload is corruption, not a clean end.
        assert!(channel.recv().await.is_err());
    }

    #[tokio::test]
    async fn finish_and_controls_reach_the_manager() {
        let (channel, mut peer) = pair();
        let control = channel.control_handle();
        control.request_idr().await.unwrap();
        control.set_bitrate(2_000_000).await.unwrap();
        match read_frame::<_, DesktopUp>(&mut peer).await.unwrap() {
            DesktopUp::Control(DesktopControl::RequestIdr) => {}
            other => panic!("expected RequestIdr, got {other:?}"),
        }
        match read_frame::<_, DesktopUp>(&mut peer).await.unwrap() {
            DesktopUp::Control(DesktopControl::SetBitrate(bps)) => {
                assert_eq!(bps, 2_000_000)
            }
            other => panic!("expected SetBitrate, got {other:?}"),
        }
        channel.finish().await.unwrap();
        match read_frame::<_, DesktopUp>(&mut peer).await.unwrap() {
            DesktopUp::Finished => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        // A control handle outliving the channel errors once the peer is
        // gone; a live peer would still accept buffered writes.
        drop(peer);
        assert!(control.request_idr().await.is_err());
    }

    #[tokio::test]
    async fn control_serializes_concurrent_senders() {
        let (channel, mut peer) = pair();
        let a = channel.control_handle();
        let b = channel.control_handle();
        let first = tokio::spawn(async move {
            for _ in 0..20 {
                a.request_idr().await.unwrap();
            }
        });
        for _ in 0..20 {
            b.request_idr().await.unwrap();
        }
        first.await.unwrap();
        // Interleaved writers must still yield 40 well-formed frames.
        for _ in 0..40 {
            match read_frame::<_, DesktopUp>(&mut peer).await.unwrap() {
                DesktopUp::Control(DesktopControl::RequestIdr) => {}
                other => panic!("expected RequestIdr, got {other:?}"),
            }
        }
    }

    /// Synthetic input sink — `serve_desktop_with` probes the host's real
    /// backend when `input_sink` is `None`, which does not exist headless.
    struct NoopInput;
    impl rds_desktop::InputSink for NoopInput {
        fn inject(&mut self, _: &InputEvent) -> Result<(), rds_desktop::DesktopError> {
            Ok(())
        }
    }

    /// Loopback QUIC endpoints plus a synthetic-frame serving side —
    /// the same shape `session_v2` uses, small enough for a unit file.
    /// The endpoints stay owned by the returned guard: an endpoint drop
    /// closes its connections, so the session dies with it otherwise.
    struct Relay {
        session: rds_desktop::client::DesktopSession,
        server_task: tokio::task::JoinHandle<()>,
        _server_ep: rds_net::Endpoint,
        _client_ep: rds_net::Endpoint,
    }

    async fn loopback_relay() -> Relay {
        loopback_relay_with_payload(1024).await
    }

    async fn loopback_relay_with_payload(payload_bytes: usize) -> Relay {
        use rds_core::{HelloAck, StreamHello, UniHello};
        use rds_desktop::{SessionConfig, SyntheticProducer, serve_desktop_with};

        let config = || rds_net::EndpointConfig {
            bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
            discovery: false,
            ..Default::default()
        };
        let server_ep = rds_net::bind_endpoint(config()).await.unwrap();
        let client_ep = rds_net::bind_endpoint(config()).await.unwrap();
        let target = server_ep.addr();
        let server_ep_guard = server_ep.clone();
        let server_task = tokio::spawn(async move {
            let conn = server_ep.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let (hello, route) = match read_frame::<_, StreamHello>(&mut recv).await.unwrap() {
                StreamHello::DesktopV2 { session, hello } => {
                    (hello, Some(UniHello::DesktopFrames { id: session }))
                }
                StreamHello::Desktop(hello) => (hello, None),
                other => panic!("unexpected hello {other:?}"),
            };
            write_frame(
                &mut send,
                &HelloAck::Desktop(DesktopCaps {
                    displays: vec![],
                    codecs: vec![rds_core::Codec::H264],
                }),
            )
            .await
            .unwrap();
            serve_desktop_with(
                conn,
                send,
                recv,
                hello,
                SessionConfig {
                    input_sink: Some(Box::new(NoopInput)),
                    producer: Some(Box::new(
                        SyntheticProducer::new(30, 640, 480, payload_bytes).keyframe_every(5),
                    )),
                    frame_route: route,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        });
        let conn = client_ep.connect(target, rds_core::ALPN).await.unwrap();
        let session = rds_desktop::client::DesktopSession::connect_opts(
            &conn,
            rds_core::DesktopHello {
                display: 0,
                max_fps: 30,
                codec: rds_core::Codec::H264,
                input_acks: false,
            },
            rds_desktop::client::SessionOpts {
                session: Some(rand::random()),
                relay_encoded: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        Relay {
            session,
            server_task,
            _server_ep: server_ep_guard,
            _client_ep: client_ep,
        }
    }

    /// Read one down-message plus its raw payload when it is a frame.
    async fn recv_down(viewer: &mut UnixStream) -> io::Result<DesktopDown> {
        match read_frame::<_, DesktopDown>(viewer).await? {
            down @ DesktopDown::Frame { .. } => {
                read_payload(viewer).await?;
                Ok(down)
            }
            down => Ok(down),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_relay_publishes_encoded() {
        let mut relay = loopback_relay().await;
        let encoded = relay.session.encoded.as_mut().expect("relay tap");
        let delivery = tokio::time::timeout(std::time::Duration::from_secs(10), encoded.recv())
            .await
            .expect("no encoded frame")
            .expect("encoded tap closed");
        assert!(!delivery.payload.is_empty());
        // Frame headers publish on the shared tap in relay mode too.
        let header = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            relay.session.frame_headers.recv(),
        )
        .await
        .expect("no frame header")
        .expect("header tap closed");
        assert_eq!(header.seq, delivery.header.seq);
        relay.server_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_relays_frames_echoes_controls_and_finishes() {
        let relay = loopback_relay().await;
        let (mut ipc, mut viewer) = UnixStream::pair().unwrap();
        let pump = tokio::spawn(async move { serve(&mut ipc, relay.session).await });
        // Encoded frames arrive as postcard header + raw payload.
        for _ in 0..8 {
            match recv_down(&mut viewer).await.unwrap() {
                DesktopDown::Frame { .. } => {}
                other => panic!("expected frame, got {other:?}"),
            }
        }
        // A viewer control crosses the pump into the remote session and
        // its heartbeat echo comes back down.
        write_frame(
            &mut viewer,
            &DesktopUp::Control(DesktopControl::Heartbeat { seq: 77, ts_ms: 5 }),
        )
        .await
        .unwrap();
        loop {
            match recv_down(&mut viewer).await.unwrap() {
                DesktopDown::Event(rds_core::DesktopEvent::Heartbeat { seq, .. }) => {
                    assert_eq!(seq, 77);
                    break;
                }
                DesktopDown::Frame { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        write_frame(&mut viewer, &DesktopUp::Finished)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), pump)
            .await
            .expect("pump did not stop on Finished")
            .unwrap()
            .unwrap();
        relay.server_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_ends_when_remote_session_drops() {
        let relay = loopback_relay().await;
        let (mut ipc, mut viewer) = UnixStream::pair().unwrap();
        let pump = tokio::spawn(async move { serve(&mut ipc, relay.session).await });
        // Read a frame so the pump is mid-stream when the remote dies.
        match recv_down(&mut viewer).await.unwrap() {
            DesktopDown::Frame { .. } => {}
            other => panic!("expected frame, got {other:?}"),
        }
        relay.server_task.abort();
        // Remote conn closure ends both taps; the viewer sees Finished.
        loop {
            match recv_down(&mut viewer).await.unwrap() {
                DesktopDown::Finished => break,
                DesktopDown::Frame { .. } | DesktopDown::Event(_) => {}
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), pump)
            .await
            .expect("pump did not stop on remote end")
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_ends_on_caller_eof() {
        let relay = loopback_relay().await;
        let (mut ipc, viewer) = UnixStream::pair().unwrap();
        let pump = tokio::spawn(async move { serve(&mut ipc, relay.session).await });
        drop(viewer);
        tokio::time::timeout(std::time::Duration::from_secs(5), pump)
            .await
            .expect("pump did not stop on caller EOF")
            .unwrap()
            .unwrap();
        relay.server_task.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn separate_events_progress_while_video_body_is_not_read() {
        use super::super::desktop_events;
        use std::time::Duration;
        let relay = loopback_relay_with_payload(1024 * 1024).await;
        let registry = Arc::new(desktop_events::Registry::default());
        let registration = registry.register().unwrap();
        let route = registration.route;
        let subscription = registry.take(route).unwrap();
        let stop = registration.stop.clone();
        let (video_server, mut video_client) = tokio::io::duplex(64);
        let (mut event_server, mut event_client) = UnixStream::pair().unwrap();
        let events =
            tokio::spawn(
                async move { desktop_events::serve(&mut event_server, subscription).await },
            );
        let media = tokio::spawn(async move {
            {
                let (read, write) = tokio::io::split(video_server);
                serve_separated_io(read, write, relay.session, registration).await
            }
        });
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            read_frame::<_, DesktopDown>(&mut video_client),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(first, DesktopDown::Frame { .. }));
        let bytes = video_client.read_u32().await.unwrap();
        assert!(
            bytes > 64 && bytes <= 1024 * 1024,
            "body must exceed the deterministic video capacity"
        );
        // Retain the whole large body unread. On the old single socket, an
        // event cannot be parsed before draining exactly these payload bytes.
        for seq in 1..=8 {
            write_frame(
                &mut video_client,
                &DesktopUp::Control(DesktopControl::Heartbeat {
                    seq,
                    ts_ms: seq + 100,
                }),
            )
            .await
            .unwrap();
            let event = tokio::time::timeout(
                Duration::from_secs(2),
                read_frame::<_, DesktopDown>(&mut event_client),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                matches!(event, DesktopDown::Event(rds_core::DesktopEvent::Heartbeat { seq: echoed, ts_ms }) if echoed == seq && ts_ms == seq + 100)
            );
            assert!(!media.is_finished(), "video owner unexpectedly ended");
        }
        // Event subscriber departure ends its own blocked video writer.
        drop(event_client);
        tokio::time::timeout(Duration::from_secs(2), events)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), media)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(stop.is_cancelled());
        relay.server_task.abort();
    }

    #[tokio::test]
    async fn separated_readers_keep_events_independent_of_full_media_queue() {
        let (video, mut peer) = UnixStream::pair().unwrap();
        let (events, mut event_peer) = UnixStream::pair().unwrap();
        let caps = DesktopCaps {
            displays: vec![],
            codecs: vec![rds_core::Codec::H264],
        };
        let (mut channel, mut events) =
            ManagedDesktop::separated(video, events, SessionId([7; 16]), caps, 0);
        // Force the FIFO reader's existing one-message channel to fill and
        // block. Independent event parsing must remain available.
        for seq in 0..3 {
            write_frame(
                &mut peer,
                &DesktopDown::Frame {
                    header: header(seq),
                },
            )
            .await
            .unwrap();
            write_payload(&mut peer, &[0xff; 16]).await.unwrap();
        }
        write_frame(
            &mut event_peer,
            &DesktopDown::Event(rds_core::DesktopEvent::InputAck {
                seq: 77,
                handled_ts_ms: 0,
            }),
        )
        .await
        .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            rds_core::DesktopEvent::InputAck { seq: 77, .. }
        ));
        for seq in 0..3 {
            assert!(
                matches!(channel.recv().await.unwrap(), Some(ManagedMessage::Frame(frame)) if frame.header.seq == seq)
            );
        }
        drop(channel);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn separated_client_uses_authenticated_manager_and_retains_shared_peer() {
        use super::super::{Client, Prepared, Server};
        use rds_core::{
            HelloAck, StreamHello, UniHello,
            local::{Command, Reply},
        };
        use std::{os::unix::fs::DirBuilderExt, time::Duration};
        let backends = {
            #[cfg(feature = "transport-noq")]
            {
                vec![rds_net::Backend::Iroh, rds_net::Backend::Noq]
            }
            #[cfg(not(feature = "transport-noq"))]
            {
                vec![rds_net::Backend::Iroh]
            }
        };
        for (backend, payload_receipts, clipboard) in backends.into_iter().flat_map(|backend| {
            [
                (backend, false, false),
                (backend, true, false),
                (backend, false, true),
                (backend, true, true),
            ]
        }) {
            let config = || rds_net::EndpointConfig {
                backend,
                bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
                discovery: false,
                ..Default::default()
            };
            let remote = rds_net::bind_endpoint(config()).await.unwrap();
            let local = rds_net::bind_endpoint(config()).await.unwrap();
            let remote_task = tokio::spawn({
                let remote = remote.clone();
                async move {
                    let conn = remote.accept().await.unwrap().await.unwrap();
                    let mut workers = tokio::task::JoinSet::new();
                    loop {
                        tokio::select! {
                            Some(result) = workers.join_next(), if !workers.is_empty() => { result.unwrap(); },
                            streams = conn.accept_bi() => {
                                let (mut send, mut recv) = match streams { Ok(pair) => pair, Err(_) => break };
                                let conn = conn.clone();
                                workers.spawn(async move {
                                    let greeting = read_frame::<_, StreamHello>(&mut recv).await.unwrap();
                                    let negotiated = matches!(&greeting,StreamHello::DesktopV4 { .. } | StreamHello::DesktopV5 { payload_receipts: true, .. });
                                    match greeting {
                                        StreamHello::Ping { nonce } => {
                                            write_frame(&mut send, &HelloAck::Ok).await.unwrap();
                                            send.write_all(&nonce.to_be_bytes()).await.unwrap();
                                            send.finish().unwrap();
                                        }
                                        StreamHello::DesktopV3 { session, hello, output_height } | StreamHello::DesktopV4 { session, hello, output_height } => {
                                            assert_eq!(negotiated,payload_receipts);
                                            assert_eq!(output_height, 1080);
                                            let caps=DesktopCaps { displays: vec![], codecs: vec![rds_core::Codec::H264] };
                                            write_frame(&mut send, &if negotiated { HelloAck::DesktopV4(caps) } else { HelloAck::Desktop(caps) }).await.unwrap();
                                            rds_desktop::serve_desktop_with(conn, send, recv, hello, rds_desktop::SessionConfig {
                                                frame_route: Some(UniHello::DesktopFrames { id: session }),
                                                payload_receipts: negotiated,
                                                input_sink: Some(Box::new(NoopInput)),
                                                producer: Some(Box::new(rds_desktop::SyntheticProducer::new(30, 640, 480, 1024))),
                                                ..Default::default()
                                            }).await.unwrap();
                                        }
                                        StreamHello::DesktopV5 { session, hello, output_height, payload_receipts: v5_receipts, clipboard } => {
                                            assert!(clipboard);
                                            assert_eq!(v5_receipts, payload_receipts);
                                            assert_eq!(output_height, 1080);
                                            let caps=DesktopCaps { displays: vec![], codecs: vec![rds_core::Codec::H264] };
                                            write_frame(&mut send, &HelloAck::DesktopV5(caps)).await.unwrap();
                                            rds_desktop::serve_desktop_with(conn, send, recv, hello, rds_desktop::SessionConfig {
                                                frame_route: Some(UniHello::DesktopFrames { id: session }),
                                                clipboard: rds_desktop::ServingClipboard::Text { reverse: true },
                                                payload_receipts: v5_receipts,
                                                input_sink: Some(Box::new(NoopInput)),
                                                producer: Some(Box::new(rds_desktop::SyntheticProducer::new(30, 640, 480, 1024))),
                                                ..Default::default()
                                            }).await.unwrap();
                                        }
                                        other => panic!("unexpected service {other:?}"),
                                    }
                                });
                            }
                        }
                    }
                    workers.shutdown().await;
                }
            });
            let root = std::path::Path::new("/tmp")
                .canonicalize()
                .unwrap()
                .join(format!(
                    "rds-event-route-{}-{}",
                    std::process::id(),
                    rand::random::<u64>()
                ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .unwrap();
            let mut manager = Server::start(
                Some(Prepared::bind(&root).await.unwrap()),
                local.clone(),
                None,
            );
            let client = Client::new(&root);
            let Reply::Connected(id) = client
                .request(Command::Connect {
                    target: rds_net::Ticket::of(&remote).to_string(),
                    grant: None,
                })
                .await
                .unwrap()
            else {
                panic!("missing session")
            };
            let hello = rds_core::DesktopHello {
                display: 0,
                max_fps: 30,
                codec: rds_core::Codec::H264,
                input_acks: true,
            };
            let (mut desktop, mut events) = if clipboard {
                client
                    .desktop_features(Some(id), hello, 1080, payload_receipts, true)
                    .await
                    .unwrap()
            } else if payload_receipts {
                client
                    .desktop_profile_separated_receipts(Some(id), hello, 1080)
                    .await
                    .unwrap()
            } else {
                client
                    .desktop_profile_separated(Some(id), hello, 1080)
                    .await
                    .unwrap()
            };
            for _ in 0..5 {
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(2), desktop.recv())
                        .await
                        .unwrap()
                        .unwrap(),
                    Some(ManagedMessage::Frame(_))
                ));
            }
            let controls = desktop.control_handle();
            let seq = controls
                .send_input(InputKind::KeyDown { code: 30 })
                .await
                .unwrap();
            let ack = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(matches!(ack, rds_core::DesktopEvent::InputAck { seq: ack, .. } if ack == seq));
            controls.heartbeat().await.unwrap();
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(2), events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                rds_core::DesktopEvent::Heartbeat { .. }
            ));
            drop(desktop);
            assert!(
                tokio::time::timeout(Duration::from_secs(2), events.recv())
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                matches!(client.request(Command::Ping { session: Some(id), nonce: 91 }).await.unwrap(), Reply::Pong { session, .. } if session == id)
            );
            manager.close().await.unwrap();
            local.close().await;
            remote.close().await;
            tokio::time::timeout(Duration::from_secs(2), remote_task)
                .await
                .unwrap()
                .unwrap();
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
