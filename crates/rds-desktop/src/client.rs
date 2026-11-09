//! Viewing side of a desktop session.
//!
//! `DesktopSession` owns the control stream (input + encoder steering +
//! heartbeats) and a receiver of decoded frames. Frame streams that arrive
//! stale are reset immediately, so a slow link degrades to lower effective
//! fps rather than queueing latency; when the decode queue is full the
//! *oldest* queued decoded frame is evicted — newest wins. Compressed
//! relay frames instead use bounded backpressure to preserve references.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rds_core::{
    Codec, DesktopCaps, DesktopControl, DesktopEvent, DesktopHello, FrameHeader, HelloAck,
    InputEvent, InputKind, StreamHello,
};
use rds_net::{Connection, read_frame, write_frame};
use tokio::sync::{Semaphore, SemaphorePermit, mpsc};
use tokio::task::JoinSet;
use tracing::Instrument;

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
// A realtime frame cannot reserve a reader for a whole request deadline.
// Even a high-resolution keyframe must finish promptly or release its budget
// for fresh IDR recovery. Control/handshake policy remains independent.
const FRAME_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const KEYFRAME_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

fn reorder_wait(rtt_ms: u64) -> std::time::Duration {
    // A missing stream tag may itself be in transit. Two observed round trips
    // tolerate WAN reordering; the cap keeps a truly missing reference bounded.
    let ms = if rtt_ms == u64::MAX {
        250
    } else {
        rtt_ms.saturating_mul(2).clamp(100, 1000)
    };
    std::time::Duration::from_millis(ms)
}

#[derive(Default)]
struct HeartbeatProbes {
    pending: std::collections::VecDeque<(u64, u64, std::time::Instant)>,
    last_echoed_sent: Option<std::time::Instant>,
}

impl HeartbeatProbes {
    fn sent(&mut self, seq: u64, ts_ms: u64) {
        // Caller timestamps are opaque correlation values. In managed mode
        // they belong to the viewer, whose clock survives session reconnects.
        self.pending.retain(|(s, t, _)| (*s, *t) != (seq, ts_ms));
        if self.pending.len() >= 64 {
            self.pending.pop_front();
        }
        self.pending
            .push_back((seq, ts_ms, std::time::Instant::now()));
    }

    fn echoed(&mut self, seq: u64, ts_ms: u64) -> Option<std::time::Duration> {
        let index = self
            .pending
            .iter()
            .position(|(s, t, _)| (*s, *t) == (seq, ts_ms))?;
        let (_, _, sent) = self.pending.remove(index)?;
        self.last_echoed_sent = Some(self.last_echoed_sent.map_or(sent, |old| old.max(sent)));
        Some(sent.elapsed())
    }

