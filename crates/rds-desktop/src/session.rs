//! Serving side of a desktop session.
//!
//! Frame delivery follows the MoQ pattern: every encoded frame goes out on
//! its own uni-directional stream carrying a `FrameHeader`, a fresher
//! queued frame always supersedes a stale one, and a stale frame still
//! in flight is reset mid-send rather than finishing on the wire.
//! Keyframes are never superseded — every delta behind them depends on
//! their landing. Input events, encoder steering and heartbeats arrive
//! on the bi-directional control stream, which outranks every frame
//! stream.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use rds_core::{DesktopControl, DesktopEvent, DesktopHello, FrameHeader, read_frame, write_frame};
use rds_net::{Connection, PathStats, RecvStream, SendStream};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::DesktopError;

/// Highest input/control priority; video frames rank below.
const CONTROL_PRIORITY: i32 = i32::MAX;
/// Frame streams sit at the midpoint: strictly below control, above
/// QUIC's default so they can't be starved by lower-priority traffic
/// the connection might one day carry.
const FRAME_PRIORITY: i32 = i32::MAX / 2;

/// Pacing sample interval for the bitrate controller.
const PACING_INTERVAL: Duration = Duration::from_millis(250);
/// Loss ratio that drives the bitrate down (2%).
const LOSS_STEP_DOWN: f64 = 0.02;
/// RTT growth over baseline that counts as congestion (1.5×).
const RTT_STEP_UP: f64 = 1.5;

/// Monotonic clock shared by producer and writer so `FrameHeader`
/// timestamps are comparable within one session.
#[derive(Clone)]
pub struct SessionClock {
    start: Instant,
}

