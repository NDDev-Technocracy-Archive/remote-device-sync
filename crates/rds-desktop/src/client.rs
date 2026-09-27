//! Viewing side of a desktop session.
//!
//! `DesktopSession` owns the control stream (input + encoder steering +
//! heartbeats) and a receiver of decoded frames. Frame streams that arrive
//! stale are reset immediately, so a slow link degrades to lower effective
//! fps rather than queueing latency; when the decode queue is full the
//! *oldest* queued frame is evicted — newest wins.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rds_core::{
    Codec, DesktopCaps, DesktopControl, DesktopEvent, DesktopHello, FrameHeader, HelloAck,
    InputEvent, InputKind, StreamHello,
};
use rds_net::{Connection, read_frame, write_frame};
use tokio::sync::{Semaphore, SemaphorePermit, mpsc};
use tokio::task::JoinSet;

use crate::{DesktopError, RawFrame, SessionClock, mailbox};

/// Largest frame payload accepted off the wire. A real H.264 frame is
/// orders of magnitude smaller; the bound exists so a hostile or
/// broken peer cannot grow the receive buffer without limit.
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_FRAME_READERS: usize = 4;
const GLOBAL_FRAME_READERS: usize = 8;
static FRAME_SLOTS: Semaphore = Semaphore::const_new(GLOBAL_FRAME_READERS);
// A canceled caller cannot interrupt an already running native decoder. Keep
// its permit inside the blocking closure so repeated sessions cannot bypass it.
static DECODE_SLOTS: Semaphore = Semaphore::const_new(2);

/// Minimum interval between decode-failure IDR requests — a corrupt
/// stretch must not turn into an IDR storm.
const IDR_MIN_INTERVAL_MS: u64 = 500;

/// Bound on waiting for a frame stream's header or body, and on the
/// session handshake ack — a peer that opens a tagged stream and
/// stalls mid-send would otherwise park a task per stream.
const FRAME_STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Decode state owned by one session. Codec reference frames are chain
/// state: a decoder shared across sessions would cross-contaminate
/// streams, so it lives here and is dropped with the session.
struct Delivery {
    #[cfg(feature = "x11")]
    decoder: Option<crate::H264Decoder>,
    /// Session-clock ms of the last decode-failure IDR request.
    last_idr_req_ms: Option<u64>,
    waiting_keyframe: bool,
}

/// What `Delivery::decode` made of one payload.
enum DecodeOutcome {
    /// A frame came out of the decoder (x11 build only — headless
    /// builds have no decoder).
    #[cfg(feature = "x11")]
    Decoded(RawFrame),
    /// Decoder buffered the input without producing a frame — happens
    /// on the first frames of a chain; not an error.
    Buffered,
    /// Payload could not be decoded — the reference chain is broken
    /// until the next keyframe.
    Failed,
}

impl Delivery {
    fn new() -> Self {
        Self {
            #[cfg(feature = "x11")]
            decoder: None,
            last_idr_req_ms: None,
            waiting_keyframe: true,
        }
    }

    fn invalidate(&mut self) {
        self.waiting_keyframe = true;
    }

