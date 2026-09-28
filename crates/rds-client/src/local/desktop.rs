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

/// Write one encoded payload with its u32 length prefix.
async fn write_payload<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "payload too large"))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await
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
                Ok(DesktopUp::Control(control)) => {
                    if let Err(e) = ctrl.send(control).await {
                        eprintln!("dbg: up: session control closed: {e}");
                        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "control closed"));
                    }
                }
                Ok(DesktopUp::Finished) => {
                    eprintln!("dbg: up: viewer Finished");
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    eprintln!("dbg: up: viewer EOF");
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("dbg: up: read error: {e}");
                    return Err(e);
                }
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
                            eprintln!("dbg: down: frame over payload cap");
                            return Err(invalid());
                        }
                        if f.header.seq == 0 {
                            eprintln!("dbg: down: first frame seq=0");
                        }
                        write_frame(&mut writer, &DesktopDown::Frame { header: f.header }).await?;
                        write_payload(&mut writer, &f.payload).await?;
                    }
                    None => {
                        eprintln!("dbg: down: encoded tap ended");
                        enc_open = false;
                    }
                },
                event = events.recv(), if ev_open => match event {
                    Some(e) => write_frame(&mut writer, &DesktopDown::Event(e)).await?,
                    None => ev_open = false,
                },
            }
        }
        eprintln!("dbg: down: finished, sending Finished");
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
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |s| s.checked_add(1))
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
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |s| s.checked_add(1))
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
    reader: OwnedReadHalf,
    state: Arc<ControlState>,
    /// The manager session this channel is pinned to.
    pub session: SessionId,
    /// Negotiated capabilities the remote agent reported.
    pub caps: DesktopCaps,
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
        let (reader, writer) = stream.into_split();
        Self {
            reader,
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

    /// A cloneable control half for tasks that send while `recv` runs —
    /// mirroring `DesktopSession::control_sender`.
    pub fn control_handle(&self) -> ManagedControl {
        ManagedControl {
            state: Arc::clone(&self.state),
        }
    }

    /// Next manager→viewer message; `None` after the channel ends. EOF
    /// between messages means the manager went away — also `None` — while
    /// EOF inside a frame payload stays an error.
    pub async fn recv(&mut self) -> io::Result<Option<ManagedMessage>> {
        match read_frame::<_, DesktopDown>(&mut self.reader).await {
            Ok(DesktopDown::Frame { header }) => {
                let payload = read_payload(&mut self.reader).await?;
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
                        SyntheticProducer::new(30, 640, 480, 1024).keyframe_every(5),
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
}