impl Default for SessionClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl SessionClock {
    /// Milliseconds since the session clock started.
    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

/// One produced frame ready for the writer task.
pub struct Produced {
    pub header: FrameHeader,
    pub payload: Bytes,
}

/// Live knobs the session applies while a producer runs: the pacing
/// controller writes `bitrate`, the control stream flips `idr`, and the
/// producer reports `deadline_misses` it observed.
pub struct ProducerControls {
    pub bitrate: Arc<AtomicU64>,
    pub idr: Arc<AtomicBool>,
    pub deadline_misses: Arc<AtomicU64>,
}

impl ProducerControls {
    fn new(initial_bps: u64) -> Self {
        Self {
            bitrate: Arc::new(AtomicU64::new(initial_bps)),
            idr: Arc::new(AtomicBool::new(true)),
            deadline_misses: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// A blocking frame source: captures and encodes at its own cadence.
///
/// `produce` runs on a blocking thread; returning `None` ends the video
/// side of the session. Implementations honor `controls.bitrate` /
/// `controls.idr` each call and count a slot missed into
/// `controls.deadline_misses` when a frame lands after its cadence slot.
pub trait FrameProducer: Send + 'static {
    fn produce(
        &mut self,
        seq: u64,
        controls: &ProducerControls,
        clock: &SessionClock,
    ) -> Option<Produced>;
}

/// Everything `serve_desktop` needs beyond the negotiated hello.
#[derive(Default)]
pub struct SessionConfig {
    /// Hard ceiling for encoder bitrate — the grant's `max_bps`
    /// constraint lands here when the connection is grant-authorized.
    pub bitrate_ceiling: Option<u64>,
    /// Deny input even when a backend is available. Encoder steering and
    /// heartbeats remain usable. The agent derives this from verified grants.
    pub view_only: bool,
    /// Input backend override. `None` lazily probes on the input worker.
    /// Synthetic sessions should supply a synthetic sink, never the host's.
    pub input_sink: Option<Box<dyn crate::InputSink>>,
    /// Frame source override; `None` uses the platform capture backend.
    /// Synthetic sources are how tests pressurize the pipeline.
    pub producer: Option<Box<dyn FrameProducer>>,
    /// Session clock override. Tests share one clock with the client so
    /// header timestamps compare directly to client receive times.
    pub clock: Option<SessionClock>,
}

/// Adaptive bitrate for one session (Sunshine lesson: pace the encoder
/// to *measured* path quality, not a static guess).
///
/// Each `step` consumes one sample window: path counters plus the count
/// of producer deadline misses. Loss, congestion events or RTT growth
/// over the session baseline push bitrate down multiplicatively; clean
/// windows probe upward toward the ceiling; deadline misses push down
/// even when the path looks clean (encoder starvation is congestion too).
pub struct BitrateController {
    current: u64,
    floor: u64,
    ceiling: u64,
    baseline_rtt_ms: Option<u64>,
    last_sent: u64,
    last_lost: u64,
    last_congestion: u64,
    primed: bool,
}

impl BitrateController {
    pub fn new(initial: u64, ceiling: u64) -> Self {
        Self {
            current: initial.min(ceiling),
            floor: 100_000,
            ceiling,
            baseline_rtt_ms: None,
            last_sent: 0,
            last_lost: 0,
            last_congestion: 0,
            primed: false,
        }
    }

    /// Current target bitrate.
    pub fn current(&self) -> u64 {
        self.current
    }

    /// One pacing step. `path` is the selected path's counters (delta'd
    /// against the previous call); `deadline_misses` is the count of
    /// produce calls that missed their cadence slot since the last step.
    pub fn step(&mut self, path: Option<PathStats>, deadline_misses: u64) -> u64 {
        let mut next = self.current;
        if let Some(p) = path {
            let d_sent = p.sent.saturating_sub(self.last_sent);
            let d_lost = p.lost.saturating_sub(self.last_lost);
            let loss = if d_sent > 0 {
                d_lost as f64 / d_sent as f64
            } else {
                0.0
            };
            let congestion = p.congestion_events > self.last_congestion;
            let rtt_ms = p.rtt.as_millis() as u64;
            let rtt_high = self
                .baseline_rtt_ms
                .is_some_and(|b| rtt_ms > (b as f64 * RTT_STEP_UP) as u64);
            if self.baseline_rtt_ms.is_none() && rtt_ms > 0 {
                self.baseline_rtt_ms = Some(rtt_ms);
            }
            self.last_sent = p.sent;
            self.last_lost = p.lost;
            self.last_congestion = p.congestion_events;

            // First sample only establishes the baseline — never react
            // to counters we didn't watch accumulate.
            if self.primed && (loss > LOSS_STEP_DOWN || congestion || rtt_high) {
                next = (next * 7 / 10).max(self.floor);
            } else if self.primed && deadline_misses > 0 {
                next = (next * 85 / 100).max(self.floor);
            } else if self.primed {
                next = (next * 11 / 10).min(self.ceiling);
            }
            self.primed = true;
        } else if deadline_misses > 0 {
            next = (next * 85 / 100).max(self.floor);
        }
        self.current = next;
        next
    }
}

/// Serve one desktop session on an already-accepted stream pair.
///
/// `conn` is needed to open per-frame uni streams back to the viewer.
pub async fn serve_desktop(
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    hello: DesktopHello,
) -> Result<(), DesktopError> {
    serve_desktop_with(conn, send, recv, hello, SessionConfig::default()).await
}

/// Full session form: explicit config lets callers bound the bitrate
/// (grant constraints) and substitute the frame source (tests, bench).
pub async fn serve_desktop_with(
    conn: Connection,
    mut send: SendStream,
    mut recv: RecvStream,
    hello: DesktopHello,
    config: SessionConfig,
) -> Result<(), DesktopError> {
    send.set_priority(CONTROL_PRIORITY)?;
    let clock = config.clock.clone().unwrap_or_default();
    let max_fps = hello.max_fps.clamp(1, 240);
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(max_fps));
    let acks = hello.input_acks;
    let ceiling = config.bitrate_ceiling.unwrap_or(8_000_000).max(100_000);
    let controls = ProducerControls::new(4_000_000_u64.min(ceiling));
    // No backend is opened for a view-only session. Blocking platform calls
    // run on one bounded, session-owned worker, outside the async executor.
    let mut input = None;
    let mut input_sink = config.input_sink;

    // Capture+encode runs on a blocking thread; frames flow to the writer.
    let (tx, mut rx) = mpsc::channel::<Produced>(2);
    let capture_task = {
        let clock = clock.clone();
        let bitrate = Arc::clone(&controls.bitrate);
        let idr = Arc::clone(&controls.idr);
        let misses = Arc::clone(&controls.deadline_misses);
        let mut producer = config.producer;
        tokio::task::spawn_blocking(move || {
            let producer_controls = ProducerControls {
                bitrate,
                idr,
                deadline_misses: misses,
            };
            let mut source = match producer.take() {
                Some(p) => p,
                None => platform_producer(hello.display, frame_interval),
            };
            let mut seq = 0u64;
            loop {
                // The channel is the session's lifecycle: when the
                // session ends the writer drops `rx` and this loop exits
                // — abort() cannot interrupt spawn_blocking work.
                if tx.is_closed() {
                    return;
                }
                match source.produce(seq, &producer_controls, &clock) {
                    Some(p) => {
                        // Bounded queue: when full the writer is behind
                        // and this frame is dropped — its successor
                        // lands fresher. A keyframe carries the pending
                        // IDR request though, so dropping one re-arms the
                        // flag instead of losing the request to
                        // backpressure.
                        let was_keyframe = p.header.keyframe;
                        if tx.try_send(p).is_err() && was_keyframe {
                            producer_controls.idr.store(true, Ordering::Relaxed);
                        }
                        seq += 1;
                    }
                    None => return,
                }
            }
        })
    };

    // Pacing: sample path counters + deadline misses into the controller,
    // which writes the bitrate the producer reads each frame.
    let pacing = {
        let conn = conn.clone();
        let bitrate = Arc::clone(&controls.bitrate);
        let misses = Arc::clone(&controls.deadline_misses);
        let mut controller = BitrateController::new(4_000_000, ceiling);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PACING_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let missed = misses.swap(0, Ordering::Relaxed);
                let bps = controller.step(conn.current_path_stats(), missed);
                bitrate.store(bps.min(u64::from(u32::MAX)), Ordering::Relaxed);
            }
        })
    };

    // Writer task: one uni stream per frame. A continuous producer
    // means any frame queued behind an in-progress send is already
    // stale — the collapse keeps only the newest (decode-aware: a
    // queued keyframe always survives since deltas behind it can't
    // decode without it), and a send still in flight when a fresher
    // frame arrives is reset mid-write rather than allowed to finish
    // (MoQ-style stale reset): the client would drop the tail anyway,
    // so its unsent bytes only consume path capacity the fresh frame
    // needs.
    let writer_conn = conn.clone();
    let writer_clock = clock.clone();
    let writer_bitrate = Arc::clone(&controls.bitrate);
    let writer_idr = Arc::clone(&controls.idr);
    let mut writer = tokio::spawn(async move {
        // Token bucket on the paced bitrate: offering faster than the
        // path sustains only backlogs QUIC's send buffer with frames
        // that arrive stale — the collapse cannot reach them once
        // buffered. Debt is capped at half a second so a large
        // keyframe can't stall the writer.
        let mut budget = 0.0f64;
        let mut last = Instant::now();
        let mut pending: Option<Produced> = None;
        'writer: loop {
            let mut produced = match pending.take() {
                Some(p) => p,
                None => match rx.recv().await {
                    Some(p) => p,
                    None => break,
                },
            };
            produced = collapse(produced, &mut rx);
            // Encode-failure placeholders carry no payload: sending one
            // decodes to garbage on the client, while a skipped seq is
            // what the client's gap→IDR resync is for.
            if produced.payload.is_empty() {
                continue;
            }
            let bps = writer_bitrate.load(Ordering::Relaxed).max(50_000) as f64 / 8.0;
            let now = Instant::now();
            budget = (budget + now.duration_since(last).as_secs_f64() * bps).min(bps * 0.25);
            last = now;
            let cost = produced.payload.len() as f64 + 64.0;
            if cost > budget {
                let wait = ((cost - budget) / bps).min(0.5);
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                budget = (budget - cost).max(-bps * 0.5);
                // Frames produced during the pacing wait are fresher —
                // collapse once more before committing to the wire.
                produced = collapse(produced, &mut rx);
                if produced.payload.is_empty() {
                    continue;
                }
            } else {
                budget -= cost;
            }
            produced.header.send_ts_ms = writer_clock.now_ms();
            match send_frame(&writer_conn, &produced, &mut rx).await {
                SendOutcome::Sent => {}
                SendOutcome::Superseded(newer) => pending = Some(newer),
                SendOutcome::ResetStale => {
                    // The dropped tail broke the delta chain — the next
                    // produced frame must be an IDR, and the queued
                    // deltas in front of it are undecodable.
                    writer_idr.store(true, Ordering::Relaxed);
                    while let Ok(queued) = rx.try_recv() {
                        if queued.header.keyframe {
                            pending = Some(queued);
                            continue 'writer;
                        }
                    }
                }
                SendOutcome::Done | SendOutcome::Failed => break 'writer,
            }
        }
    });

    // Control loop: input + encoder steering + heartbeat, until the
    // peer goes away. `send` also carries DesktopEvent replies.
    let send_clock = clock.clone();
    let session_display = hello.display;
    let control = async {
        loop {
            match read_frame::<_, DesktopControl>(&mut recv).await {
                Ok(DesktopControl::Input(ev)) => {
                    if config.view_only {
                        tracing::debug!("view-only input dropped");
                        continue;
                    }
                    // The grant/display constraint was scoped to the
                    // hello's display — an event targeting another
                    // display is out of scope. Skip it (and don't ack:
                    // an ack reports the event handled).
                    if ev.display_id != session_display {
                        tracing::warn!(
                            event_display = ev.display_id,
                            session_display,
                            "input event for out-of-scope display dropped"
                        );
                        continue;
                    }
                    let input = input.get_or_insert_with(|| {
                        super::input::worker::InputWorker::new(input_sink.take())
                    });
                    let seq = ev.seq;
                    if let Err(e) = input.inject(ev).await {
                        tracing::warn!("input injection failed: {e}");
                        continue;
                    }
                    if acks {
                        let ack = DesktopEvent::InputAck {
                            seq,
                            handled_ts_ms: send_clock.now_ms(),
                        };
                        if write_frame(&mut send, &ack).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(DesktopControl::RequestIdr) => {
                    controls.idr.store(true, Ordering::Relaxed);
                }
                Ok(DesktopControl::SetBitrate(bps)) => {
                    let bps = u64::from(bps.max(50_000)).min(ceiling);
                    controls.bitrate.store(bps, Ordering::Relaxed);
                }
                Ok(DesktopControl::Heartbeat { seq, ts_ms }) => {
                    if write_frame(&mut send, &DesktopEvent::Heartbeat { seq, ts_ms })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break, // peer closed the control stream
            }
        }
    };

    let result = tokio::select! {
        _ = control => Ok(()),
        res = &mut writer => match res {
            Ok(()) => Ok(()),
            Err(e) => Err(DesktopError::Io(std::io::Error::other(e.to_string()))),
        },
    };
    // Teardown has to unwind every task: `writer` and `pacing` must be
    // aborted — dropping their JoinHandles only detaches them. Once the
    // writer stops, `rx` closes and the blocking producer exits on
    // `tx.is_closed()` (abort() cannot interrupt spawn_blocking work).
    writer.abort();
    pacing.abort();
    capture_task.abort();
    result
}

/// Platform capture producer, or a `NullProducer` when the build has no
/// capture backend (session still serves control input + heartbeats).
fn platform_producer(_display: u32, _interval: Duration) -> Box<dyn FrameProducer> {
    #[cfg(all(target_os = "linux", feature = "x11"))]
    match x11::X11Producer::new(_display, _interval) {
        Ok(p) => return Box::new(p),
        Err(e) => tracing::warn!("capture init failed: {e}"),
    }
    Box::new(NullProducer)
}

/// Producer that yields nothing — the control plane of the session
/// stays live while the video side cleanly idles.
pub struct NullProducer;

impl FrameProducer for NullProducer {
    fn produce(&mut self, _seq: u64, _c: &ProducerControls, _t: &SessionClock) -> Option<Produced> {
        None
    }
}

/// Deterministic synthetic producer for tests and benches: fixed-size
/// patterned frames at a precise cadence, honoring bitrate (throttles
/// byte generation) and IDR requests (marks the next frame a keyframe).
/// Needs no display, codec or platform — the pressure on the pipeline
/// (queueing, priority, pacing) is identical to a real encoder.
pub struct SyntheticProducer {
    interval: Duration,
    width: u32,
    height: u32,
    frame_bytes: usize,
    keyframe_every: u64,
    next_due: Instant,
}

impl SyntheticProducer {
    /// `fps` capped by the caller's `max_fps`; `frame_bytes` sets the
    /// per-frame payload so tests control offered load directly.
    pub fn new(fps: u32, width: u32, height: u32, frame_bytes: usize) -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / f64::from(fps.max(1))),
            width,
            height,
            frame_bytes,
            keyframe_every: 120,
            next_due: Instant::now(),
        }
    }

    pub fn keyframe_every(mut self, n: u64) -> Self {
        self.keyframe_every = n.max(1);
        self
    }
}