    fn request_idr(&mut self, ctrl: &mpsc::Sender<DesktopControl>, now_ms: u64) {
        if self
            .last_idr_req_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= IDR_MIN_INTERVAL_MS)
            && ctrl.try_send(DesktopControl::RequestIdr).is_ok()
        {
            self.last_idr_req_ms = Some(now_ms);
        }
    }

    fn decode(&mut self, header: &FrameHeader, body: Vec<u8>) -> DecodeOutcome {
        if self.waiting_keyframe {
            if !header.keyframe {
                return DecodeOutcome::Failed;
            }
            // Recreate the reference chain on the blocking worker, not in
            // the async receive loop that detected the gap.
            #[cfg(feature = "x11")]
            {
                self.decoder = None;
            }
        }
        self.waiting_keyframe = false;
        #[cfg(feature = "x11")]
        {
            use crate::Decoder;
            if body.is_empty() {
                return DecodeOutcome::Failed;
            }
            if self.decoder.is_none() {
                match crate::H264Decoder::new() {
                    Ok(d) => self.decoder = Some(d),
                    Err(e) => {
                        tracing::warn!("decoder init failed: {e}");
                        return DecodeOutcome::Failed;
                    }
                }
            }
            let encoded = crate::EncodedFrame {
                codec: header.codec,
                data: bytes::Bytes::from(body),
                keyframe: header.keyframe,
            };
            match self.decoder.as_mut().unwrap().decode(&encoded) {
                Ok(Some(raw)) if raw.width == header.width && raw.height == header.height => {
                    tracing::debug!(
                        "frame seq={} {}x{} decoded",
                        header.seq,
                        raw.width,
                        raw.height
                    );
                    DecodeOutcome::Decoded(raw)
                }
                Ok(Some(_)) => DecodeOutcome::Failed,
                Ok(None) => {
                    tracing::debug!("frame seq={} buffered", header.seq);
                    DecodeOutcome::Buffered
                }
                Err(e) => {
                    tracing::debug!("decode seq={} failed: {e}", header.seq);
                    DecodeOutcome::Failed
                }
            }
        }
        #[cfg(not(feature = "x11"))]
        {
            let _ = header;
            // No decoder in this build: a non-empty payload is simply
            // consumed; an empty one still marks a broken delivery and
            // deserves the IDR resync below.
            if body.is_empty() {
                DecodeOutcome::Failed
            } else {
                DecodeOutcome::Buffered
            }
        }
    }
}

/// A live desktop session on the client side.
pub struct DesktopSession {
    /// Decoded frames in arrival order; stale ones never arrive here.
    /// Bounded, newest-wins: a full queue evicts the oldest frame.
    pub frames: mailbox::Receiver<RawFrame>,
    /// Headers of every frame that survived the stale-drop check and
    /// arrived complete — the wire-level tap tests and tooling use to
    /// measure latency without a decoder.
    pub frame_headers: mailbox::Receiver<FrameHeader>,
    /// Server→client control events (input acks, heartbeat echoes).
    pub events: mailbox::Receiver<DesktopEvent>,
    /// Send input or encoder control to the serving side.
    ctrl_tx: mpsc::Sender<DesktopControl>,
    /// Next expected frame sequence — the lowest seq still accepted.
    next_seq: Arc<AtomicU64>,
    /// Next input-event sequence number (ack correlation).
    input_seq: AtomicU64,
    /// Next heartbeat sequence number.
    heartbeat_seq: AtomicU64,
    /// Latest control-plane RTT measured via heartbeat echo (ms).
    control_rtt_ms: Arc<AtomicU64>,
    caps: DesktopCaps,
    display: u32,
    clock: SessionClock,
    receiving: Arc<AtomicUsize>,
    /// Owns every asynchronous session task, including frame readers.
    task: tokio::task::JoinHandle<()>,
}

impl Drop for DesktopSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Encoded receive work only; excludes transport buffers and decoded images.
#[derive(Debug, Clone, Copy)]
pub struct ReceiveStats {
    pub in_flight: usize,
    pub max_in_flight: usize,
    pub global_in_flight: usize,
    pub global_max_in_flight: usize,
    pub max_payload_bytes: usize,
}

/// Reset an abandoned control write, including a canceled handshake.
struct ControlSend(rds_net::SendStream);
impl Drop for ControlSend {
    fn drop(&mut self) {
        let _ = self.0.reset(0u32.into());
    }
}