    fn observation(&self) -> crate::control_read::PendingReplies {
        // A matched newer probe proves progress beyond an older missing echo.
        // Retain ordinary correlation behavior, but do not diagnose healthy
        // idle periods as stalled solely because that old entry remains.
        let mut count = 0;
        let mut oldest = None;
        for (seq, _, sent) in &self.pending {
            if self
                .last_echoed_sent
                .is_none_or(|confirmed| *sent > confirmed)
            {
                count += 1;
                if oldest.is_none_or(|(_, at)| *sent < at) {
                    oldest = Some((*seq, *sent));
                }
            }
        }
        crate::control_read::PendingReplies {
            count,
            oldest_seq: oldest.map(|(seq, _)| seq),
            oldest_age: oldest.map(|(_, at)| at.elapsed()),
        }
    }
}

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

    fn request_idr(&mut self, ctrl: &mpsc::Sender<DesktopControl>, now_ms: u64) -> bool {
        if self
            .last_idr_req_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= IDR_MIN_INTERVAL_MS)
            && ctrl.try_send(DesktopControl::RequestIdr).is_ok()
        {
            self.last_idr_req_ms = Some(now_ms);
            return true;
        }
        false
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
            let started = std::time::Instant::now();
            let outcome = self.decoder.as_mut().unwrap().decode(&encoded);
            tracing::trace!(target:"rds_desktop::frame_timing", frame_seq=header.seq,
                decode_us=started.elapsed().as_micros(), payload_bytes=encoded.data.len(),
                "desktop frame decode completed");
            match outcome {
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
    /// Explicit native clipboard admission, independent of screen/input.
    pub native_clipboard: Option<crate::clipboard::peer::Handle>,
    /// Decoded frames in arrival order; stale ones never arrive here.
    /// Bounded, newest-wins: a full queue evicts the oldest frame.
    pub frames: mailbox::Receiver<RawFrame>,
    /// Headers of every frame that survived the stale-drop check and
    /// arrived complete — the wire-level tap tests and tooling use to
    /// measure latency without a decoder.
    pub frame_headers: mailbox::Receiver<FrameHeader>,
    /// Server→client control events (input acks, heartbeat echoes).
    pub events: mpsc::Receiver<DesktopEvent>,
    /// Encoded wire frames in relay mode; `None` on a direct session.
    pub encoded: Option<mpsc::Receiver<EncodedDelivery>>,
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
        if let Some(peer) = &self.native_clipboard {
            peer.close();
        }
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

/// One per-desktop lease from an already prepared viewer clipboard owner.
pub struct NativeClipboard {
    pub provider: Arc<dyn crate::clipboard::native::Provider>,
    pub admitted: rds_core::clipboard::Capabilities,
    pub policy: crate::clipboard::native::PeerPolicy,
    pub original: crate::clipboard::NativeLifetime,
}

struct NativeAdmission(Option<Arc<dyn crate::clipboard::native::Provider>>);
impl Drop for NativeAdmission {
    fn drop(&mut self) {
        if let Some(provider) = &self.0 {
            drop(provider.close());
        }
    }
}

/// Options for [`DesktopSession::connect_opts`].
#[derive(Default)]
pub struct SessionOpts {
    /// Explicit DesktopV6 mode, mutually exclusive with reverse text. This
    /// lease closes on rejected/canceled handshake or session teardown.
    pub native_clipboard: Option<NativeClipboard>,
    /// Request DesktopV4 and bounded complete-payload proofs. Older peers must
    /// reject the requested mode; no implicit legacy fallback is performed.
    pub payload_receipts: bool,
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
    /// Explicit video profile; zero means source, 16..=4320 selects height.
    pub output_height: Option<u32>,
    /// Relay mode (the local session manager): publish each encoded
    /// payload to [`DesktopSession::encoded`] instead of decoding it —
    /// no decoder is created and `frames` stays empty. Transport-level
    /// resync (IDR on sequence gaps) still runs; decode-chain discipline
    /// is the downstream viewer's job.
    pub relay_encoded: bool,
    /// Negotiate reverse text clipboard offers.  Production viewers opt in
    /// explicitly; the library default remains legacy-compatible.
    pub reverse_clipboard: bool,
}

/// One encoded frame exactly as it arrived on the wire, published in relay
/// mode for consumers that forward rather than decode.
#[derive(Debug)]
pub struct EncodedDelivery {
    pub header: FrameHeader,
    /// Annex-B encoded payload, already bounded by the receive budget.
    pub payload: bytes::Bytes,
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
        mut opts: SessionOpts,
    ) -> Result<Self, DesktopError> {
        let mut native_admission = NativeAdmission(
            opts.native_clipboard
                .as_ref()
                .map(|local| local.provider.clone()),
        );
        let display = hello.display;
        let observation =
            crate::control_observation::ControlObservation::new(conn.path_observer())?;
        let control_instance = observation.instance;
        let clock = opts.clock.unwrap_or_default();
        let route = opts
            .session
            .map(|session| rds_core::UniHello::DesktopFrames { id: session })
            .unwrap_or(rds_core::UniHello::Desktop);
        let native_caps = if let Some(local) = &opts.native_clipboard {
            local
                .policy
                .validate()
                .map_err(|error| DesktopError::Input(error.to_string()))?;
            crate::clipboard::peer::validate_admission(local.provider.as_ref(), &local.admitted)
                .map_err(|error| DesktopError::Input(error.to_string()))?;
            if opts.reverse_clipboard || opts.session.is_none_or(|id| id == [0; 16]) {
                return Err(DesktopError::Input(
                    "native clipboard requires an isolated session and excludes text mode".into(),
                ));
            }
            Some(local.admitted.clone())
        } else {
            None
        };
        let native_requested = native_caps.is_some();
        let mut native_route = if native_requested {
            Some(
                conn.uni_streams(rds_core::UniHello::ClipboardMetadata {
                    id: opts.session.expect("validated native session ID"),
                })
                .map_err(|error| DesktopError::Io(std::io::Error::other(error)))?,
            )
        } else {
            None
        };
        // Claim before any wire I/O or spawned work. A duplicate claim must
        // not start another remote session and then leak local tasks on error.
        let uni = conn
            .uni_streams(route)
            .map_err(|e| DesktopError::Io(std::io::Error::other(e.to_string())))?;
        let greeting = if let Some(clipboard) = native_caps {
            let output_height = opts.output_height.unwrap_or(0);
            if output_height != 0 && !(16..=4320).contains(&output_height) {
                return Err(DesktopError::Capture(
                    "video height must be 0 or 16..=4320".into(),
                ));
            }
            StreamHello::DesktopV6 {
                session: opts.session.expect("validated native session ID"),
                hello,
                output_height,
                payload_receipts: opts.payload_receipts,
                clipboard,
            }
        } else if opts.reverse_clipboard {
            let session = opts.session.ok_or_else(|| {
                DesktopError::Capture("reverse clipboard requires an isolated session route".into())
            })?;
            let output_height = opts.output_height.unwrap_or(0);
            if output_height != 0 && !(16..=4320).contains(&output_height) {
                return Err(DesktopError::Capture(
                    "video height must be 0 or 16..=4320".into(),
                ));
            }
            StreamHello::DesktopV5 {
                session,
                hello,
                output_height,
                payload_receipts: opts.payload_receipts,
                clipboard: true,
            }
        } else if opts.payload_receipts {
            let session = opts.session.ok_or_else(|| {
                DesktopError::Capture("payload receipts require an isolated session route".into())
            })?;
            let output_height = opts.output_height.unwrap_or(0);
            if output_height != 0 && !(16..=4320).contains(&output_height) {
                return Err(DesktopError::Capture(
                    "video height must be 0 or 16..=4320".into(),
                ));
            }
            StreamHello::DesktopV4 {
                session,
                hello,
                output_height,
            }
        } else {
            match (opts.session, opts.output_height) {
                (Some(session), Some(output_height)) => {
                    if output_height != 0 && !(16..=4320).contains(&output_height) {
                        return Err(DesktopError::Capture(
                            "video height must be 0 or 16..=4320".into(),
                        ));
                    }
                    StreamHello::DesktopV3 {
                        session,
                        hello,
                        output_height,
                    }
                }
                (Some(session), None) => StreamHello::DesktopV2 { session, hello },
                (None, None) => StreamHello::Desktop(hello),
                (None, Some(_)) => {
                    return Err(DesktopError::Capture(
                        "video profile requires an isolated session route".into(),
                    ));
                }
            }
        };
        let (mut send, recv, caps, peer_caps) = tokio::time::timeout(FRAME_STREAM_TIMEOUT, async {
            let (send, mut recv) = conn.open_bi().await?;
            let mut send = ControlSend(send);
            rds_net::wire::prioritize_control(&send.0, &greeting)?;
            write_frame(&mut send.0, &greeting).await?;
            let (caps, peer_caps) = match read_frame::<_, HelloAck>(&mut recv).await? {
                HelloAck::DesktopV6 { desktop, clipboard } if native_requested => {
                    (desktop, clipboard)
                }
                HelloAck::DesktopV5(caps) if opts.reverse_clipboard && !native_requested => {
                    (caps, None)
                }
                HelloAck::DesktopV4(caps)
                    if opts.payload_receipts && !opts.reverse_clipboard && !native_requested =>
                {
                    (caps, None)
                }
                HelloAck::Desktop(caps)
                    if !opts.payload_receipts && !opts.reverse_clipboard && !native_requested =>
                {
                    (caps, None)
                }
                HelloAck::Ok
                    if !opts.payload_receipts && !opts.reverse_clipboard && !native_requested =>
                {
                    (
                        DesktopCaps {
                            displays: vec![],
                            codecs: vec![],
                        },
                        None,
                    )
                }
                HelloAck::Error { message } => return Err(DesktopError::Capture(message)),
                _ => {
                    return Err(DesktopError::Capture(
                        "unexpected desktop receipt negotiation ack".into(),
                    ));
                }
            };
            Ok((send, recv, caps, peer_caps))
        })
        .await
        .map_err(|_| DesktopError::Capture("session handshake timed out".into()))??;

        let native_clipboard = if let Some(local) = opts.native_clipboard.take() {
            if let Some(peer) = peer_caps {
                Some(
                    crate::clipboard::peer::Prepared::claimed(
                        crate::clipboard::peer::Setup {
                            connection: conn.clone(),
                            desktop: opts.session.expect("validated native session ID"),
                            side: rds_core::clipboard::Side::Initiator,
                            provider: local.provider,
                            admitted: local.admitted,
                            peer,
                            policy: local.policy,
                            original: local.original,
                        },
                        native_route.take().expect("claimed native route"),
                    )
                    .and_then(crate::clipboard::peer::Prepared::start)
                    .map_err(|error| DesktopError::Input(error.to_string()))?,
                )
            } else {
                local.provider.close().await;
                None
            }
        } else {
            None
        };
        native_admission.0 = None;
        drop(native_route);
        let native_handle = native_clipboard
            .as_ref()
            .map(crate::clipboard::peer::Session::handle);

        let (frame_tx, frames) = mailbox::channel(4);
        let (header_tx, frame_headers) = mailbox::channel(64);
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<DesktopControl>(64);
        let (receipt_tx, mut receipt_rx) = mpsc::channel::<DesktopControl>(MAX_FRAME_READERS);
        let (events_tx, events) = mpsc::channel::<DesktopEvent>(128);
        let (encoded_tx, encoded) = match opts.relay_encoded {
            true => {
                // Compressed references must reach local decode in order. One
                // queued frame bounds IPC latency without evicting its predecessor.
                let (tx, rx) = mpsc::channel::<EncodedDelivery>(1);
                (Some(tx), Some(rx))
            }
            false => (None, None),
        };
        let next_seq = Arc::new(AtomicU64::new(0));
        // `u64::MAX` = "not measured": a loopback heartbeat can
        // legitimately round-trip in 0 ms, so 0 cannot be the sentinel.
        let control_rtt_ms = Arc::new(AtomicU64::new(u64::MAX));

        let receiving = Arc::new(AtomicUsize::new(0));
        // Construct ownership before spawning. Dropping the supervisor aborts
        // its JoinSet, which in turn drops streams and the frame-reader JoinSet.
        let mut tasks = JoinSet::new();
        let probes = Arc::new(tokio::sync::Mutex::new(HeartbeatProbes::default()));
        let sending_probes = probes.clone();
        tasks.spawn(
            async move {
                loop {
                    // Fair selection keeps both input and bounded payload proofs
                    // progressing; no video/FIFO/decode wait runs on this writer.
                    let msg = tokio::select! {
                        msg = ctrl_rx.recv() => msg,
                        msg = receipt_rx.recv(), if opts.payload_receipts => msg,
                    };
                    let Some(msg) = msg else {
                        break;
                    };
                    let heartbeat_seq = if let DesktopControl::Heartbeat { seq, ts_ms } = &msg {
                        sending_probes.lock().await.sent(*seq, *ts_ms);
                        Some(*seq)
                    } else {
                        None
                    };
                    let input_seq = match &msg {
                        DesktopControl::Input(event) => Some(event.seq),
                        _ => None,
                    };
                    let started = std::time::Instant::now();
                    if let Some(input_seq) = input_seq {
                        tracing::trace!(target:"rds_desktop::input_timing", control_instance, input_seq,
                            "desktop input control write started");
                    }
                    if let Some(heartbeat_seq) = heartbeat_seq {
                        tracing::trace!(target:"rds_desktop::control_timing", control_instance, heartbeat_seq,
                            "desktop heartbeat control write started");
                    }
                    let written = matches!(
                        tokio::time::timeout(FRAME_STREAM_TIMEOUT, write_frame(&mut send.0, &msg)).await,
                        Ok(Ok(()))
                    );
                    if let Some(heartbeat_seq) = heartbeat_seq {
                        tracing::trace!(target:"rds_desktop::control_timing", control_instance, heartbeat_seq, written,
                            write_us=started.elapsed().as_micros(), "desktop heartbeat control write completed");
                    }
                    if let Some(input_seq) = input_seq {
                        tracing::trace!(target:"rds_desktop::input_timing", control_instance, input_seq, written,
                            write_us=started.elapsed().as_micros(), "desktop input control write completed");
                    }
                    if !written {
                        tracing::warn!(target:"rds_desktop::control_timing", control_instance, input_seq, heartbeat_seq,
                            "desktop control writer ended after incomplete write");
                        break;
                    }
                }
            }
            .in_current_span(),
        );

        let rtt_marker = control_rtt_ms.clone();
        tasks.spawn(
            async move {
                let observation = observation.spawn();
                let progress = crate::control_read::ReadProgress::default();
                let observing_probes = probes.clone();
                let _reader_observer = progress.observe(control_instance, move || {
                    observing_probes.try_lock().ok().map(|p|p.observation())
                });
                let mut recv = crate::control_read::ObservedRead::new(recv, progress.clone());
                loop {
                    let read_started = std::time::Instant::now();
                    progress.begin();
                    let reply = read_frame::<_, DesktopEvent>(&mut recv).await;
                    progress.end();
                    match reply {
                        Ok(ev @ DesktopEvent::Heartbeat { seq, ts_ms }) => {
                            let rtt = probes.lock().await.echoed(seq, ts_ms);
                            tracing::trace!(target:"rds_desktop::control_timing", control_instance, heartbeat_seq=seq,
                                matched=rtt.is_some(), read_wait_us=read_started.elapsed().as_micros(),
                                rtt_ms=rtt.map(|rtt| rtt.as_millis()), "desktop heartbeat reply read");
                            if let Some(rtt) = rtt {
                                observation.heartbeat(seq, rtt);
                                rtt_marker.store(rtt.as_millis() as u64, Ordering::Relaxed);
                            }
                            if !matches!(tokio::time::timeout(FRAME_STREAM_TIMEOUT, events_tx.send(ev)).await, Ok(Ok(()))) {
                                break;
                            }
                        }
                        Ok(ev) => {
                            if !matches!(tokio::time::timeout(FRAME_STREAM_TIMEOUT, events_tx.send(ev)).await, Ok(Ok(()))) {
                                break;
                            }
                        }
                        Err(error) => {
                            tracing::debug!(target:"rds_desktop::control_timing", control_instance, error_kind=?error.kind(),
                                read_wait_us=read_started.elapsed().as_micros(), "desktop event reader ended");
                            break;
                        }
                    }
                }
            }
            .in_current_span(),
        );
        tasks.spawn(
            receive_frames(
                uni,
                ReceiveContext {
                    frame_tx,
                    header_tx,
                    encoded_tx,
                    next_seq: next_seq.clone(),
                    ctrl: ctrl_tx.clone(),
                    clock: clock.clone(),
                    receiving: receiving.clone(),
                    control_rtt_ms: control_rtt_ms.clone(),
                    receipts: opts.payload_receipts.then_some(receipt_tx),
                },
            )
            .in_current_span(),
        );
        let task = tokio::spawn(
            async move {
                // EOF/error on any session leg ends all sibling work even while
                // the underlying connection remains available for other services.
                let _ = tasks.join_next().await;
                tasks.shutdown().await;
                if let Some(peer) = native_clipboard
                    && let Err(error) = peer.close().await
                {
                    tracing::debug!(%error, "native clipboard ended during desktop shutdown");
                }
            }
            .in_current_span(),
        );

        Ok(Self {
            native_clipboard: native_handle,
            frames,
            frame_headers,
            events,
            encoded,
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

    /// Queue one fully-formed control message verbatim — callers that
    /// manage their own sequencing use this; everyone else prefers the
    /// typed helpers.
    pub async fn send_control(&self, control: DesktopControl) -> Result<(), DesktopError> {
        self.ctrl_tx
            .send(control)
            .await
            .map_err(|_| DesktopError::Input("control channel closed".into()))
    }

    /// Cloneable control-queue handle — the managed relay forwards a
    /// viewer's verbatim `DesktopControl` messages through it while the
    /// session's receivers are consumed elsewhere. Direct callers use the
    /// typed helpers (`send_input`, `heartbeat`, `request_idr`,
    /// `set_bitrate`) that fill sequence metadata in.
    pub fn control_sender(&self) -> mpsc::Sender<DesktopControl> {
        self.ctrl_tx.clone()
    }
}

/// What [`RelayDecoder::push`] made of one relayed encoded frame.
pub enum RelayOutcome {
    /// A frame came out of the decoder (decoder-enabled builds only).
    #[cfg(feature = "x11")]
    Frame(RawFrame),
    /// Consumed without producing a frame — the codec is still buffering
    /// inputs, or this build has no decoder at all.
    Pending,
    /// The reference chain is broken; the viewer should send
    /// `DesktopControl::RequestIdr` upstream.
    NeedIdr,
}

/// Viewer-side decode chain for a relayed frame channel
/// (`local::DesktopDown::Frame`): applies the same wait-for-keyframe and
/// broken-chain discipline a direct session does, while the resync request
/// stays with the caller's own control path. `NeedIdr` reports are rate
/// limited like the in-session request path, so one report covers a run
/// of broken frames.
pub struct RelayDecoder {
    delivery: Delivery,
    /// Wall clock of the last `NeedIdr` report.
    last_idr_report: Option<std::time::Instant>,
    next_seq: Option<u64>,
}

impl RelayDecoder {
    pub fn new() -> Self {
        Self {
            delivery: Delivery::new(),
            last_idr_report: None,
            next_seq: None,
        }
    }

    /// Feed one relayed encoded frame. Input must already have passed the
    /// relay's sequence checks — this owns only decode-chain state.
    pub fn push(&mut self, header: &FrameHeader, payload: Vec<u8>) -> RelayOutcome {
        if header.seq == u64::MAX || self.next_seq.is_some_and(|next| header.seq < next) {
            return RelayOutcome::Pending;
        }
        if self.next_seq.is_some_and(|next| header.seq != next) {
            self.delivery.invalidate();
        }
        self.next_seq = header.seq.checked_add(1);
        match self.delivery.decode(header, payload) {
            #[cfg(feature = "x11")]
            DecodeOutcome::Decoded(raw) => RelayOutcome::Frame(raw),
            DecodeOutcome::Buffered => RelayOutcome::Pending,
            DecodeOutcome::Failed => {
                self.delivery.invalidate();
                let now = std::time::Instant::now();
                let due = self.last_idr_report.is_none_or(|last| {
                    now.duration_since(last).as_millis() as u64 >= IDR_MIN_INTERVAL_MS
                });
                if due {
                    self.last_idr_report = Some(now);
                    RelayOutcome::NeedIdr
                } else {
                    RelayOutcome::Pending
                }
            }
        }
    }

    /// Cancellation retains the global decoder permit in native work. No
    /// connection/control handle is moved into the blocking decoder.
    pub async fn push_bounded(
        self,
        header: FrameHeader,
        payload: Vec<u8>,
    ) -> Result<(Self, RelayOutcome), DesktopError> {
        let probe = crate::decode_work::Probe::new(&header, payload.len());
        crate::decode_work::run(&DECODE_SLOTS, FRAME_STREAM_TIMEOUT, probe, move || {
            let mut decoder = self;
            let outcome = decoder.push(&header, payload);
            (decoder, outcome)
        })
        .await
        .map_err(DesktopError::from)
    }
}

impl Default for RelayDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn next_control_seq(sequence: &AtomicU64) -> Result<u64, DesktopError> {
    sequence
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |seq| {
            seq.checked_add(1)
        })
        .map_err(|_| DesktopError::Input("control sequence exhausted".into()))
}

struct ReceiveContext {
    frame_tx: mailbox::Sender<RawFrame>,
    header_tx: mailbox::Sender<FrameHeader>,
    /// Relay-mode tap: encoded payloads publish here instead of decoding.
    encoded_tx: Option<mpsc::Sender<EncodedDelivery>>,
    next_seq: Arc<AtomicU64>,
    ctrl: mpsc::Sender<DesktopControl>,
    clock: SessionClock,
    receiving: Arc<AtomicUsize>,
    control_rtt_ms: Arc<AtomicU64>,
    receipts: Option<mpsc::Sender<DesktopControl>>,
}

/// Successors of one missing reference must not queue repeated large IDRs
/// while a reliable recovery request/key is still in transit. Actual reader
/// failure or a received key ends that episode; absent either, retry within
/// the existing key-reader deadline. Failed control admission never arms it.
#[derive(Default)]
struct GapRepair {
    requested_at_ms: Option<u64>,
}
impl GapRepair {
    fn request(
        &mut self,
        delivery: &mut Delivery,
        ctrl: &mpsc::Sender<DesktopControl>,
        now_ms: u64,
    ) -> bool {
        if self
            .requested_at_ms
            .is_some_and(|at| now_ms.saturating_sub(at) < KEYFRAME_READ_TIMEOUT.as_millis() as u64)
        {
            return false;
        }
        if delivery.request_idr(ctrl, now_ms) {
            self.requested_at_ms = Some(now_ms);
            return true;
        }
        false
    }
    fn clear(&mut self) {
        self.requested_at_ms = None;
    }
}

enum FrameRead {
    Complete(FrameHeader, Vec<u8>, FrameBudget),
    Stale,
    Rejected,
    TimedOut,
}

async fn receive_frames(mut uni: rds_net::UniStreams, ctx: ReceiveContext) {
    let mut readers: JoinSet<FrameRead> = JoinSet::new();
    let mut ordered: crate::order::Ordered<(Vec<u8>, FrameBudget)> =
        crate::order::Ordered::default();
    let mut repair = tokio::time::interval(std::time::Duration::from_millis(20));
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Keep the decoded queue open for the session even in a headless build.
    let _frames = &ctx.frame_tx;
    let mut delivery = Delivery::new();
    let mut gap_repair = GapRepair::default();
    let mut admitted = 0u64;
    let mut completed = 0u64;
    let mut rejected = 0u64;
    let mut stale = 0u64;
    let mut timed_out = 0u64;
    let mut gaps = 0u64;
    let mut received_keys = 0u64;
    let mut health = std::time::Instant::now();
    loop {
        if let Some((header, (body, budget))) = ordered.pop() {
            let expected = ctx.next_seq.load(Ordering::Relaxed);
            if header.seq < expected {
                continue;
            }
            if header.seq > expected {
                delivery.invalidate();
            }
            ctx.next_seq.store(header.seq + 1, Ordering::Relaxed);
            ctx.header_tx.send(header.clone());
            completed += 1;
            if header.keyframe {
                gap_repair.clear();
                received_keys += 1;
            }
            if let Some(tx) = &ctx.encoded_tx {
                if tx
                    .send(EncodedDelivery {
                        header,
                        payload: body.into(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
            let probe = crate::decode_work::Probe::new(&header, body.len());
            let decoded =
                crate::decode_work::run(&DECODE_SLOTS, FRAME_STREAM_TIMEOUT, probe, move || {
                    let _budget = budget;
                    let result = delivery.decode(&header, body);
                    (delivery, result)
                })
                .await;
            let (state, outcome) = match decoded {
                Ok(pair) => pair,
                Err(crate::decode_work::Error::Timeout) => {
                    delivery = Delivery::new();
                    delivery.request_idr(&ctx.ctrl, ctx.clock.now_ms());
                    continue;
                }
                Err(_) => break,
            };
            delivery = state;
            match outcome {
                #[cfg(feature = "x11")]
                DecodeOutcome::Decoded(raw) => {
                    ctx.frame_tx.send(raw);
                }
                DecodeOutcome::Buffered => {}
                DecodeOutcome::Failed => {
                    delivery.invalidate();
                    delivery.request_idr(&ctx.ctrl, ctx.clock.now_ms());
                }
            }
            continue;
        }
        tokio::select! {
            biased;
            _ = repair.tick() => {
                // An admitted reference has its own bounded reader deadline.
                // Before its tag arrives, allow a measured WAN reorder window
                // rather than generating an IDR at the old fixed 100 ms.
                let wait = reorder_wait(ctx.control_rtt_ms.load(Ordering::Relaxed));
                if readers.is_empty() && ordered.expire(wait) {
                    gaps+=1;
                    delivery.invalidate();
                    let now_ms = ctx.clock.now_ms();
                    let request_sent = gap_repair.request(&mut delivery, &ctx.ctrl, now_ms);
                    tracing::warn!(expected_seq=ctx.next_seq.load(Ordering::Relaxed),reorder_budget_ms=wait.as_millis(),request_sent,pending_recovery_age_ms=gap_repair.requested_at_ms.map(|at| now_ms.saturating_sub(at)),"desktop reference gap expired");
                }
                if health.elapsed()>=std::time::Duration::from_secs(5) {
                    tracing::info!(admitted,completed,rejected,stale,timed_out,gaps,received_keys,in_flight=ctx.receiving.load(Ordering::Relaxed),"desktop receiver health");
                    health=std::time::Instant::now();
                }
            },
            frame = readers.join_next(), if !readers.is_empty() => {
                match frame {
                    Some(Ok(FrameRead::Complete(header,body,budget)))=>ordered.push(header,(body,budget)),
                    Some(Ok(FrameRead::Stale))=>{stale+=1;},
                    Some(Ok(FrameRead::TimedOut))=>{timed_out+=1;gap_repair.clear();delivery.request_idr(&ctx.ctrl,ctx.clock.now_ms());},
                    _=>{rejected+=1;gap_repair.clear();delivery.request_idr(&ctx.ctrl,ctx.clock.now_ms());},
                }
            }
            // A compressed relay consumer can backpressure completed readers.
            // Leave further streams in the bounded router inbox instead of
            // rejecting a legitimate successor while its local queue is busy.
            stream = uni.recv(), if ctx.encoded_tx.is_none() || ctx.receiving.load(Ordering::Relaxed) < MAX_FRAME_READERS => {
                let Some(stream) = stream else { break; };
                // Refuse excess work immediately; don't create parked tasks
                // or read a large body before obtaining its memory budget.
                if ctx.receiving.load(Ordering::Relaxed) >= MAX_FRAME_READERS { rejected+=1;continue; }
                let Ok(slot) = FRAME_SLOTS.try_acquire() else { rejected+=1;continue; };
                admitted+=1;
                ctx.receiving.fetch_add(1, Ordering::Relaxed);
                let budget = FrameBudget { _slot: slot, receiving: ctx.receiving.clone() };
                let next_seq = ctx.next_seq.clone();
                readers.spawn(read_one_with_receipt(stream, next_seq, budget, ctx.receipts.clone()).in_current_span());
            }
        }
    }
    readers.shutdown().await;
}

async fn read_one_with_receipt(
    mut stream: rds_net::RecvStream,
    next_seq: Arc<AtomicU64>,
    budget: FrameBudget,
    receipts: Option<mpsc::Sender<DesktopControl>>,
) -> FrameRead {
    let mut body_bytes = 0usize;
    let started = std::time::Instant::now();
    let header: FrameHeader =
        match tokio::time::timeout(FRAME_READ_TIMEOUT, read_frame(&mut stream)).await {
            Ok(Ok(header)) => header,
            Ok(Err(_)) => return FrameRead::Rejected,
            Err(_) => {
                tracing::warn!(
                    stage = "header",
                    body_bytes,
                    elapsed_ms = started.elapsed().as_millis(),
                    "desktop frame receive deadline exceeded"
                );
                return FrameRead::TimedOut;
            }
        };
    let frame_seq = header.seq;
    let keyframe = header.keyframe;
    let header_ms = started.elapsed().as_millis();
    if header.seq == u64::MAX
        || crate::frame_bytes(header.width as usize, header.height as usize).is_none()
    {
        return FrameRead::Rejected;
    }
    let expected_seq = next_seq.load(Ordering::Relaxed);
    if header.seq < expected_seq {
        let _ = stream.stop(rds_core::DESKTOP_FRAME_OBSOLETE.into());
        tracing::debug!(
            frame_seq,
            expected_seq,
            "desktop obsolete predecessor released"
        );
        return FrameRead::Stale;
    }
    let deadline = if keyframe {
        KEYFRAME_READ_TIMEOUT
    } else {
        FRAME_READ_TIMEOUT
    };
    let reading = async {
        // Retain metadata-only progress when the future times out. read_to_end
        // discards its partial buffer on cancellation and hid whether even a
        // header or any media bytes arrived during observed freezes.
        let mut body = Vec::new();
        let mut hasher = receipts.as_ref().map(|_| blake3::Hasher::new());
        let mut chunk = [0u8; 16 * 1024];
        while let Some(count) = stream.read(&mut chunk).await.ok()? {
            if count > MAX_FRAME_BYTES.saturating_sub(body.len()) {
                return None;
            }
            if let Some(hasher) = hasher.as_mut() {
                hasher.update(&chunk[..count]);
            }
            body.extend_from_slice(&chunk[..count]);
            body_bytes = body.len();
        }
        if let Some(receipts) = receipts {
            if body.is_empty() {
                return None;
            }
            let hasher = hasher?;
            let digest = *hasher.finalize().as_bytes();
            let obsolete = header.seq < next_seq.load(Ordering::Relaxed);
            // The proof belongs to the bounded wire reader, before ordered
            // decode or local IPC can backpressure completed payloads.
            receipts
                .send(DesktopControl::FrameReceived {
                    seq: header.seq,
                    digest,
                    obsolete,
                })
                .await
                .ok()?;
            tracing::trace!(target:"rds_desktop::frame_timing",frame_seq=header.seq,payload_bytes=body.len(),obsolete,
                "desktop complete payload receipt queued");
        }
        Some((header, body, budget))
    };
    match tokio::time::timeout(deadline.saturating_sub(started.elapsed()), reading).await {
        Ok(Some((header, body, budget))) => {
            if started.elapsed() >= std::time::Duration::from_millis(250) {
                tracing::warn!(
                    frame_seq,
                    keyframe,
                    body_bytes,
                    header_ms,
                    elapsed_ms = started.elapsed().as_millis(),
                    "desktop frame receive completed slowly"
                );
            }
            FrameRead::Complete(header, body, budget)
        }
        Ok(None) => {
            tracing::debug!(
                ?frame_seq,
                keyframe,
                body_bytes,
                "desktop frame rejected before completion"
            );
            FrameRead::Rejected
        }
        Err(_) => {
            tracing::warn!(
                ?frame_seq,
                keyframe,
                body_bytes,
                elapsed_ms = started.elapsed().as_millis(),
                "desktop frame receive deadline exceeded"
            );
            FrameRead::TimedOut
        }
    }
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
    #[cfg(not(feature = "x11"))]
    println!(
        "note: headless build has no decoder — frames stay encoded (use the encoded relay tap)"
    );
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
    fn heartbeat_measurements_are_bounded_and_require_exact_correlation() {
        let mut probes = HeartbeatProbes::default();
        for seq in 0..128 {
            probes.sent(seq, 1_000_000 + seq);
        }
        assert_eq!(probes.pending.len(), 64);
        assert!(probes.echoed(0, 1_000_000).is_none());
        assert!(probes.echoed(127, 0).is_none());
        assert!(probes.echoed(126, 1_000_127).is_none());
        assert!(probes.echoed(127, 1_000_127).is_some());
        assert!(probes.echoed(127, 1_000_127).is_none());
        probes.sent(64, 1_000_064);
        assert_eq!(
            probes
                .pending
                .iter()
                .filter(|(s, t, _)| (*s, *t) == (64, 1_000_064))
                .count(),
            1
        );
    }

    #[test]
    fn stale_missing_probe_does_not_make_passive_read_observation_overdue() {
        let mut probes = HeartbeatProbes::default();
        probes.sent(1, 11);
        probes.sent(2, 22);
        assert_eq!(probes.observation().count, 2);
        assert_eq!(probes.observation().oldest_seq, Some(1));
        assert!(probes.echoed(2, 22).is_some());
        assert_eq!(probes.observation().count, 0);
        // Diagnostic freshness must not remove ordinary late-reply correlation.
        assert!(probes.echoed(1, 11).is_some());
        probes.sent(3, 33);
        assert_eq!(probes.observation().oldest_seq, Some(3));
    }

    #[test]
    fn reference_wait_uses_rtt_and_caps_untrusted_or_missing_measurements() {
        assert_eq!(reorder_wait(0).as_millis(), 100);
        assert_eq!(reorder_wait(200).as_millis(), 400);
        assert_eq!(reorder_wait(50_000).as_millis(), 1000);
        assert_eq!(reorder_wait(u64::MAX - 1).as_millis(), 1000);
        assert_eq!(reorder_wait(u64::MAX).as_millis(), 250);
    }

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
    fn gap_repair_coalesces_until_a_key_failure_or_bounded_retry() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut delivery = Delivery::new();
        let mut repair = GapRepair::default();
        assert!(repair.request(&mut delivery, &tx, 0));
        assert!(matches!(rx.try_recv(), Ok(DesktopControl::RequestIdr)));
        for now in [500, 1000, 7999] {
            assert!(!repair.request(&mut delivery, &tx, now));
        }
        assert!(rx.try_recv().is_err());
        assert!(repair.request(&mut delivery, &tx, 8000));
        assert!(matches!(rx.try_recv(), Ok(DesktopControl::RequestIdr)));
        repair.clear();
        assert!(repair.request(&mut delivery, &tx, 8500));
        assert!(matches!(rx.try_recv(), Ok(DesktopControl::RequestIdr)));
    }

    #[test]
    fn a_full_control_queue_does_not_arm_gap_repair() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(DesktopControl::RequestIdr).unwrap();
        let mut delivery = Delivery::new();
        let mut repair = GapRepair::default();
        assert!(!repair.request(&mut delivery, &tx, 0));
        assert!(repair.requested_at_ms.is_none());
        rx.try_recv().unwrap();
        assert!(repair.request(&mut delivery, &tx, 0));
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