impl FrameProducer for SyntheticProducer {
    fn produce(
        &mut self,
        seq: u64,
        controls: &ProducerControls,
        clock: &SessionClock,
    ) -> Option<Produced> {
        self.next_due += self.interval;
        if let Some(sleep) = self.next_due.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep);
        } else {
            // Landed after our slot — count the deadline miss for the
            // pacing controller and resync the cadence.
            controls.deadline_misses.fetch_add(1, Ordering::Relaxed);
            self.next_due = Instant::now();
        }
        let capture_ts_ms = clock.now_ms();
        // Synthetic encode: bitrate hint throttles frame size (clamped),
        // IDR requests flip the next frame to keyframe.
        let wants_idr = controls.idr.swap(false, Ordering::Relaxed);
        let keyframe = wants_idr || seq.is_multiple_of(self.keyframe_every);
        let bps = controls.bitrate.load(Ordering::Relaxed);
        let cap = (bps / 8 / 30).max(256) as usize; // ~30fps byte budget floor
        let len = self.frame_bytes.min(cap.max(self.frame_bytes.min(256)));
        let mut payload = vec![0u8; len.max(64)];
        payload[..8].copy_from_slice(&seq.to_le_bytes());
        let encode_done_ts_ms = clock.now_ms();
        Some(Produced {
            header: FrameHeader {
                seq,
                capture_ts_ms,
                encode_done_ts_ms,
                send_ts_ms: 0, // stamped by the writer
                keyframe,
                codec: rds_core::Codec::H264,
                width: self.width,
                height: self.height,
            },
            payload: Bytes::from(payload),
        })
    }
}