struct FrameBudget {
    _slot: SemaphorePermit<'static>,
    receiving: Arc<AtomicUsize>,
}
impl Drop for FrameBudget {
    fn drop(&mut self) {
        self.receiving.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Options for [`DesktopSession::connect_opts`].
#[derive(Default)]
pub struct SessionOpts {
    /// Override the session clock. Tests pass the same `SessionClock`
    /// to both peers so `FrameHeader` timestamps compare directly to
    /// local receive times; production leaves each side on its own
    /// clock (cross-machine latency then needs an RTT estimate).
    pub clock: Option<SessionClock>,
    /// Per-session frame-route ID. `Some(id)` opens a `DesktopV2`
    /// session: the greeting echoes `id` and the viewer claims
    /// `UniHello::DesktopFrames { id }`, so a delayed frame stream from
    /// an ended session can never reach this session's inbox. `None`
    /// keeps the legacy shared `Desktop` route for old peers.
    pub session: Option<[u8; 16]>,
}

impl DesktopSession {
    /// Open a session on an established connection. Mints a random
    /// session ID and opens the `DesktopV2` per-session frame route; an
    /// agent that cannot decode the greeting refuses before any session
    /// work. `connect_opts` with `session: None` keeps the legacy shared
    /// route for peers that predate `DesktopV2`.
    pub async fn connect(
        conn: &Connection,
        display: u32,
        max_fps: u32,
        codec: Codec,
    ) -> Result<Self, DesktopError> {
        Self::connect_opts(
            conn,
            DesktopHello {
                display,
                max_fps,
                codec,
                input_acks: false,
            },
            SessionOpts {
                session: Some(rand::random()),
                ..Default::default()
            },
        )
        .await
    }

    /// Open a session with the full hello (input acks) and options.
    pub async fn connect_opts(
        conn: &Connection,
        hello: DesktopHello,
        opts: SessionOpts,
    ) -> Result<Self, DesktopError> {
        let display = hello.display;
        let clock = opts.clock.unwrap_or_default();
        let route = opts
            .session
            .map(|session| rds_core::UniHello::DesktopFrames { id: session })
            .unwrap_or(rds_core::UniHello::Desktop);
        // Claim before any wire I/O or spawned work. A duplicate claim must
        // not start another remote session and then leak local tasks on error.
        let uni = conn
            .uni_streams(route)
            .map_err(|e| DesktopError::Io(std::io::Error::other(e.to_string())))?;
        let greeting = match opts.session {
            Some(session) => StreamHello::DesktopV2 { session, hello },
            None => StreamHello::Desktop(hello),
        };
        let (mut send, mut recv, caps) = tokio::time::timeout(FRAME_STREAM_TIMEOUT, async {
            let (send, mut recv) = conn.open_bi().await?;
            let mut send = ControlSend(send);
            write_frame(&mut send.0, &greeting).await?;
            let caps = match read_frame::<_, HelloAck>(&mut recv).await? {
                HelloAck::Desktop(caps) => caps,
                HelloAck::Ok => DesktopCaps {
                    displays: vec![],
                    codecs: vec![],
                },
                HelloAck::Error { message } => return Err(DesktopError::Capture(message)),
                HelloAck::Info(_) => return Err(DesktopError::Capture("unexpected ack".into())),
            };
            Ok((send, recv, caps))
        })
        .await
        .map_err(|_| DesktopError::Capture("session handshake timed out".into()))??;

        let (frame_tx, frames) = mailbox::channel(4);
        let (header_tx, frame_headers) = mailbox::channel(64);
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<DesktopControl>(64);
        let (events_tx, events) = mailbox::channel::<DesktopEvent>(128);
        let next_seq = Arc::new(AtomicU64::new(0));
        // `u64::MAX` = "not measured": a loopback heartbeat can
        // legitimately round-trip in 0 ms, so 0 cannot be the sentinel.
        let control_rtt_ms = Arc::new(AtomicU64::new(u64::MAX));

        let receiving = Arc::new(AtomicUsize::new(0));
        // Construct ownership before spawning. Dropping the supervisor aborts
        // its JoinSet, which in turn drops streams and the frame-reader JoinSet.
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            while let Some(msg) = ctrl_rx.recv().await {
                if !matches!(
                    tokio::time::timeout(FRAME_STREAM_TIMEOUT, write_frame(&mut send.0, &msg))
                        .await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
        });

        let rtt_marker = control_rtt_ms.clone();
        let event_clock = clock.clone();
        tasks.spawn(async move {
            loop {
                match read_frame::<_, DesktopEvent>(&mut recv).await {
                    Ok(ev @ DesktopEvent::Heartbeat { ts_ms, .. }) => {
                        rtt_marker.store(
                            event_clock.now_ms().saturating_sub(ts_ms),
                            Ordering::Relaxed,
                        );
                        events_tx.send(ev);
                    }
                    Ok(ev) => {
                        events_tx.send(ev);
                    }
                    Err(_) => break,
                }
            }
        });
        tasks.spawn(receive_frames(
            uni,
            ReceiveContext {
                frame_tx,
                header_tx,
                next_seq: next_seq.clone(),
                ctrl: ctrl_tx.clone(),
                clock: clock.clone(),
                receiving: receiving.clone(),
            },
        ));
        let task = tokio::spawn(async move {
            // EOF/error on any session leg ends all sibling work even while
            // the underlying connection remains available for other services.
            let _ = tasks.join_next().await;
            tasks.shutdown().await;
        });

        Ok(Self {
            frames,
            frame_headers,
            events,
            ctrl_tx,
            next_seq,
            input_seq: AtomicU64::new(0),
            heartbeat_seq: AtomicU64::new(0),
            control_rtt_ms,
            caps,
            display,
            clock,
            receiving,
            task,
        })
    }

    pub fn receive_stats(&self) -> ReceiveStats {
        ReceiveStats {
            in_flight: self.receiving.load(Ordering::Relaxed),
            max_in_flight: MAX_FRAME_READERS,
            global_in_flight: GLOBAL_FRAME_READERS - FRAME_SLOTS.available_permits(),
            global_max_in_flight: GLOBAL_FRAME_READERS,
            max_payload_bytes: MAX_FRAME_BYTES,
        }
    }

    pub fn caps(&self) -> &DesktopCaps {
        &self.caps
    }

    /// Highest frame sequence fully received so far.
    pub fn latest_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed).saturating_sub(1)
    }