#[cfg(all(target_os = "linux", feature = "x11"))]
mod x11 {
    use super::*;
    use crate::capture::x11::X11Capturer;
    use crate::codec::openh264::H264Encoder;
    use crate::{Capturer, Encoder};

    /// Live X11 capture → OpenH264 encode at a fixed cadence.
    pub struct X11Producer {
        capturer: X11Capturer,
        encoder: H264Encoder,
        interval: Duration,
        next_due: Instant,
    }

    impl X11Producer {
        pub fn new(display: u32, interval: Duration) -> Result<Self, DesktopError> {
            let capturer = X11Capturer::new(display)?;
            let fps = 1.0 / interval.as_secs_f32();
            let encoder = H264Encoder::new(4_000_000, fps)?;
            Ok(Self {
                capturer,
                encoder,
                interval,
                next_due: Instant::now(),
            })
        }
    }

    impl X11Producer {
        /// Damage-aware pause: while the screen is still, capture and
        /// encode cost nothing. Polls every `IDLE_POLL` for a damage
        /// event, bounded by `IDLE_MAX` so the stream still emits a
        /// refresh frame about once a second and session teardown
        /// (the caller's `is_closed` check) is never deferred past it.
        /// `idr` wakes the loop early: a viewer joining or recovering
        /// from loss asks for a keyframe and must not wait out the cap.
        fn idle_wait(&mut self, idr: &AtomicBool) {
            const IDLE_POLL: Duration = Duration::from_millis(25);
            const IDLE_MAX: Duration = Duration::from_secs(1);
            if self.capturer.changed() || idr.load(Ordering::Relaxed) {
                return;
            }
            let deadline = Instant::now() + IDLE_MAX;
            loop {
                std::thread::sleep(IDLE_POLL);
                if self.capturer.changed()
                    || idr.load(Ordering::Relaxed)
                    || Instant::now() >= deadline
                {
                    break;
                }
            }
            // Idle time is not a cadence miss: reset the schedule so
            // the skipped slots don't count as deadline misses.
            self.next_due = Instant::now();
        }
    }

    impl FrameProducer for X11Producer {
        fn produce(
            &mut self,
            seq: u64,
            controls: &ProducerControls,
            clock: &SessionClock,
        ) -> Option<Produced> {
            self.idle_wait(&controls.idr);
            self.next_due += self.interval;
            if let Some(sleep) = self.next_due.checked_duration_since(Instant::now()) {
                std::thread::sleep(sleep);
            } else {
                controls.deadline_misses.fetch_add(1, Ordering::Relaxed);
                self.next_due = Instant::now();
            }
            let raw = match self.capturer.capture() {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("capture failed: {e}");
                    return None;
                }
            };
            let capture_ts_ms = clock.now_ms();
            if controls.idr.swap(false, Ordering::Relaxed) {
                self.encoder.request_idr();
            }
            self.encoder.set_bitrate(
                controls
                    .bitrate
                    .load(Ordering::Relaxed)
                    .min(u64::from(u32::MAX)) as u32,
            );
            let (width, height) = (raw.width, raw.height);
            match self.encoder.encode(&raw) {
                Ok(frame) => Some(Produced {
                    header: FrameHeader {
                        seq,
                        capture_ts_ms,
                        encode_done_ts_ms: clock.now_ms(),
                        send_ts_ms: 0,
                        keyframe: frame.keyframe,
                        codec: frame.codec,
                        width,
                        height,
                    },
                    payload: frame.data,
                }),
                Err(e) => {
                    tracing::warn!("encode failed: {e}");
                    Some(Produced {
                        header: FrameHeader {
                            seq,
                            capture_ts_ms,
                            encode_done_ts_ms: clock.now_ms(),
                            send_ts_ms: 0,
                            keyframe: false,
                            codec: rds_core::Codec::H264,
                            width,
                            height,
                        },
                        payload: Bytes::new(),
                    })
                }
            }
        }
    }
}