    /// Latest control-plane round trip measured by heartbeat echo.
    /// `None` until the first heartbeat returns.
    pub fn control_rtt(&self) -> Option<std::time::Duration> {
        match self.control_rtt_ms.load(Ordering::Relaxed) {
            u64::MAX => None,
            ms => Some(std::time::Duration::from_millis(ms)),
        }
    }

    /// Queue one input event for the serving side. Sequence, timestamp
    /// and target display are filled in from session state.
    pub async fn send_input(&self, kind: InputKind) -> Result<u64, DesktopError> {
        let seq = next_control_seq(&self.input_seq)?;
        let event = InputEvent {
            seq,
            event_ts_ms: self.clock.now_ms(),
            display_id: self.display,
            kind,
        };
        self.ctrl_tx
            .send(DesktopControl::Input(event))
            .await
            .map_err(|_| DesktopError::Input("control channel closed".into()))?;
        Ok(seq)
    }

    /// Liveness probe: the server echoes it; `control_rtt()` reflects
    /// the round trip once the echo lands.
    pub async fn heartbeat(&self) -> Result<u64, DesktopError> {
        let seq = next_control_seq(&self.heartbeat_seq)?;
        self.ctrl_tx
            .send(DesktopControl::Heartbeat {
                seq,
                ts_ms: self.clock.now_ms(),
            })
            .await
            .map_err(|_| DesktopError::Input("control channel closed".into()))?;
        Ok(seq)
    }

    /// Ask the encoder for a fresh keyframe.
    pub async fn request_idr(&self) -> Result<(), DesktopError> {
        self.ctrl_tx
            .send(DesktopControl::RequestIdr)
            .await
            .map_err(|_| DesktopError::Input("control channel closed".into()))
    }