/// Drain queued frames newest-wins. Decode-aware: once a keyframe is
/// in the mix it absorbs everything — deltas produced after it cannot
/// decode without it, so the keyframe is kept and later deltas are
/// skipped rather than the other way around.
fn collapse(mut produced: Produced, rx: &mut mpsc::Receiver<Produced>) -> Produced {
    let mut have_keyframe = produced.header.keyframe;
    while let Ok(newer) = rx.try_recv() {
        if newer.header.keyframe || !have_keyframe {
            have_keyframe |= newer.header.keyframe;
            produced = newer;
        }
    }
    produced
}

/// Reset code for a frame stream abandoned mid-send — the frame went
/// stale while still in flight, so its tail is dropped instead of
/// consuming path capacity the fresher frame needs.
const STALE_FRAME_RESET: u32 = 0x1;
/// One budget covers stream credit, tag, header and the complete payload.
const FRAME_SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// An interrupted frame must never look like a successfully finished payload.
struct FrameSend {
    stream: SendStream,
    finished: bool,
}

impl Drop for FrameSend {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.stream.reset(STALE_FRAME_RESET.into());
        }
    }
}

/// How one frame send ended.
enum SendOutcome {
    /// Frame fully sent.
    Sent,
    /// A fresher decodable frame supersedes — send it next.
    Superseded(Produced),
    /// A stale delta was reset mid-send: the reference chain is broken
    /// on the client and only an IDR resyncs it, so queued deltas are
    /// worthless and the next produced frame must be a keyframe.
    ResetStale,
    /// Producer closed mid-send; the final frame was finished.
    Done,
    /// Transport failure — the writer ends.
    Failed,
}

/// Send one frame on its own tagged uni stream, aborting mid-write if
/// a fresher frame lands: an in-flight keyframe is finished (the chain
/// behind it depends on it), a stale delta is reset.
async fn send_frame(
    conn: &Connection,
    produced: &Produced,
    rx: &mut mpsc::Receiver<Produced>,
) -> SendOutcome {
    match tokio::time::timeout(FRAME_SEND_TIMEOUT, send_frame_inner(conn, produced, rx)).await {
        Ok(outcome) => outcome,
        Err(_) => {
            tracing::debug!("frame send deadline exceeded");
            SendOutcome::Failed
        }
    }
}

async fn send_frame_inner(
    conn: &Connection,
    produced: &Produced,
    rx: &mut mpsc::Receiver<Produced>,
) -> SendOutcome {
    let mut sending = match conn.open_uni().await {
        Ok(stream) => FrameSend {
            stream,
            finished: false,
        },
        Err(e) => {
            tracing::debug!("frame stream open failed: {e}");
            return SendOutcome::Failed;
        }
    };
    let stream = &mut sending.stream;
    // Frame streams rank below the control stream — a stale frame
    // must never delay an input event or a resync request.
    if let Err(e) = stream.set_priority(FRAME_PRIORITY) {
        tracing::debug!("frame stream priority failed: {e}");
    }
    // Every uni stream leads with its UniHello tag — the receiver's
    // per-connection demux routes on it.
    if let Err(e) = write_frame(&mut *stream, &rds_core::UniHello::Desktop).await {
        tracing::debug!("frame tag write failed: {e}");
        return SendOutcome::Failed;
    }
    if let Err(e) = write_frame(&mut *stream, &produced.header).await {
        tracing::debug!("frame header write failed: {e}");
        return SendOutcome::Failed;
    }
    let outcome = match send_payload(stream, produced, rx).await {
        Ok(PayloadOutcome::Abandoned(next)) => {
            return next.map_or(SendOutcome::ResetStale, SendOutcome::Superseded);
        }
        Ok(PayloadOutcome::Sent) => SendOutcome::Sent,
        Ok(PayloadOutcome::Superseded(next)) => SendOutcome::Superseded(next),
        Ok(PayloadOutcome::ProducerEnded) => SendOutcome::Done,
        Err(e) => {
            tracing::debug!("frame send failed: {e}");
            return SendOutcome::Failed;
        }
    };
    if let Err(e) = stream.finish() {
        tracing::debug!("frame finish failed: {e}");
        return SendOutcome::Failed;
    }
    sending.finished = true;
    outcome
}

/// Completed payloads may FIN; abandoned ones must RESET.
enum PayloadOutcome {
    Sent,
    Superseded(Produced),
    ProducerEnded,
    Abandoned(Option<Produced>),
}