    /// Request an encoder bitrate in bits per second.
    pub async fn set_bitrate(&self, bps: u32) -> Result<(), DesktopError> {
        self.ctrl_tx
            .send(DesktopControl::SetBitrate(bps))
            .await
            .map_err(|_| DesktopError::Input("control channel closed".into()))
    }
}

fn next_control_seq(sequence: &AtomicU64) -> Result<u64, DesktopError> {
    sequence
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |seq| {
            seq.checked_add(1)
        })
        .map_err(|_| DesktopError::Input("control sequence exhausted".into()))
}

struct ReceiveContext {
    frame_tx: mailbox::Sender<RawFrame>,
    header_tx: mailbox::Sender<FrameHeader>,
    next_seq: Arc<AtomicU64>,
    ctrl: mpsc::Sender<DesktopControl>,
    clock: SessionClock,
    receiving: Arc<AtomicUsize>,
}

async fn receive_frames(mut uni: rds_net::UniStreams, ctx: ReceiveContext) {
    let mut readers: JoinSet<Option<(FrameHeader, Vec<u8>, FrameBudget)>> = JoinSet::new();
    // Keep the decoded queue open for the session even in a headless build.
    let _frames = &ctx.frame_tx;
    let mut delivery = Delivery::new();
    loop {
        tokio::select! {
            biased;
            frame = readers.join_next(), if !readers.is_empty() => {
                let Some(Ok(Some((header, body, budget)))) = frame else { continue; };
                let expected = ctx.next_seq.load(Ordering::Relaxed);
                if header.seq < expected { continue; }
                if header.seq > expected {
                    delivery.invalidate();
                    delivery.request_idr(&ctx.ctrl, ctx.clock.now_ms());
                }
                // read_one rejects u64::MAX before publishing anything.
                ctx.next_seq.store(header.seq + 1, Ordering::Relaxed);
                ctx.header_tx.send(header.clone());
                let Ok(slot) = DECODE_SLOTS.acquire().await else { break; };
                // No control/queue/connection handles escape into native work.
                // An already running call may finish after cancellation, but
                // it cannot publish and holds both permits until it returns.
                let decoded = tokio::task::spawn_blocking(move || {
                    let (_slot, _budget) = (slot, budget);
                    let result = delivery.decode(&header, body);
                    (delivery, result)
                }).await;
                let Ok((state, outcome)) = decoded else { break; };
                delivery = state;
                match outcome {
                    #[cfg(feature = "x11")]
                    DecodeOutcome::Decoded(raw) => { ctx.frame_tx.send(raw); }
                    DecodeOutcome::Buffered => {}
                    DecodeOutcome::Failed => {
                        delivery.invalidate();
                        delivery.request_idr(&ctx.ctrl, ctx.clock.now_ms());
                    }
                }
            }
            stream = uni.recv() => {
                let Some(stream) = stream else { break; };
                // Refuse excess work immediately; don't create parked tasks
                // or read a large body before obtaining its memory budget.
                if readers.len() >= MAX_FRAME_READERS { continue; }
                let Ok(slot) = FRAME_SLOTS.try_acquire() else { continue; };
                ctx.receiving.fetch_add(1, Ordering::Relaxed);
                let budget = FrameBudget { _slot: slot, receiving: ctx.receiving.clone() };
                let next_seq = ctx.next_seq.clone();
                readers.spawn(async move {
                    tokio::time::timeout(FRAME_STREAM_TIMEOUT, read_one(stream, next_seq, budget))
                        .await.ok().flatten()
                });
            }
        }
    }
    readers.shutdown().await;
}