async fn send_payload<W: AsyncWrite + Unpin>(
    stream: &mut W,
    produced: &Produced,
    rx: &mut mpsc::Receiver<Produced>,
) -> std::io::Result<PayloadOutcome> {
    // write_all is not cancellation-safe: keep its progress alive across a
    // producer event. Starting another write_all would duplicate the prefix.
    let writing = stream.write_all(&produced.payload);
    tokio::pin!(writing);
    tokio::select! {
        result = &mut writing => {
            result?;
            Ok(PayloadOutcome::Sent)
        }
        newer = rx.recv() => match newer {
            None => {
                writing.await?;
                Ok(PayloadOutcome::ProducerEnded)
            }
            Some(newer) if produced.header.keyframe => {
                writing.await?;
                Ok(PayloadOutcome::Superseded(newer))
            }
            Some(newer) => Ok(PayloadOutcome::Abandoned(
                newer.header.keyframe.then_some(newer)
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn produced(seq: u64, keyframe: bool) -> Produced {
        Produced {
            header: FrameHeader {
                seq,
                capture_ts_ms: 0,
                encode_done_ts_ms: 0,
                send_ts_ms: 0,
                keyframe,
                codec: rds_core::Codec::H264,
                width: 16,
                height: 16,
            },
            payload: Bytes::from((0..4096).map(|n| (n % 251) as u8).collect::<Vec<_>>()),
        }
    }

    async fn partial_write(closed: bool) {
        use std::future::{Future, poll_fn};
        use std::task::Poll;
        use tokio::io::AsyncReadExt;

        tokio::time::timeout(Duration::from_secs(3), async {
            let (mut writer, mut reader) = tokio::io::duplex(64);
            let (tx, mut rx) = mpsc::channel(1);
            let frame = produced(0, true);
            let mut wire = vec![0; 17];
            {
                let mut sending = Box::pin(send_payload(&mut writer, &frame, &mut rx));
                // Poll until the tiny stream buffer is full, then consume only
                // a prefix. The producer event must interrupt a partial write.
                poll_fn(|cx| {
                    assert!(sending.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                }).await;
                reader.read_exact(&mut wire).await.unwrap();
                if !closed {
                    tx.send(produced(1, false)).await.unwrap();
                }
                drop(tx);
                poll_fn(|cx| {
                    assert!(sending.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                }).await;
                let (result, ()) = tokio::join!(sending, async {
                    // Drain enough to complete the write; after dropping the
                    // writer below, read to EOF to detect any extra bytes.
                    while wire.len() < frame.payload.len() {
                        let mut chunk = [0; 256];
                        let n = reader.read(&mut chunk).await.unwrap();
                        assert_ne!(n, 0);
                        wire.extend_from_slice(&chunk[..n]);
                    }
                });
                if closed {
                    assert!(matches!(result.unwrap(), PayloadOutcome::ProducerEnded));
                } else {
                    assert!(matches!(result.unwrap(), PayloadOutcome::Superseded(next) if next.header.seq == 1));
                }
            }
            drop(writer);
            reader.read_to_end(&mut wire).await.unwrap();
            assert_eq!(wire.len(), frame.payload.len(), "partial-write prefix was duplicated");
            assert_eq!(wire.as_slice(), frame.payload.as_ref());
        }).await.expect("partial frame send hung");
    }

    #[tokio::test]
    async fn partial_keyframe_supersession_preserves_exact_bytes() {
        partial_write(false).await;
    }

    #[tokio::test]
    async fn partial_final_frame_preserves_exact_bytes() {
        partial_write(true).await;
    }

    fn path(sent: u64, lost: u64, rtt_ms: u64, congestion: u64) -> PathStats {
        PathStats {
            path_id: 0,
            rtt: Duration::from_millis(rtt_ms),
            cwnd: 64 * 1024,
            sent,
            lost,
            sent_bytes: sent * 1200,
            recv_bytes: 0,
            congestion_events: congestion,
            selected: true,
            via_relay: false,
        }
    }

    #[test]
    fn controller_drops_bitrate_on_loss() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step(Some(path(1000, 0, 20, 0)), 0); // prime baseline
        let bps = c.step(Some(path(2000, 80, 20, 0)), 0); // 4% loss
        assert!(bps < 4_000_000, "loss must cut bitrate, got {bps}");
    }

    #[test]
    fn controller_recovers_toward_ceiling() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step(Some(path(1000, 0, 20, 0)), 0);
        let mut bps = c.current();
        for i in 0..20 {
            bps = c.step(Some(path(2000 + i * 1000, 0, 20, 0)), 0);
        }
        assert_eq!(bps, 8_000_000, "clean windows must reach the ceiling");
    }

    #[test]
    fn controller_honors_floor_and_misses() {
        let mut c = BitrateController::new(200_000, 8_000_000);
        c.step(Some(path(1000, 0, 20, 0)), 0);
        for _ in 0..10 {
            c.step(Some(path(0, 0, 20, 0)), 5); // encoder starving
        }
        assert_eq!(c.current(), 100_000, "deadline misses bottom out at floor");
    }

    #[test]
    fn controller_reacts_to_rtt_growth() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step(Some(path(1000, 0, 20, 0)), 0);
        let bps = c.step(Some(path(2000, 0, 60, 0)), 0); // 3× baseline RTT
        assert!(bps < 4_000_000, "RTT growth must cut bitrate, got {bps}");
    }

    #[test]
    fn first_sample_never_penalizes() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        // A path that already lost packets before we started watching.
        let bps = c.step(Some(path(1_000_000, 500_000, 20, 0)), 0);
        assert_eq!(bps, 4_000_000, "first sample establishes baseline only");
    }
}