async fn read_one(
    mut stream: rds_net::RecvStream,
    next_seq: Arc<AtomicU64>,
    budget: FrameBudget,
) -> Option<(FrameHeader, Vec<u8>, FrameBudget)> {
    let header: FrameHeader = read_frame(&mut stream).await.ok()?;
    if header.seq == u64::MAX
        || header.seq < next_seq.load(Ordering::Relaxed)
        || crate::frame_bytes(header.width as usize, header.height as usize).is_none()
    {
        return None;
    }
    let body = stream.read_to_end(MAX_FRAME_BYTES).await.ok()?;
    Some((header, body, budget))
}

/// Headless desktop run: connects, prints capabilities, streams decode
/// stats until interrupted. A GUI front-end consumes `session.frames`.
pub async fn run_desktop_client(
    conn: Connection,
    display: u32,
    max_fps: u32,
) -> Result<(), DesktopError> {
    let mut session = DesktopSession::connect(&conn, display, max_fps, Codec::H264).await?;
    println!("desktop caps: {:?}", session.caps());
    let mut count = 0u64;
    let start = std::time::Instant::now();
    while let Some(frame) = session.frames.recv().await {
        count += 1;
        if count.is_multiple_of(30) {
            let secs = start.elapsed().as_secs_f64();
            println!(
                "decoded {count} frames, {:.1} fps, last {}x{}",
                count as f64 / secs,
                frame.width,
                frame.height
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_control_sequences_never_wrap() {
        let sequence = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_control_seq(&sequence).unwrap(), u64::MAX - 1);
        assert!(next_control_seq(&sequence).is_err());
        assert!(next_control_seq(&sequence).is_err());
        assert_eq!(sequence.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn resync_requests_start_immediately_and_share_one_rate_limit() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut delivery = Delivery::new();
        delivery.request_idr(&tx, 0);
        assert!(matches!(rx.try_recv(), Ok(DesktopControl::RequestIdr)));
        for now in [0, 1, 499] {
            delivery.invalidate();
            delivery.request_idr(&tx, now);
        }
        assert!(rx.try_recv().is_err());
        delivery.request_idr(&tx, 500);
        assert!(matches!(rx.try_recv(), Ok(DesktopControl::RequestIdr)));
    }

    #[test]
    fn decoded_dimensions_are_bounded_before_bgra_allocation() {
        assert_eq!(crate::frame_bytes(7680, 4320), Some(7680 * 4320 * 4));
        for (w, h) in [(0, 1), (1, 0), (8192, 8192), (usize::MAX, 1)] {
            assert!(crate::frame_bytes(w, h).is_none());
        }
    }

    #[cfg(feature = "x11")]
    #[test]
    fn native_decode_resyncs_at_keyframe_and_checks_header_dimensions() {
        use crate::Encoder;
        let mut encoder = crate::H264Encoder::new(1_000_000, 30.0).unwrap();
        let raw = RawFrame {
            width: 64,
            height: 64,
            stride: 256,
            data: bytes::Bytes::from(vec![80; 64 * 64 * 4]),
        };
        let encoded = encoder.encode(&raw).unwrap();
        assert!(encoded.keyframe);
        let mut header = FrameHeader {
            seq: 0,
            capture_ts_ms: 0,
            encode_done_ts_ms: 0,
            send_ts_ms: 0,
            keyframe: true,
            codec: Codec::H264,
            width: 64,
            height: 64,
        };
        let mut delivery = Delivery::new();
        assert!(matches!(
            delivery.decode(&header, encoded.data.to_vec()),
            DecodeOutcome::Decoded(_)
        ));
        delivery.invalidate();
        header.keyframe = false;
        assert!(matches!(
            delivery.decode(&header, encoded.data.to_vec()),
            DecodeOutcome::Failed
        ));
        assert!(delivery.waiting_keyframe);
        header.keyframe = true;
        assert!(matches!(
            delivery.decode(&header, encoded.data.to_vec()),
            DecodeOutcome::Decoded(_)
        ));
        header.width = 128;
        delivery.invalidate();
        assert!(matches!(
            delivery.decode(&header, encoded.data.to_vec()),
            DecodeOutcome::Failed
        ));
    }
}
