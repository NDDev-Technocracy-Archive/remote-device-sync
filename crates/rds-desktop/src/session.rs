//! Serving side of a desktop session.
//!
//! Frame delivery follows the MoQ pattern: every encoded frame goes out on
//! its own uni-directional stream carrying a `FrameHeader`. Deltas retain
//! their predecessor; only an independent keyframe can replace a delta
//! still in flight. A sequence gap requires a keyframe before delivery
//! resumes. Input events, encoder steering and heartbeats arrive
//! on the bi-directional control stream, which outranks every frame
//! stream.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use rds_core::{ClipboardErrorCode, DesktopControl, DesktopEvent, DesktopHello, FrameHeader};
use rds_net::{Connection, PathStats, RecvStream, SendStream};
use rds_net::{read_frame, write_frame};
use std::borrow::Borrow;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::Instrument;

use crate::DesktopError;
use crate::receipts::Evidence as ReceiptEvidence;

use rds_net::wire::{CONTROL_STREAM_PRIORITY, MEDIA_STREAM_PRIORITY};

/// Pacing sample interval for the bitrate controller.
const PACING_INTERVAL: Duration = Duration::from_millis(250);

fn pacing_interval() -> tokio::time::Interval {
    // Each sample represents an observation window. An immediate first tick
    // has no window, and Skip can compress two pressure samples after a stall.
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + PACING_INTERVAL,
        PACING_INTERVAL,
    );
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick
}

/// Capture briefly after accepted input even when a compositor's root DAMAGE
/// notification does not describe the redirected application repaint.
const INPUT_REFRESH_BURST_MS: u64 = 250;
// Control replies must not inherit a media stream's thirty-second budget.
// After any partial-write failure the session drops SessionSend and resets it.
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Moderate random loss holds the offered rate; severe loss reduces it.
const LOSS_HOLD: f64 = 0.02;
const LOSS_STEP_DOWN: f64 = 0.10;
// An isolated loss among 3–8 desktop datagrams is not a 12–33% path loss
// estimate. Aggregate enough packets; QUIC handles isolated retransmissions.
const LOSS_SAMPLE_MIN_SENT: u64 = 100;
// A genuinely lossy short burst can react before the full sample fills.
const LOSS_FAST_MIN_LOST: u64 = 5;
const LOSS_FAST_RATIO: f64 = 0.10;
const PATH_CUT_COOLDOWN_TICKS: u8 = 4;
/// Sustained RTT growth over the preceding baseline counts as congestion (1.5×).
const RTT_STEP_UP: f64 = 1.5;
// Millisecond rounding and normal low-RTT scheduling noise must not turn a
// 1–3 ms loopback fluctuation into a repeated 30% encoder penalty. QUIC's
// own congestion control still handles the underlying path independently.
const RTT_MIN_INCREASE_MS: u64 = 10;
// RTT jitter on tiny idle updates is not evidence of excessive offered media.
// QUIC retains congestion control below this application-level sample bound.
const RTT_SAMPLE_MIN_BYTES: u64 = 32 * 1024;
// A queued FIN is not a delivery receipt. Keep fewer unacknowledged media
// streams than the receiver's four readers, leaving capacity for recovery.
pub(crate) const MAX_PENDING_FRAME_ACKS: usize = 3;
const FRAME_ACK_TIMEOUT: Duration = Duration::from_secs(5);
const KEYFRAME_ACK_TIMEOUT: Duration = Duration::from_secs(10);
// A delayed few-packet desktop update is not evidence that encoder load
// exceeds link capacity. Require at least 16 KiB crossing the soft deadline
// in each pacing observation before timing-only feedback cuts image quality.
// Outstanding stalled delivery and hard failures keep their independent path.
const MIN_SOFT_DELIVERY_LOAD_BYTES: u64 = 16 * 1024;

// QUIC path counters may remain clean while a reliable relay queues media.
// Observe actual frame delivery as well, without retaining frame payloads.
#[derive(Default)]
struct DeliveryFeedback {
    delayed: AtomicU64,
    delayed_bytes: AtomicU64,
    late_pending: AtomicU64,
    failed: AtomicU64,
    acknowledged: AtomicU64,
    timely_bytes: AtomicU64,
    timely_receipts: AtomicU64,
    obsolete: AtomicU64,
    last_ack_ms: AtomicU64,
    producing: AtomicBool,
    produced: AtomicU64,
    codec_skips: AtomicU64,
    last_produced_ms: AtomicU64,
    inputs_handled: AtomicU64,
    max_input_inject_ms: AtomicU64,
}

impl DeliveryFeedback {
    fn mark_delayed(&self, bytes: usize) {
        self.delayed_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.delayed.fetch_add(1, Ordering::Relaxed);
    }
    fn acknowledged(&self, bytes: usize, elapsed: Duration, budget: Duration) {
        self.acknowledged.fetch_add(1, Ordering::Relaxed);
        if elapsed <= budget {
            self.timely_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
            self.timely_receipts.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct LateReceipt(Arc<DeliveryFeedback>);

impl LateReceipt {
    fn new(feedback: Arc<DeliveryFeedback>) -> Self {
        feedback.late_pending.fetch_add(1, Ordering::Relaxed);
        Self(feedback)
    }
}

impl Drop for LateReceipt {
    fn drop(&mut self) {
        self.0.late_pending.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct DeliveryPressure {
    stalled_ticks: u8,
    delayed_ticks: u8,
    penalized: bool,
}

impl DeliveryPressure {
    fn sample(
        &mut self,
        late_pending: u64,
        delayed_frames: u64,
        delayed_bytes: u64,
        delivered: bool,
        failed: bool,
    ) -> bool {
        // Counts include receipts crossing their soft deadline, not only
        // completions. Packet jitter on sparse tiny updates is independent
        // of the encoder target; cutting it would merely starve Full HD.
        if delayed_frames == 0 || delayed_bytes < MIN_SOFT_DELIVERY_LOAD_BYTES {
            self.delayed_ticks = 0;
        } else {
            // Sustained substantial delayed payload may indicate excess
            // offered media load even while receipts continue to progress.
            self.delayed_ticks = self.delayed_ticks.saturating_add(1).min(2);
        }
        if delivered || late_pending == 0 {
            self.penalized = false;
        }
        self.stalled_ticks = if late_pending > 0 && !delivered {
            self.stalled_ticks.saturating_add(1).min(2)
        } else {
            0
        };
        // A blocked keyframe pauses capture: repeated cuts cannot shrink the
        // already encoded payload. Penalize this blockage once, then wait for
        // progress or a distinct hard failure before reducing again.
        let stalled = self.stalled_ticks >= 2;
        let sustained_delay = self.delayed_ticks >= 2;
        let impaired = stalled || sustained_delay || failed;
        if impaired && !self.penalized {
            self.penalized = true;
            return true;
        }
        if !impaired {
            self.penalized = false;
        }
        false
    }
}

#[derive(Clone)]
struct FrameDelivery {
    repair: Arc<crate::media_repair::MediaRepair>,
    idr: Arc<crate::ProducerSignal>,
    feedback: Arc<DeliveryFeedback>,
    latest_key_seq: Arc<AtomicU64>,
    payload_receipts: Option<crate::receipts::Receipts>,
}

#[derive(Debug, PartialEq, Eq)]
enum FrameReceipt {
    Delivered,
    Obsolete,
    Failed,
}

fn request_frame_repair(latest_key_seq: &AtomicU64, idr: &crate::ProducerSignal, failed_seq: u64) {
    let key = latest_key_seq.load(Ordering::Acquire);
    if key == u64::MAX || key <= failed_seq {
        idr.store(true, Ordering::Release);
    }
}

fn delivery_delay_budget(path: Option<PathStats>) -> Duration {
    path.map_or(Duration::from_millis(250), |p| {
        p.rtt
            .saturating_mul(3)
            .clamp(Duration::from_millis(250), Duration::from_secs(1))
    })
}

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

struct CapturePermit(Arc<AtomicU64>);
impl Drop for CapturePermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}
struct AdmittedFrame {
    produced: Produced,
    generation: u64,
    key: Option<crate::media_repair::KeyPermit>,
    permit: CapturePermit,
}
impl Borrow<Produced> for AdmittedFrame {
    fn borrow(&self) -> &Produced {
        &self.produced
    }
}
impl std::ops::Deref for AdmittedFrame {
    type Target = Produced;
    fn deref(&self) -> &Produced {
        &self.produced
    }
}
impl std::ops::DerefMut for AdmittedFrame {
    fn deref_mut(&mut self) -> &mut Produced {
        &mut self.produced
    }
}

/// Live knobs the session applies while a producer runs: the pacing
/// controller writes `bitrate`, the control stream flips `idr` and files
/// `requested` bitrate targets (0 = none pending) for the controller to
/// drain, and the producer reports `deadline_misses` it observed.
#[derive(Clone)]
pub struct ProducerControls {
    pub bitrate: Arc<AtomicU64>,
    pub idr: Arc<crate::ProducerSignal>,
    pub deadline_misses: Arc<AtomicU64>,
    pub requested: Arc<AtomicU64>,
    /// Session-clock deadline for recent-input capture; does not request IDRs.
    pub input_refresh_until_ms: Arc<AtomicU64>,
    /// Accepted input not yet observed by the admitted capture producer.
    pub input_refresh_pending: Arc<crate::ProducerSignal>,
    pub(crate) wake: Arc<crate::producer_signal::Wake>,
}

impl ProducerControls {
    /// Initial steering state for a standalone or serving frame producer.
    pub fn new(initial_bps: u64) -> Self {
        let wake = Arc::new(crate::producer_signal::Wake::default());
        Self {
            bitrate: Arc::new(AtomicU64::new(initial_bps)),
            idr: crate::ProducerSignal::new(true, wake.clone()),
            deadline_misses: Arc::new(AtomicU64::new(0)),
            requested: Arc::new(AtomicU64::new(0)),
            input_refresh_until_ms: Arc::new(AtomicU64::new(0)),
            input_refresh_pending: crate::ProducerSignal::new(false, wake.clone()),
            wake,
        }
    }

    /// Check input-driven capture after acquiring normal frame admission.
    /// A pending wake survives a blocked producer; consuming it starts one
    /// bounded burst on the producer's current session clock, without an IDR.
    pub fn input_refresh_active(&self, now_ms: u64) -> bool {
        if self.input_refresh_pending.swap(false, Ordering::AcqRel) {
            self.input_refresh_until_ms.fetch_max(
                now_ms.saturating_add(INPUT_REFRESH_BURST_MS),
                Ordering::AcqRel,
            );
        }
        self.input_refresh_until_ms.load(Ordering::Acquire) > now_ms
    }
}

/// A blocking frame source: captures and encodes at its own cadence.
///
/// `produce` runs on a blocking thread; returning `None` ends the video
/// side of the session. Implementations honor `controls.bitrate` /
/// `controls.idr` each call and count a slot missed into
/// `controls.deadline_misses` when a frame lands after its cadence slot.
pub trait FrameProducer: Send + 'static {
    /// Session admission deliberately paused capture for delivery or its
    /// negotiated rate cap. Rebase elapsed cadence
    /// without counting that wait as capture/encoder starvation.
    fn resume_after_backpressure(&mut self) {}
    /// A codec may intentionally emit no frame without changing its reference
    /// chain. Only that case preserves the encoded sequence; capture/encode
    /// errors remain discontinuities requiring resync.
    fn preserves_reference(&self) -> bool {
        false
    }
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
    /// Original caller scope/key/budget retained by real native work, even when
    /// the session future is canceled. This is lifetime, never authorization.
    pub native_lifetime: Option<crate::clipboard::NativeLifetime>,
    /// Prepared, agent-owned backend; native types do not enter the wire.
    pub source: Option<Arc<dyn crate::DesktopSource>>,
    /// Explicit DesktopV4 payload proofs; legacy sessions keep FIN receipts.
    pub payload_receipts: bool,
    /// Explicit per-session height overrides the deployment fallback. Zero
    /// keeps native geometry; None uses RDS_DESKTOP_OUTPUT_HEIGHT if supplied.
    pub output_height: Option<u32>,
    /// Native extent advertised at admission. Refuse a resize racing the
    /// initial capture probe rather than using stale viewer coordinates.
    pub source_extent: Option<(u32, u32)>,
    /// Hard ceiling for encoder bitrate — the grant's `max_bps`
    /// constraint lands here when the connection is grant-authorized.
    pub bitrate_ceiling: Option<u64>,
    /// Deny input even when a backend is available. Encoder steering and
    /// heartbeats remain usable. The agent derives this from verified grants.
    pub view_only: bool,
    /// The V5 greeting explicitly opted into native→viewer text clipboard
    /// offers. Legacy sessions keep only the original viewer→native paste.
    pub reverse_clipboard: bool,
    /// Input backend override. `None` lazily probes on the input worker.
    /// Synthetic sessions should supply a synthetic sink, never the host's.
    pub input_sink: Option<Box<dyn crate::InputSink>>,
    /// Frame source override; `None` uses the platform capture backend.
    /// Synthetic sources are how tests pressurize the pipeline.
    pub producer: Option<Box<dyn FrameProducer>>,
    /// Session clock override. Tests share one clock with the client so
    /// header timestamps compare directly to client receive times.
    pub clock: Option<SessionClock>,
    /// Uni-stream route tag frame streams lead with. `DesktopV2`
    /// sessions carry the negotiated `DesktopFrames { id }` route so a
    /// delayed stream from an ended session cannot reach a replacement;
    /// the default `Desktop` tag serves the legacy shared route.
    pub frame_route: Option<rds_core::UniHello>,
}

/// Adaptive bitrate for one session (Sunshine lesson: pace the encoder
/// to *measured* path quality, not a static guess).
///
/// Each `step` consumes one sample window: path counters plus the count
/// of producer deadline misses. A sufficiently populated severe-loss sample
/// or sustained RTT growth pushes bitrate down multiplicatively; moderate
/// loss holds the path estimate. Isolated transport congestion events
/// remain diagnostic and do not bypass the sample requirement. Clean
/// windows probe upward toward the ceiling; deadline misses push down
/// even when the path looks clean (encoder starvation is congestion too).
pub struct BitrateController {
    current: u64,
    floor: u64,
    ceiling: u64,
    previous_rtt_ms: Option<u64>,
    last_sent: u64,
    last_sent_bytes: u64,
    last_lost: u64,
    loss_sample_sent: u64,
    loss_sample_lost: u64,
    loss_hold: bool,
    rtt_rise_baseline: Option<u64>,
    rtt_rise_bytes: u64,
    path_cut_cooldown_ticks: u8,
    reduction_reason: Option<&'static str>,
    primed: bool,
    last_path: Option<u64>,
    delivery_hold_ticks: u8,
    delivery_cut_cooldown_ticks: u8,
    rtt_reduction: bool,
}

impl BitrateController {
    pub fn new(initial: u64, ceiling: u64) -> Self {
        Self {
            current: initial.min(ceiling),
            floor: 100_000.min(ceiling),
            ceiling,
            previous_rtt_ms: None,
            last_sent: 0,
            last_sent_bytes: 0,
            last_lost: 0,
            loss_sample_sent: 0,
            loss_sample_lost: 0,
            loss_hold: false,
            rtt_rise_baseline: None,
            rtt_rise_bytes: 0,
            path_cut_cooldown_ticks: 0,
            reduction_reason: None,
            primed: false,
            last_path: None,
            delivery_hold_ticks: 0,
            delivery_cut_cooldown_ticks: 0,
            rtt_reduction: false,
        }
    }

    /// Current target bitrate.
    pub fn current(&self) -> u64 {
        self.current
    }

    /// Steer to a viewer-requested target, clamped to the controller's
    /// own floor and ceiling; adaptation resumes from there.
    pub fn steer(&mut self, bps: u64) {
        self.current = bps.clamp(self.floor, self.ceiling);
    }

    /// One pacing step. `path` is the selected path's counters (delta'd
    /// against the previous call); `deadline_misses` is the count of
    /// produce calls that missed their cadence slot since the last step.
    pub fn step(&mut self, path: Option<PathStats>, deadline_misses: u64) -> u64 {
        let mut next = self.current;
        self.reduction_reason = None;
        self.rtt_reduction = false;
        if let Some(p) = path {
            if self.last_path != Some(p.path_id) {
                self.last_path = Some(p.path_id);
                self.previous_rtt_ms = None;
                self.last_sent = 0;
                self.last_sent_bytes = 0;
                self.last_lost = 0;
                self.loss_sample_sent = 0;
                self.loss_sample_lost = 0;
                self.loss_hold = false;
                self.rtt_rise_baseline = None;
                self.rtt_rise_bytes = 0;
                self.path_cut_cooldown_ticks = 0;
                self.primed = false;
            }
            let d_sent = p.sent.saturating_sub(self.last_sent);
            let d_bytes = p.sent_bytes.saturating_sub(self.last_sent_bytes);
            let d_lost = p.lost.saturating_sub(self.last_lost);
            if self.primed {
                self.loss_sample_sent = self.loss_sample_sent.saturating_add(d_sent);
                self.loss_sample_lost = self.loss_sample_lost.saturating_add(d_lost);
            }
            let loss = if self.loss_sample_sent > 0 {
                self.loss_sample_lost as f64 / self.loss_sample_sent as f64
            } else {
                0.0
            };
            let sample_ready = self.loss_sample_sent >= LOSS_SAMPLE_MIN_SENT
                || (self.loss_sample_lost >= LOSS_FAST_MIN_LOST && loss > LOSS_FAST_RATIO);
            let loss_high = sample_ready && loss > LOSS_STEP_DOWN;
            if sample_ready {
                self.loss_hold = loss > LOSS_HOLD && !loss_high;
                self.loss_sample_sent = 0;
                self.loss_sample_lost = 0;
            }
            // A congestion-event counter also advances on isolated packet
            // loss. Do not let it bypass the minimum loss sample. Transport
            // congestion control continues to pace every packet independently.
            let rtt_ms = p.rtt.as_millis() as u64;
            let mut rtt_high = false;
            if rtt_ms > 0 {
                let rises_from = |baseline: u64| {
                    rtt_ms.saturating_sub(baseline) >= RTT_MIN_INCREASE_MS
                        && rtt_ms > (baseline as f64 * RTT_STEP_UP) as u64
                };
                if let Some(baseline) = self.rtt_rise_baseline.take() {
                    // Confirm on a second pacing observation. A delayed ACK
                    // or scheduling spike must not compound encoder penalties.
                    rtt_high = rises_from(baseline)
                        && d_bytes > 0
                        && self.rtt_rise_bytes.saturating_add(d_bytes) >= RTT_SAMPLE_MIN_BYTES;
                    self.rtt_rise_bytes = 0;
                } else if let Some(baseline) = self.previous_rtt_ms
                    && rises_from(baseline)
                {
                    self.rtt_rise_baseline = Some(baseline);
                    self.rtt_rise_bytes = d_bytes;
                }
                self.previous_rtt_ms = Some(rtt_ms);
            }
            self.last_sent = p.sent;
            self.last_sent_bytes = p.sent_bytes;
            self.last_lost = p.lost;
            self.path_cut_cooldown_ticks = self.path_cut_cooldown_ticks.saturating_sub(1);

            // First sample only establishes the baseline. Coalesce one path
            // burst for a second, as with correlated media-receipt pressure.
            if self.primed && (loss_high || rtt_high) {
                if self.path_cut_cooldown_ticks == 0 {
                    self.rtt_reduction = rtt_high;
                    next = (next / 10 * 7 + next % 10 * 7 / 10).max(self.floor);
                    self.path_cut_cooldown_ticks = PATH_CUT_COOLDOWN_TICKS;
                    self.reduction_reason = Some(if loss_high {
                        "sampled_packet_loss"
                    } else {
                        "sustained_rtt"
                    });
                }
            } else if self.primed && deadline_misses > 0 {
                self.reduction_reason = Some("producer_deadline");
                next = (next / 100 * 85 + next % 100 * 85 / 100).max(self.floor);
            } else if self.primed && !self.loss_hold {
                next = next.saturating_add(next / 10).min(self.ceiling);
            }
            self.primed = true;
        } else if deadline_misses > 0 {
            self.reduction_reason = Some("producer_deadline");
            next = (next / 100 * 85 + next % 100 * 85 / 100).max(self.floor);
        }
        self.current = next;
        next
    }

    // The serving session supplements path samples with frame ACKs. A late
    // frame reduces offered load before its hard reset deadline. Hold that
    // reduction for five seconds; coalesce a burst of correlated receipts for
    // one second, and increase only on fresh successful delivery
    // and at 1% per sample so clean relay packet counters cannot immediately
    // drive the encoder back into the same backlog.
    #[cfg(test)]
    fn step_with_delivery(
        &mut self,
        path: Option<PathStats>,
        deadline_misses: u64,
        impaired: bool,
        delivered: bool,
    ) -> u64 {
        self.step_with_delivery_floor(path, deadline_misses, impaired, delivered, None)
    }

    fn step_with_delivery_floor(
        &mut self,
        path: Option<PathStats>,
        deadline_misses: u64,
        impaired: bool,
        delivered: bool,
        delivery_floor: Option<u64>,
    ) -> u64 {
        let previous = self.current;
        let held = self.delivery_hold_ticks > 0;
        let proposed = self.step(path, deadline_misses);
        self.delivery_cut_cooldown_ticks = self.delivery_cut_cooldown_ticks.saturating_sub(1);
        self.current = if impaired {
            self.delivery_hold_ticks = 20;
            if self.delivery_cut_cooldown_ticks == 0 {
                self.delivery_cut_cooldown_ticks = 4;
                self.path_cut_cooldown_ticks = PATH_CUT_COOLDOWN_TICKS;
                // RTT/loss and a delayed receipt may report the same event.
                // Apply the stronger response once, never multiply both cuts.
                let media_cut = (previous / 10 * 7 + previous % 10 * 7 / 10).max(self.floor);
                if media_cut <= proposed {
                    self.reduction_reason = Some("media_delivery");
                }
                proposed.min(media_cut)
            } else {
                proposed.min(previous)
            }
        } else if self.delivery_hold_ticks > 0 {
            self.delivery_hold_ticks -= 1;
            proposed.min(previous)
        } else if !delivered {
            proposed.min(previous)
        } else {
            // Reliable QUIC already retransmits isolated packet loss. Fresh
            // media receipts permit a bounded probe through a moderate-loss
            // hold; otherwise a rate cut would become an absorbing low-quality
            // state even after delivery recovers. Severe loss/RTT cuts remain.
            let proposed = if self.loss_hold && proposed == previous {
                previous
                    .saturating_add((previous / 100).max(1))
                    .min(self.ceiling)
            } else {
                proposed
            };
            proposed.min(previous.saturating_add((previous / 100).max(1)))
        };
        // Independent QUIC loss declarations must not lower a healthy media
        // stream beneath conservatively observed successful timely goodput.
        // RTT/producer pressure and real delivery problems retain their cuts.
        if !impaired
            && !held
            && delivered
            && self.delivery_hold_ticks == 0
            && !self.rtt_reduction
            && self.rtt_rise_baseline.is_none()
            && deadline_misses == 0
            && self.reduction_reason == Some("sampled_packet_loss")
            && let Some(floor) = delivery_floor
        {
            self.current = self.current.max(floor.min(self.ceiling));
        }
        self.current
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
    send: SendStream,
    mut recv: RecvStream,
    hello: DesktopHello,
    config: SessionConfig,
) -> Result<(), DesktopError> {
    let mut send = SessionSend(send);
    if config.payload_receipts
        && !matches!(
            config.frame_route,
            Some(rds_core::UniHello::DesktopFrames { .. })
        )
    {
        return Err(DesktopError::Capture(
            "payload receipts require an isolated session route".into(),
        ));
    }
    if config
        .output_height
        .is_some_and(|h| h != 0 && !(16..=4320).contains(&h))
    {
        return Err(DesktopError::Capture(
            "video height must be 0 or 16..=4320".into(),
        ));
    }
    send.0.set_priority(CONTROL_STREAM_PRIORITY)?;
    // Dropping the serving future aborts async siblings and queued blocking
    // work. A running capture call may finish, then sees its receiver closed.
    let mut workers = JoinSet::new();
    let mut capture = JoinSet::new();
    let clock = config.clock.clone().unwrap_or_default();
    let max_fps = hello.max_fps.clamp(1, 240);
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(max_fps));
    tracing::info!(display=hello.display,max_fps,output_height=?config.output_height,view_only=config.view_only,frame_route=?config.frame_route,"desktop serving started");
    let acks = hello.input_acks;
    if config.bitrate_ceiling.is_some_and(|rate| rate < 100_000) {
        return Err(DesktopError::Encode(
            "granted bitrate is below the supported 100000 bps floor".into(),
        ));
    }
    let ceiling = config
        .bitrate_ceiling
        .unwrap_or(8_000_000)
        .min(u64::from(u32::MAX));
    let controls = ProducerControls::new(4_000_000_u64.min(ceiling));
    let capture_stop = crate::producer_signal::Stop(controls.wake.clone());
    let native = crate::clipboard::ownership::Owner::new();
    if let Some(context) = config.native_lifetime.clone()
        && !native.retain(context)
    {
        return Err(DesktopError::Input(
            "native session context unavailable".into(),
        ));
    }
    struct CloseNative(Arc<crate::clipboard::ownership::Owner>);
    impl Drop for CloseNative {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let _close_native = CloseNative(native.clone());
    let native_closed = || DesktopError::Input("native session closed".into());
    // No backend is opened for a view-only session. Blocking platform calls
    // run on one bounded, session-owned worker, outside the async executor.
    let mut input = None;
    let clipboard_supported = config
        .source
        .as_ref()
        .is_none_or(|source| source.clipboard_supported());
    let mut input_sink = config.input_sink;
    if !config.view_only
        && input_sink.is_none()
        && let Some(source) = config.source.as_ref()
    {
        input_sink = Some(
            source.input(
                hello.display,
                config
                    .source_extent
                    .ok_or_else(|| DesktopError::Input("missing admitted source extent".into()))?,
            )?,
        );
    }

    // Capture+encode runs on a blocking thread; frames flow to the writer.
    let (tx, mut rx) = mpsc::channel::<AdmittedFrame>(2);
    let (media_repair, mut media_changes) = crate::media_repair::MediaRepair::new();
    let latest_key_seq = Arc::new(AtomicU64::new(u64::MAX));
    let capture_admission = Arc::new(AtomicU64::new(0));
    let delivery_feedback = Arc::new(DeliveryFeedback::default());
    let payload_receipts = config.payload_receipts.then(crate::receipts::Receipts::new);
    tracing::info!(
        payload_receipts = config.payload_receipts,
        "desktop delivery receipt mode"
    );
    {
        let clock = clock.clone();
        let producer_controls = controls.clone();
        let mut producer = config.producer;
        let native_work = native.begin().ok_or_else(native_closed)?;
        let backend = config.source.clone();
        let repair = media_repair.clone();
        let latest_key_seq = latest_key_seq.clone();
        let capture_admission = capture_admission.clone();
        let feedback = delivery_feedback.clone();
        let parent = tracing::Span::current();
        capture.spawn_blocking(move || {
            let native_work = native_work;
            let _entered = parent.enter();
            if native_work.closed() {
                return;
            }
            let mut source = match producer.take() {
                Some(p) => p,
                None => match backend {
                    Some(backend) => match backend.producer(hello.display, frame_interval, config.output_height, config.source_extent) {
                        Ok(producer) => producer,
                        Err(error) => { tracing::warn!(%error, "desktop source unavailable"); return; }
                    },
                    None => platform_producer(hello.display, frame_interval, config.output_height, config.source_extent),
                },
            };
            let mut seq = 0u64;
            let mut generation = 0;
            let mut next_capture = Instant::now();
            loop {
                // The channel is the session's lifecycle: when the
                // session ends the writer drops `rx` and this loop exits
                // — abort() cannot interrupt spawn_blocking work.
                if native_work.closed() || tx.is_closed() {
                    return;
                }
                // Reserve capture cadence through the bounded producer queue
                // before spending CPU or changing the codec's references.
                // Encoding into a full queue used to drop references and
                // force repeated expensive IDRs while QUIC was backlogged.
                let mut paused = false;
                let permit = loop {
                    while tx.capacity() == 0 || repair.key_pending() {
                        paused = true;
                        if tx.is_closed() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    if capture_admission
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                            (count < MAX_PENDING_FRAME_ACKS as u64).then_some(count + 1)
                        })
                        .is_ok()
                    {
                        break CapturePermit(capture_admission.clone());
                    }
                    if tx.is_closed() {
                        return;
                    }
                    paused = true;
                    std::thread::sleep(Duration::from_millis(1));
                };
                // Damage can wake a platform producer immediately. Enforce
                // the negotiated cap at admission, independently of its idle
                // wait or implementation. Capture cost consumes this interval.
                while let Some(wait) = next_capture.checked_duration_since(Instant::now()) {
                    paused = true;
                    if tx.is_closed() {
                        return;
                    }
                    std::thread::sleep(wait.min(Duration::from_millis(20)));
                }
                if paused {
                    // Deliberate admission waits are not encoder starvation.
                    source.resume_after_backpressure();
                }
                let produce_started = Instant::now();
                let frame_generation = repair.generation();
                if generation != frame_generation {
                    generation = frame_generation;
                    producer_controls.idr.store(true, Ordering::Release);
                }
                feedback.producing.store(true, Ordering::Relaxed);
                let result = source.produce(seq, &producer_controls, &clock);
                feedback.producing.store(false, Ordering::Relaxed);
                match result {
                    Some(p) => {
                        // A producer may wait for damage/cadence before it
                        // starts capture. Anchor the next admission to that
                        // shared-clock timestamp, without adding encoding cost.
                        let capture_started = clock
                            .start
                            .checked_add(Duration::from_millis(p.header.capture_ts_ms))
                            .unwrap_or(produce_started)
                            .clamp(produce_started, Instant::now());
                        next_capture = capture_started + frame_interval;
                        // A repair can race a blocking encode. Its mutated
                        // references are discarded; the next epoch forces IDR.
                        if repair.generation() != frame_generation {
                            continue;
                        }
                        if p.payload.is_empty() && source.preserves_reference() {
                            feedback.codec_skips.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        if !p.payload.is_empty() {
                            tracing::trace!(target:"rds_desktop::frame_timing", frame_seq=p.header.seq,
                                capture_ms=p.header.capture_ts_ms, encode_done_ms=p.header.encode_done_ts_ms,
                                payload_bytes=p.payload.len(), keyframe=p.header.keyframe, "desktop frame produced");
                            feedback
                                .last_produced_ms
                                .store(clock.now_ms(), Ordering::Relaxed);
                            feedback.produced.fetch_add(1, Ordering::Relaxed);
                        }
                        let key = if p.header.keyframe && !p.payload.is_empty() {
                            let Some(key) =
                                repair.key(frame_generation, producer_controls.idr.clone())
                            else {
                                continue;
                            };
                            latest_key_seq.store(p.header.seq, Ordering::Release);
                            Some(key)
                        } else {
                            None
                        };
                        // Losing any encoded reference breaks its successors,
                        // not only losing an IDR. Keep the two-slot bound and
                        // ask the producer for an independent replacement.
                        if tx
                            .try_send(AdmittedFrame {
                                produced: p,
                                generation: frame_generation,
                                key,
                                permit,
                            })
                            .is_err()
                        {
                            producer_controls.idr.store(true, Ordering::Relaxed);
                        }
                        let Some(next) = seq.checked_add(1) else {
                            return;
                        };
                        seq = next;
                    }
                    None => return,
                }
            }
        })
    };

    // Pacing: sample path counters + actual media delivery + deadline misses,
    // which writes the bitrate the producer reads each frame.
    {
        let conn = conn.clone();
        let bitrate = Arc::clone(&controls.bitrate);
        let requested = Arc::clone(&controls.requested);
        let misses = Arc::clone(&controls.deadline_misses);
        let feedback = delivery_feedback.clone();
        let admission = capture_admission.clone();
        let repair = media_repair.clone();
        let progress_clock = clock.clone();
        let mut controller = BitrateController::new(4_000_000, ceiling);
        let mut delivery_rate = crate::delivery_rate::DeliveryRate::default();
        workers.spawn(async move {
            let mut last_delayed = 0;
            let mut last_delayed_bytes = 0;
            let mut last_failed = 0;
            let mut last_acknowledged = 0;
            let mut pressure = DeliveryPressure::default();
            let mut health = Instant::now();
            let mut tick = pacing_interval();
            loop {
                tick.tick().await;
                // A viewer-filed target survives past one tick: the
                // controller is steered to it, then adaptation resumes.
                let req = requested.swap(0, Ordering::Relaxed);
                if req != 0 {
                    controller.steer(req);
                }
                let missed = misses.swap(0, Ordering::Relaxed);
                let previous = controller.current();
                let path = conn.current_path_stats();
                let delayed = feedback.delayed.load(Ordering::Relaxed);
                let delayed_bytes_total = feedback.delayed_bytes.load(Ordering::Relaxed);
                let failed = feedback.failed.load(Ordering::Relaxed);
                let acknowledged = feedback.acknowledged.load(Ordering::Relaxed);
                let delayed_frames = delayed.saturating_sub(last_delayed);
                let delayed_bytes = delayed_bytes_total.saturating_sub(last_delayed_bytes);
                let failed_frames = failed.saturating_sub(last_failed);
                let delivered = acknowledged > last_acknowledged;
                let late_pending = feedback.late_pending.load(Ordering::Relaxed);
                let impaired = pressure.sample(
                    late_pending,
                    delayed_frames,
                    delayed_bytes,
                    delivered,
                    failed_frames > 0,
                );
                let delivery_floor = delivery_rate.sample(
                    path.map(|p| p.path_id), progress_clock.now_ms(),
                    feedback.timely_bytes.load(Ordering::Relaxed),
                    feedback.timely_receipts.load(Ordering::Relaxed),
                    impaired || failed_frames > 0 || late_pending > 0,
                );
                let bps = controller.step_with_delivery_floor(
                    path,
                    missed,
                    impaired,
                    delivered,
                    delivery_floor,
                );
                (last_delayed, last_failed, last_acknowledged) = (delayed, failed, acknowledged);
                last_delayed_bytes = delayed_bytes_total;
                if bps != previous {
                    tracing::debug!(
                        previous_bps = previous,
                        bitrate_bps = bps,
                        timely_delivery_floor_bps = delivery_floor,
                        deadline_misses = missed,
                        delayed_frames,
                        delayed_bytes,
                        failed_frames,
                        late_pending,
                        delivery_stalled_ticks = pressure.stalled_ticks,
                        last_ack_ms = feedback.last_ack_ms.load(Ordering::Relaxed),
                        path_rtt_ms = ?path.map(|p| p.rtt.as_millis()),
                        path_id = ?path.map(|p| p.path_id),
                        path_via_relay = ?path.map(|p| p.via_relay),
                        path_sent = ?path.map(|p| p.sent),
                        path_lost = ?path.map(|p| p.lost),
                        path_congestion_events = ?path.map(|p| p.congestion_events),
                        "desktop bitrate adapted"
                    );
                }
                if bps < previous {
                    tracing::info!(
                        reduction_reason = controller.reduction_reason.unwrap_or("unknown"),
                        previous_bps = previous,
                        bitrate_bps = bps,
                        deadline_misses = missed,
                        delivery_impaired = impaired,
                        delivered,
                        late_pending,
                        delayed_frames,
                        delayed_bytes,
                        failed_frames,
                        path_rtt_ms = ?path.map(|p| p.rtt.as_millis()),
                        path_id = ?path.map(|p| p.path_id),
                        path_cwnd_bytes = ?path.map(|p| p.cwnd),
                        path_sent_bytes = ?path.map(|p| p.sent_bytes),
                        path_received_bytes = ?path.map(|p| p.recv_bytes),
                        path_via_relay = ?path.map(|p| p.via_relay),
                        path_sent = ?path.map(|p| p.sent),
                        path_lost = ?path.map(|p| p.lost),
                        path_congestion_events = ?path.map(|p| p.congestion_events),
                        "desktop bitrate reduced"
                    );
                }
                bitrate.store(bps.min(u64::from(u32::MAX)), Ordering::Relaxed);
                // Independent of frame sends: during a freeze, distinguish
                // native production, codec skips and delivery backpressure.
                if health.elapsed() >= Duration::from_secs(5) {
                    let produced = feedback.produced.load(Ordering::Relaxed);
                    tracing::info!(
                        producing = feedback.producing.load(Ordering::Relaxed),
                        produced,
                        codec_skips = feedback.codec_skips.load(Ordering::Relaxed),
                        last_produced_age_ms = ?(produced > 0).then(|| progress_clock.now_ms().saturating_sub(feedback.last_produced_ms.load(Ordering::Relaxed))),
                        pending_media_frames = admission.load(Ordering::Acquire),
                        keyframe_pending = repair.key_pending(),
                        media_repair_generation = repair.generation(),
                        media_repair_active = repair.active(),
                        media_repair_requests = repair.requests.load(Ordering::Relaxed),
                        media_repair_coalesced = repair.coalesced.load(Ordering::Relaxed),
                        bitrate_bps = bps,
                        delayed_delivery = delayed,
                        timely_delivery_floor_bps = delivery_floor,
                        timely_delivery_bytes = feedback.timely_bytes.load(Ordering::Relaxed),
                        timely_deliveries = feedback.timely_receipts.load(Ordering::Relaxed),
                        delayed_frames,
                        delayed_frames_total = delayed,
                        delayed_bytes,
                        delayed_bytes_total,
                        impaired,
                        late_pending,
                        delivery_stalled_ticks = pressure.stalled_ticks,
                        failed_delivery = failed,
                        obsolete_delivery = feedback.obsolete.load(Ordering::Relaxed),
                        last_ack_ms = feedback.last_ack_ms.load(Ordering::Relaxed),
                        path_rtt_ms = ?path.map(|p| p.rtt.as_millis()),
                        path_id = ?path.map(|p| p.path_id),
                        path_cwnd_bytes = ?path.map(|p| p.cwnd),
                        path_sent_bytes = ?path.map(|p| p.sent_bytes),
                        path_received_bytes = ?path.map(|p| p.recv_bytes),
                        path_via_relay = ?path.map(|p| p.via_relay),
                        path_sent = ?path.map(|p| p.sent),
                        path_lost = ?path.map(|p| p.lost),
                        path_congestion_events = ?path.map(|p| p.congestion_events),
                        inputs_handled = feedback.inputs_handled.load(Ordering::Relaxed),
                        max_input_inject_ms = feedback.max_input_inject_ms.swap(0, Ordering::Relaxed),
                        "desktop production and delivery health"
                    );
                    health = Instant::now();
                }
            }
        }.in_current_span())
    };

    // Writer task: one uni stream per frame. Backpressure retains references
    // through the bounded producer queue. A broken chain
    // requests an IDR locally instead of waiting for a client roundtrip.
    let writer_conn = conn.clone();
    let writer_clock = clock.clone();
    let writer_bitrate = Arc::clone(&controls.bitrate);
    let writer_idr = Arc::clone(&controls.idr);
    let writer_feedback = delivery_feedback.clone();
    let frame_route = config.frame_route.unwrap_or(rds_core::UniHello::Desktop);
    let writer_receipts = payload_receipts.clone();
    let repair = media_repair.clone();
    workers.spawn(async move {
        // Token bucket on the paced bitrate: offering faster than the
        // path sustains only backlogs QUIC's send buffer with frames
        // that arrive stale. Debt is capped at half a second so a large
        // keyframe can't stall the writer.
        let mut budget = 0.0f64;
        let mut last = Instant::now();
        let mut pending: Option<AdmittedFrame> = None;
        let mut chain = FrameChain::default();
        let mut sent = 0u64;
        let mut superseded = 0u64;
        let mut acknowledgements = JoinSet::new();
        let mut acknowledged = 0u64;
        let mut obsolete = 0u64;
        let mut failed_delivery = 0u64;
        let mut health = Instant::now();
        let mut generation = 0;
        'writer: loop {
            let current_generation = repair.generation();
            if current_generation != generation {
                let canceled_receipts = acknowledgements.len();
                // Only this desktop writer owns these tasks. Their reset-on-
                // drop streams release obsolete media without closing control
                // or any other connection service.
                acknowledgements.shutdown().await;
                if pending
                    .as_ref()
                    .is_some_and(|p| p.generation != current_generation)
                {
                    pending = None;
                }
                chain = FrameChain::default();
                budget = 0.0;
                last = Instant::now();
                generation = current_generation;
                tracing::info!(
                    media_repair_generation = generation,
                    canceled_receipts,
                    "desktop obsolete media retired for explicit repair"
                );
            }
            drain_frame_receipts(
                &mut acknowledgements,
                &mut chain,
                &writer_idr,
                &mut acknowledged,
                &mut obsolete,
                &mut failed_delivery,
            );
            if acknowledgements.len() >= MAX_PENDING_FRAME_ACKS {
                let receipt = tokio::select! {
                    biased;
                    changed = repair.changed(&mut media_changes, generation) => {
                        if changed.is_err() { break 'writer; }
                        continue 'writer;
                    }
                    receipt = acknowledgements.join_next() => receipt,
                };
                match receipt {
                    Some(Ok((_, FrameReceipt::Delivered))) => acknowledged += 1,
                    Some(Ok((_, FrameReceipt::Obsolete))) => obsolete += 1,
                    Some(Ok((seq, FrameReceipt::Failed))) => {
                        failed_delivery += 1;
                        chain.failed_receipt(seq);
                    }
                    _ => {
                        failed_delivery += 1;
                        chain.next = None;
                        writer_idr.store(true, Ordering::Relaxed);
                    }
                }
                continue;
            }
            let mut produced = match pending.take() {
                Some(p) => p,
                None => {
                    let next = tokio::select! {
                        biased;
                        changed = repair.changed(&mut media_changes, generation) => {
                            if changed.is_err() { break 'writer; }
                            continue 'writer;
                        }
                        frame = rx.recv() => frame,
                    };
                    match next {
                        Some(p) => p,
                        None => break 'writer,
                    }
                }
            };
            if produced.generation != generation {
                continue;
            }
            if produced.payload.is_empty() {
                chain.next = None;
                request_frame_repair(&latest_key_seq, &writer_idr, produced.header.seq);
                continue;
            }
            // Receipts can finish while the writer waits for the next frame.
            // Retire them before testing whether a recovered key has an empty
            // media queue; completed tasks are not outstanding delivery.
            drain_frame_receipts(
                &mut acknowledgements,
                &mut chain,
                &writer_idr,
                &mut acknowledged,
                &mut obsolete,
                &mut failed_delivery,
            );
            let bps = writer_bitrate.load(Ordering::Relaxed).max(50_000) as f64 / 8.0;
            let now = Instant::now();
            budget = (budget + now.duration_since(last).as_secs_f64() * bps).min(bps * 0.25);
            last = now;
            // With no unconfirmed media, a bounded independent key can start
            // immediately. QUIC still paces its packets and the existing key
            // receipt barrier prevents dependent capture from accumulating.
            let wait = frame_pacing_wait(
                &mut budget,
                bps,
                produced.header.keyframe,
                produced.payload.len(),
                acknowledgements.len(),
            );
            if wait >= Duration::from_millis(100) {
                tracing::info!(
                    frame_seq = produced.header.seq,
                    keyframe = produced.header.keyframe,
                    payload_bytes = produced.payload.len(),
                    wait_ms = wait.as_millis(),
                    bitrate_bps = writer_bitrate.load(Ordering::Relaxed),
                    "desktop frame pacing delayed"
                );
            }
            if !wait.is_zero() {
                tokio::select! {
                    biased;
                    changed = repair.changed(&mut media_changes, generation) => {
                        if changed.is_err() { break 'writer; }
                        continue 'writer;
                    }
                    _ = tokio::time::sleep(wait) => {},
                }
            }
            // Admission follows the final selection: advancing this before
            // the pacing wait would lose track of frames collapsed afterward.
            if !chain.admit(&produced) {
                request_frame_repair(&latest_key_seq, &writer_idr, produced.header.seq);
                continue;
            }
            produced.header.send_ts_ms = writer_clock.now_ms();
            tracing::trace!(
                frame_seq = produced.header.seq,
                keyframe = produced.header.keyframe,
                payload_bytes = produced.payload.len(),
                bitrate_bps = writer_bitrate.load(Ordering::Relaxed),
                capture_encode_ms = produced
                    .header
                    .encode_done_ts_ms
                    .saturating_sub(produced.header.capture_ts_ms),
                encode_to_send_ms = produced
                    .header
                    .send_ts_ms
                    .saturating_sub(produced.header.encode_done_ts_ms),
                "desktop frame sending"
            );
            let outcome = tokio::select! {
                biased;
                changed = repair.changed(&mut media_changes, generation) => {
                    if changed.is_err() { break 'writer; }
                    continue 'writer;
                }
                outcome = send_frame(
                    &writer_conn, frame_route, produced, &mut rx, &mut acknowledgements,
                    FrameDelivery {
                        repair: repair.clone(), idr: writer_idr.clone(),
                        feedback: writer_feedback.clone(), latest_key_seq: latest_key_seq.clone(),
                        payload_receipts: writer_receipts.clone(),
                    },
                ) => outcome,
            };
            match outcome {
                SendOutcome::Sent => {
                    sent += 1;
                }
                SendOutcome::Obsolete => {
                    obsolete += 1;
                }
                SendOutcome::Superseded(newer) => {
                    superseded += 1;
                    pending = Some(newer);
                }
                SendOutcome::Done => {
                    while acknowledgements.join_next().await.is_some() {}
                    break 'writer;
                }
                SendOutcome::Failed => {
                    tracing::warn!(sent, superseded, "desktop frame writer ended");
                    break 'writer;
                }
            }
            if health.elapsed() >= Duration::from_secs(5) {
                tracing::info!(
                    sent,
                    superseded,
                    acknowledged,
                    obsolete,
                    failed_delivery,
                    pending_acknowledgements = acknowledgements.len(),
                    pending_media_frames = capture_admission.load(Ordering::Acquire),
                    keyframe_pending = repair.key_pending(),
                    bitrate_bps = writer_bitrate.load(Ordering::Relaxed),
                    delayed_delivery = writer_feedback.delayed.load(Ordering::Relaxed),
                    last_ack_ms = writer_feedback.last_ack_ms.load(Ordering::Relaxed),
                    "desktop sender health"
                );
                health = Instant::now();
            }
        }
    }.in_current_span());

    // Control loop: input + encoder steering + heartbeat, until the
    // peer goes away. `send` also carries DesktopEvent replies.
    let send_clock = clock.clone();
    let session_display = hello.display;
    // `read_frame` owns a partially consumed prefix/body and is intentionally
    // not cancellation-safe.  Keep it in one reader task while the session
    // loop also waits for native clipboard changes; canceling a `select!`
    // branch must never discard a half-read control frame.
    let mut watching = config.reverse_clipboard && !config.view_only && clipboard_supported;
    let clipboard_lifetime: crate::clipboard::NativeLifetime =
        Arc::new(native.begin().ok_or_else(native_closed)?);
    let mut clipboard = watching.then(|| {
        crate::clipboard::Worker::for_source(
            config.source.as_deref(),
            session_display,
            true,
            Some(clipboard_lifetime.clone()),
        )
    });
    let control = async {
        let (control_tx, mut control_rx) =
            mpsc::channel::<Result<DesktopControl, std::io::Error>>(16);
        let mut reader = JoinSet::new();
        reader.spawn(async move {
            loop {
                let result = read_frame::<_, DesktopControl>(&mut recv).await;
                let done = result.is_err();
                if control_tx.send(result).await.is_err() || done {
                    break;
                }
            }
        });
        let mut assembly = crate::clipboard::Assembly::default();
        let mut reverse = crate::clipboard::ReverseSender::default();
        loop {
            let read_started = Instant::now();
            enum ControlWork {
                Control(Result<DesktopControl, std::io::Error>),
                Clipboard(Option<crate::clipboard::ClipboardChange>),
                SendChunk,
            }
            let work = tokio::select! {
                result = control_rx.recv() => ControlWork::Control(result.unwrap_or_else(|| Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "control reader ended")))),
                change = async { match &mut clipboard { Some(worker) => worker.next_change().await, None => std::future::pending().await } }, if watching => ControlWork::Clipboard(change),
                _ = tokio::task::yield_now(), if reverse.sending() => ControlWork::SendChunk,
            };
            match work {
                ControlWork::Clipboard(Some(change)) => {
                    if let Some(event) = reverse.offer(change.text)
                        && write_control_reply(&mut send.0, &event).await.is_err()
                    {
                        break;
                    }
                }
                ControlWork::Clipboard(None) => {
                    watching = false;
                    clipboard = None;
                    reverse.invalidate_offer();
                    tracing::warn!("clipboard watcher unavailable; video and input remain active");
                }
                ControlWork::SendChunk => {
                    if let Some(event) = reverse.next_chunk()
                        && write_control_reply(&mut send.0, &event).await.is_err()
                    {
                        break;
                    }
                }
                ControlWork::Control(result) => match result {
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
                        let mut worker = if let Some(worker) = input.take() {
                            worker
                        } else {
                            let Some(running) = native.begin() else { break };
                            super::input::worker::InputWorker::new(
                                input_sink.take(),
                                Some(Arc::new(running)),
                            )
                        };
                        let seq = ev.seq;
                        let received_ms = send_clock.now_ms();
                        let input_started = Instant::now();
                        match tokio::time::timeout(FRAME_SEND_TIMEOUT, worker.inject(ev)).await {
                            Ok(Ok(())) => input = Some(worker),
                            Ok(Err(e)) => {
                                input = Some(worker);
                                tracing::warn!("input injection failed: {e}");
                                continue;
                            }
                            Err(_) => {
                                // The admitted backend may still be running.
                                // End this session instead of probing another
                                // OS sink with the old operation in flight.
                                tracing::warn!("input injection timed out; ending owning session");
                                break;
                            }
                        }
                        controls.input_refresh_until_ms.store(
                            send_clock.now_ms().saturating_add(INPUT_REFRESH_BURST_MS),
                            Ordering::Release,
                        );
                        controls
                            .input_refresh_pending
                            .store(true, Ordering::Release);
                        delivery_feedback
                            .inputs_handled
                            .fetch_add(1, Ordering::Relaxed);
                        delivery_feedback.max_input_inject_ms.fetch_max(
                            input_started.elapsed().as_millis() as u64,
                            Ordering::Relaxed,
                        );
                        tracing::trace!(target:"rds_desktop::input_timing", input_seq=seq,
                        received_ms, handled_ms=send_clock.now_ms(), inject_ms=input_started.elapsed().as_millis(),
                        "desktop input injected");
                        if acks {
                            let ack_started = Instant::now();
                            let ack = DesktopEvent::InputAck {
                                seq,
                                handled_ts_ms: send_clock.now_ms(),
                            };
                            if write_control_reply(&mut send.0, &ack).await.is_err() {
                                break;
                            }
                            tracing::trace!(target:"rds_desktop::input_timing", input_seq=seq,
                            ack_write_ms=ack_started.elapsed().as_millis(), "desktop input acknowledgement written");
                        }
                    }
                    Ok(DesktopControl::RequestIdr) => match media_repair.request() {
                        crate::media_repair::Request::Accepted => {
                            tracing::info!(
                                media_repair_generation = media_repair.generation(),
                                "desktop explicit media repair accepted"
                            );
                        }
                        crate::media_repair::Request::Coalesced => {
                            tracing::debug!(
                                "desktop repair already delivering an independent picture"
                            );
                        }
                        crate::media_repair::Request::Exhausted => {
                            tracing::warn!("desktop repair generation exhausted");
                            break;
                        }
                    },
                    Ok(DesktopControl::SetBitrate(bps)) => {
                        let bps = u64::from(bps.max(50_000)).min(ceiling);
                        controls.requested.store(bps, Ordering::Relaxed);
                    }
                    Ok(DesktopControl::Heartbeat { seq, ts_ms }) => {
                        tracing::trace!(target:"rds_desktop::control_timing", heartbeat_seq=seq,
                        read_wait_us=read_started.elapsed().as_micros(), "desktop heartbeat control read");
                        let reply_started = Instant::now();
                        let written = write_control_reply(
                            &mut send.0,
                            &DesktopEvent::Heartbeat { seq, ts_ms },
                        )
                        .await
                        .is_ok();
                        tracing::trace!(target:"rds_desktop::control_timing", heartbeat_seq=seq, written,
                        write_us=reply_started.elapsed().as_micros(), "desktop heartbeat reply write completed");
                        if !written {
                            break;
                        }
                    }
                    Ok(DesktopControl::FrameReceived {
                        seq,
                        digest,
                        obsolete,
                    }) => {
                        let Some(receipts) = &payload_receipts else {
                            tracing::warn!(
                                frame_seq = seq,
                                "payload receipt on a legacy desktop session refused"
                            );
                            break;
                        };
                        let accepted = receipts.confirm(seq, &digest, obsolete);
                        tracing::trace!(target:"rds_desktop::frame_timing",frame_seq=seq,accepted,obsolete,
                        "desktop validated payload receipt observed");
                    }
                    Ok(DesktopControl::ClipboardRequest { id, format: _ }) => {
                        // Reverse clipboard is an explicit capability AND a
                        // control scope; view-only cannot exfiltrate a seat's
                        // clipboard even when it requests the V5 greeting.
                        if !config.reverse_clipboard || config.view_only {
                            break;
                        }
                        let result = if watching {
                            reverse.request(id)
                        } else {
                            Err(ClipboardErrorCode::Unavailable)
                        };
                        if let Err(code) = result
                            && write_control_reply(
                                &mut send.0,
                                &DesktopEvent::ClipboardError { id, code },
                            )
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(DesktopControl::ClipboardChunk {
                        id,
                        offset,
                        total,
                        data,
                    }) => {
                        if config.view_only {
                            tracing::warn!("view-only clipboard refused");
                            break;
                        }
                        if !clipboard_supported {
                            if write_control_reply(
                                &mut send.0,
                                &DesktopEvent::ClipboardError {
                                    id,
                                    code: ClipboardErrorCode::Unavailable,
                                },
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                        let text = match assembly.push(id, offset, total, data) {
                            Ok(text) => text,
                            Err(error) => {
                                tracing::warn!(%error,"clipboard transfer refused");
                                break;
                            }
                        };
                        if let Some(text) = text {
                            let publish_started = Instant::now();
                            tracing::info!(
                                transfer_id = id,
                                bytes = total,
                                "desktop clipboard publication started"
                            );
                            match tokio::time::timeout(
                                Duration::from_secs(2),
                                clipboard
                                    .get_or_insert_with(|| {
                                        crate::clipboard::Worker::for_source(
                                            config.source.as_deref(),
                                            session_display,
                                            false,
                                            Some(clipboard_lifetime.clone()),
                                        )
                                    })
                                    .publish(text),
                            )
                            .await
                            {
                                Ok(Ok(())) => {
                                    tracing::info!(
                                        transfer_id = id,
                                        bytes = total,
                                        publish_ms = publish_started.elapsed().as_millis(),
                                        "desktop clipboard publication completed"
                                    );
                                    let reply_started = Instant::now();
                                    if write_control_reply(
                                        &mut send.0,
                                        &DesktopEvent::ClipboardReady { id, bytes: total },
                                    )
                                    .await
                                    .is_err()
                                    {
                                        break;
                                    }
                                    tracing::info!(
                                        transfer_id = id,
                                        bytes = total,
                                        reply_ms = reply_started.elapsed().as_millis(),
                                        "desktop clipboard ready reply written"
                                    );
                                }
                                result => {
                                    tracing::warn!(error=?result,"clipboard publication failed; ending control before paste input");
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error,"desktop control ended");
                        tracing::debug!(target:"rds_desktop::control_timing", error_kind=?error.kind(),
                        read_wait_us=read_started.elapsed().as_micros(), "desktop control reader ended");
                        break;
                    }
                },
            }
        }
    };

    let result = tokio::select! {
        _ = control => Ok(()),
        res = workers.join_next() => match res {
            Some(Ok(())) | None => Ok(()),
            Some(Err(e)) => Err(DesktopError::Io(std::io::Error::other(e.to_string()))),
        },
    };
    native.close();
    drop(capture_stop);
    let _ = send.0.reset(0u32.into());
    // Normal exit joins the asynchronous siblings. Cancellation during this
    // shutdown still drops the JoinSet and aborts its remaining children.
    workers.shutdown().await;
    capture.abort_all();
    drop(input);
    drop(input_sink);
    if let Some(worker) = &mut clipboard {
        worker.close().await;
    }
    drop(clipboard);
    drop(clipboard_lifetime);
    // A running spawn_blocking call cannot be aborted. Its guard keeps the
    // original native authority reserved until the OS object is cleaned up.
    native.drain().await;
    while let Some(result) = capture.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "desktop capture worker failed during shutdown");
        }
    }
    result
}

async fn write_control_reply<W: AsyncWrite + Unpin>(
    send: &mut W,
    event: &DesktopEvent,
) -> std::io::Result<()> {
    let reply_class = match event {
        DesktopEvent::InputAck { .. } => "input_ack",
        DesktopEvent::Heartbeat { .. } => "heartbeat",
        DesktopEvent::ClipboardReady { .. } => "clipboard_ready",
        DesktopEvent::ClipboardOffer { .. } => "clipboard_offer",
        DesktopEvent::ClipboardChunk { .. } => "clipboard_chunk",
        DesktopEvent::ClipboardError { .. } => "clipboard_error",
    };
    match tokio::time::timeout(CONTROL_REPLY_TIMEOUT, write_frame(send, event)).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                reply_class,
                "desktop control reply deadline exceeded; ending session"
            );
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "desktop control reply deadline exceeded",
            ))
        }
    }
}

/// A canceled control reply must not end with a partial, apparently clean FIN.
struct SessionSend(SendStream);

impl Drop for SessionSend {
    fn drop(&mut self) {
        let _ = self.0.reset(0u32.into());
    }
}

/// Platform capture producer, or an empty source when native capture is
/// unavailable. Empty production ends the owning session's task group.
fn platform_producer(
    _display: u32,
    _interval: Duration,
    _height: Option<u32>,
    _extent: Option<(u32, u32)>,
) -> Box<dyn FrameProducer> {
    #[cfg(all(target_os = "linux", feature = "x11"))]
    match x11::X11Producer::new(_display, _interval, _height, _extent) {
        Ok(p) => return Box::new(p),
        Err(e) => tracing::warn!("capture init failed: {e}"),
    }
    Box::new(NullProducer)
}

/// Producer that yields nothing; the owning session ends its task group.
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

/// Idle wakeups start a new slot immediately. Scheduler jitter smaller than a
/// whole slot is not encoder starvation and must not reduce the bitrate.
pub(crate) fn advance_cadence(
    next_due: &mut Instant,
    interval: Duration,
    now: Instant,
    resumed: bool,
) -> bool {
    if resumed {
        *next_due = now;
        return false;
    }
    *next_due += interval;
    let lateness = now.saturating_duration_since(*next_due);
    if lateness >= interval {
        *next_due = now;
        return true;
    }
    if now > *next_due {
        *next_due = now;
    }
    false
}

pub(crate) fn resume_cadence(next_due: &mut Instant, interval: Duration, now: Instant) {
    *next_due = (*next_due).max(now.checked_sub(interval).unwrap_or(now));
}

fn frame_pacing_wait(
    budget: &mut f64,
    bytes_per_second: f64,
    keyframe: bool,
    payload_bytes: usize,
    pending_receipts: usize,
) -> Duration {
    let cost = payload_bytes as f64 + 64.0;
    let cold_key = keyframe && payload_bytes <= 64 * 1024 && pending_receipts == 0;
    if cost <= *budget {
        *budget -= cost;
        return Duration::ZERO;
    }
    let wait = if cold_key {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(((cost - *budget) / bytes_per_second).min(0.5))
    };
    *budget = (*budget - cost).max(-bytes_per_second * 0.5);
    wait
}

impl FrameProducer for SyntheticProducer {
    fn resume_after_backpressure(&mut self) {
        resume_cadence(&mut self.next_due, self.interval, Instant::now());
    }
    fn produce(
        &mut self,
        seq: u64,
        controls: &ProducerControls,
        clock: &SessionClock,
    ) -> Option<Produced> {
        if advance_cadence(&mut self.next_due, self.interval, Instant::now(), false) {
            controls.deadline_misses.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(sleep) = self.next_due.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep);
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
        output_height: Option<u32>,
        last_work: Duration,
        skipped: bool,
        polling: Option<(
            std::sync::Weak<crate::producer_signal::Wake>,
            Arc<crate::producer_signal::polling::Pulse>,
        )>,
    }

    impl X11Producer {
        pub fn new(
            display: u32,
            interval: Duration,
            requested: Option<u32>,
            extent: Option<(u32, u32)>,
        ) -> Result<Self, DesktopError> {
            let output_height = match requested {
                Some(0) => None,
                Some(height) => Some(height),
                None => match std::env::var("RDS_DESKTOP_OUTPUT_HEIGHT") {
                    Ok(value) => Some(
                        value
                            .parse::<u32>()
                            .ok()
                            .filter(|height| (16..=4320).contains(height))
                            .ok_or_else(|| {
                                DesktopError::Capture(
                                    "RDS_DESKTOP_OUTPUT_HEIGHT must be 16..=4320".into(),
                                )
                            })?,
                    ),
                    Err(std::env::VarError::NotPresent) => None,
                    Err(_) => {
                        return Err(DesktopError::Capture(
                            "RDS_DESKTOP_OUTPUT_HEIGHT must be valid Unicode".into(),
                        ));
                    }
                },
            };
            let capturer = X11Capturer::new(display)?;
            if let Some(extent) = extent
                && capturer
                    .displays()
                    .iter()
                    .find(|info| info.index == display)
                    .is_none_or(|info| (info.width, info.height) != extent)
            {
                return Err(DesktopError::Capture(
                    "display changed during session admission".into(),
                ));
            }
            let fps = 1.0 / interval.as_secs_f32();
            let encoder = H264Encoder::new(4_000_000, fps)?;
            Ok(Self {
                capturer,
                encoder,
                interval,
                next_due: Instant::now(),
                output_height,
                last_work: Duration::ZERO,
                skipped: false,
                polling: None,
            })
        }
    }

    impl X11Producer {
        /// Damage and steering wake the same original native poll owner.
        /// Retain the existing one-second unchanged-picture refresh deadline.
        fn idle_wait(
            &mut self,
            controls: &ProducerControls,
            clock: &SessionClock,
        ) -> Result<bool, DesktopError> {
            use std::os::fd::AsFd;
            if self
                .polling
                .as_ref()
                .is_none_or(|(owner, _)| !owner.ptr_eq(&Arc::downgrade(&controls.wake)))
            {
                self.polling = Some((Arc::downgrade(&controls.wake), controls.wake.polling()?));
            }
            let pulse = self
                .polling
                .as_ref()
                .map(|(_, pulse)| pulse.clone())
                .ok_or_else(|| DesktopError::Capture("native poll wake missing".into()))?;
            pulse.drain()?;
            let input_active = || controls.input_refresh_active(clock.now_ms());
            if controls.wake.snapshot().is_none() {
                return Err(DesktopError::Capture("native producer stopped".into()));
            }
            if input_active() || self.capturer.changed() || controls.idr.load(Ordering::Acquire) {
                return Ok(false);
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while let Some(wait) = deadline.checked_duration_since(Instant::now()) {
                let changed = self.capturer.wait_for_change(wait, pulse.as_fd())?;
                pulse.drain()?;
                if controls.wake.snapshot().is_none() {
                    return Err(DesktopError::Capture("native producer stopped".into()));
                }
                if changed || controls.idr.load(Ordering::Acquire) || input_active() {
                    break;
                }
            }
            Ok(true)
        }
    }

    impl FrameProducer for X11Producer {
        fn resume_after_backpressure(&mut self) {
            let interval = self
                .interval
                .max(self.last_work)
                .min(Duration::from_millis(500));
            resume_cadence(&mut self.next_due, interval, Instant::now());
        }
        fn preserves_reference(&self) -> bool {
            self.skipped
        }
        fn produce(
            &mut self,
            seq: u64,
            controls: &ProducerControls,
            clock: &SessionClock,
        ) -> Option<Produced> {
            let resumed = match self.idle_wait(controls, clock) {
                Ok(resumed) => resumed,
                Err(error) => {
                    if controls.wake.snapshot().is_some() {
                        tracing::warn!(%error,"native capture wait ended");
                    }
                    return None;
                }
            };
            // Capture/conversion has a fixed CPU cost that lowering encoded
            // bitrate cannot remove. Honor max_fps while using the achievable
            // cadence, rather than collapsing bitrate for every slow frame.
            let interval = self.interval.max(self.last_work);
            if advance_cadence(&mut self.next_due, interval, Instant::now(), resumed) {
                controls.deadline_misses.fetch_add(1, Ordering::Relaxed);
            }
            if !controls.wake.until(self.next_due) {
                return None;
            }
            let capture_ts_ms = clock.now_ms();
            self.skipped = false;
            let work_started = Instant::now();
            if controls.idr.swap(false, Ordering::Relaxed) {
                self.encoder.request_idr();
            }
            self.encoder.set_bitrate(
                controls
                    .bitrate
                    .load(Ordering::Relaxed)
                    .min(u64::from(u32::MAX)) as u32,
            );
            let encoder = &mut self.encoder;
            let output_height = self.output_height;
            let captured = self.capturer.capture_with(|raw| {
                let scaled = output_height
                    .filter(|height| *height < raw.height)
                    .map(|height| crate::scaling::downscale(raw, height))
                    .transpose()?;
                let raw = scaled.as_ref().map(crate::BgraFrame::from).unwrap_or(raw);
                Ok::<_, DesktopError>((raw.width, raw.height, encoder.encode_bgra(raw)))
            });
            let (width, height, encoded) = match captured {
                Ok(Ok(frame)) => frame,
                Ok(Err(error)) | Err(error) => {
                    tracing::warn!(%error, "capture or scaling failed");
                    return None;
                }
            };
            self.skipped = encoded.as_ref().is_ok_and(|frame| frame.data.is_empty());
            self.last_work = work_started.elapsed();
            match encoded {
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

/// Conservative reference contract: every delta may depend on its predecessor.
/// A producer must identify independent keyframes from the encoded bitstream.
#[derive(Default)]
struct FrameChain {
    next: Option<u64>,
    last_key_seq: Option<u64>,
}

impl FrameChain {
    fn failed_receipt(&mut self, seq: u64) {
        // The receipt worker already owns the one repair request. An older
        // result joining after independent recovery cannot break that chain.
        if self.last_key_seq.is_none_or(|key| key <= seq) {
            self.next = None;
        }
    }
    fn admit(&mut self, produced: &Produced) -> bool {
        let header = &produced.header;
        if produced.payload.is_empty()
            || header.seq == u64::MAX
            || (!header.keyframe && self.next != Some(header.seq))
        {
            self.next = None;
            return false;
        }
        self.next = header.seq.checked_add(1);
        if header.keyframe {
            self.last_key_seq = Some(header.seq);
        }
        true
    }
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
    /// Receiver already recovered past this predecessor; continue the writer.
    Obsolete,
    /// A fresher decodable frame supersedes — send it next.
    Superseded(AdmittedFrame),
    /// Producer closed mid-send; the final frame was finished.
    Done,
    /// Transport failure — the writer ends.
    Failed,
}

fn obsolete_write(error: &std::io::Error) -> bool {
    matches!(error.get_ref().and_then(|cause| cause.downcast_ref::<rds_net::WriteError>()),
        Some(rds_net::WriteError::Stopped(code)) if *code == rds_core::DESKTOP_FRAME_OBSOLETE.into())
}

fn obsolete_send(
    sending: &mut FrameSend,
    produced: &Produced,
    delivery: &FrameDelivery,
) -> SendOutcome {
    sending.finished = true;
    delivery.feedback.obsolete.fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        frame_seq = produced.header.seq,
        payload_bytes = produced.payload.len(),
        "desktop obsolete frame stopped during write"
    );
    SendOutcome::Obsolete
}

/// Send one frame on its own tagged uni stream, aborting mid-write if
/// an independent keyframe lands. Otherwise finish the reference on which
/// the next delta may depend, retaining partial-write progress.
async fn send_frame(
    conn: &Connection,
    route: rds_core::UniHello,
    produced: AdmittedFrame,
    rx: &mut mpsc::Receiver<AdmittedFrame>,
    acknowledgements: &mut JoinSet<(u64, FrameReceipt)>,
    delivery: FrameDelivery,
) -> SendOutcome {
    match tokio::time::timeout(
        FRAME_SEND_TIMEOUT,
        send_frame_inner(conn, route, produced, rx, acknowledgements, delivery),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => {
            tracing::debug!("frame send deadline exceeded");
            SendOutcome::Failed
        }
    }
}

fn drain_frame_receipts(
    receipts: &mut JoinSet<(u64, FrameReceipt)>,
    chain: &mut FrameChain,
    idr: &crate::ProducerSignal,
    acknowledged: &mut u64,
    obsolete: &mut u64,
    failed: &mut u64,
) {
    while let Some(result) = receipts.try_join_next() {
        match result {
            Ok((_, FrameReceipt::Delivered)) => *acknowledged += 1,
            Ok((_, FrameReceipt::Obsolete)) => *obsolete += 1,
            Ok((seq, FrameReceipt::Failed)) => {
                *failed += 1;
                chain.failed_receipt(seq);
            }
            _ => {
                *failed += 1;
                chain.next = None;
                idr.store(true, Ordering::Relaxed);
            }
        }
    }
}

async fn send_frame_inner(
    conn: &Connection,
    route: rds_core::UniHello,
    produced: AdmittedFrame,
    rx: &mut mpsc::Receiver<AdmittedFrame>,
    acknowledgements: &mut JoinSet<(u64, FrameReceipt)>,
    delivery: FrameDelivery,
) -> SendOutcome {
    let transfer_started = Instant::now();
    let AdmittedFrame {
        produced,
        permit,
        generation,
        mut key,
    } = produced;
    let payload_receipts = delivery.payload_receipts.clone();
    let mut payload_ticket = match &payload_receipts {
        Some(receipts) => match receipts.register(produced.header.seq) {
            Some(ticket) => Some(ticket),
            None => {
                tracing::warn!(
                    frame_seq = produced.header.seq,
                    "desktop payload receipt admission failed"
                );
                return SendOutcome::Failed;
            }
        },
        None => None,
    };
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
    if let Err(e) = stream.set_priority(MEDIA_STREAM_PRIORITY) {
        tracing::debug!("frame stream priority failed: {e}");
    }
    // Every uni stream leads with its UniHello tag — the receiver's
    // per-connection demux routes on it. Per-session routes keep a stale
    // stream out of any replacement session's inbox.
    if let Err(e) = write_frame(&mut *stream, &route).await {
        if obsolete_write(&e) {
            return obsolete_send(&mut sending, &produced, &delivery);
        }
        tracing::debug!("frame tag write failed: {e}");
        return SendOutcome::Failed;
    }
    if let Err(e) = write_frame(&mut *stream, &produced.header).await {
        if obsolete_write(&e) {
            return obsolete_send(&mut sending, &produced, &delivery);
        }
        tracing::debug!("frame header write failed: {e}");
        return SendOutcome::Failed;
    }
    let mut payload_hasher = payload_receipts.as_ref().map(|_| blake3::Hasher::new());
    let outcome = match send_payload(stream, &produced, rx, payload_hasher.as_mut()).await {
        Ok(PayloadOutcome::Abandoned(next)) => {
            return SendOutcome::Superseded(next);
        }
        Ok(PayloadOutcome::Sent) => SendOutcome::Sent,
        Ok(PayloadOutcome::Superseded(next)) => SendOutcome::Superseded(next),
        Ok(PayloadOutcome::ProducerEnded) => SendOutcome::Done,
        Err(e) => {
            if obsolete_write(&e) {
                return obsolete_send(&mut sending, &produced, &delivery);
            }
            tracing::debug!("frame send failed: {e}");
            return SendOutcome::Failed;
        }
    };
    // Superseded also means the current payload was fully written: its
    // successor waits behind the current reference. Only Abandoned returned
    // early above. Install every completed payload's proof before exposing FIN.
    if let (Some(receipts), Some(hasher)) = (&payload_receipts, payload_hasher) {
        let digest = *hasher.finalize().as_bytes();
        if !receipts.set_digest(produced.header.seq, digest) {
            tracing::debug!(
                frame_seq = produced.header.seq,
                "payload receipt digest was retired before send completed"
            );
        }
    }
    if let Err(e) = stream.finish() {
        // finish() reports an erased ClosedStream. Only a ready, exact peer
        // disposition can classify it as obsolete; never wait on other errors.
        if matches!(tokio::time::timeout(Duration::ZERO, stream.stopped()).await,
            Ok(Ok(Some(code))) if code == rds_core::DESKTOP_FRAME_OBSOLETE.into())
        {
            return obsolete_send(&mut sending, &produced, &delivery);
        }
        tracing::debug!("frame finish failed: {e}");
        return SendOutcome::Failed;
    }
    let seq = produced.header.seq;
    let keyframe = produced.header.keyframe;
    let payload_bytes = produced.payload.len();
    let delay_budget = delivery_delay_budget(conn.current_path_stats());
    let FrameDelivery {
        repair,
        idr,
        feedback,
        latest_key_seq,
        payload_receipts: _,
    } = delivery;
    // Retain the reset-on-drop owner until delivery is acknowledged. The
    // bounded task group is owned by this writer; cancellation resets its
    // outstanding frames without closing unrelated connection services.
    acknowledgements.spawn(async move {
        let started = Instant::now();
        let deadline = if keyframe {KEYFRAME_ACK_TIMEOUT} else {FRAME_ACK_TIMEOUT};
        let mut late = None;
        let result = {
            let transport = async {
                match sending.stream.stopped().await {
                    Ok(None) => Ok(ReceiptEvidence::Transport),
                    Ok(Some(code)) if code == rds_core::DESKTOP_FRAME_OBSOLETE.into() => Ok(ReceiptEvidence::Obsolete),
                    Ok(Some(_)) => Err("peer stopped frame".to_owned()),
                    Err(error) => Err(error.to_string()),
                }
            };
            let receipt = tokio::time::timeout(deadline, crate::receipts::wait(&mut payload_ticket, transport));
            tokio::pin!(receipt);
            tokio::select! {
                result = &mut receipt => result,
                _ = tokio::time::sleep(delay_budget) => {
                    feedback.mark_delayed(payload_bytes);
                    late = Some(LateReceipt::new(feedback.clone()));
                    tracing::warn!(frame_seq=seq,keyframe,payload_bytes,delay_budget_ms=delay_budget.as_millis(),"desktop frame delivery delayed");
                    receipt.await
                }
            }
        };
        feedback.last_ack_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        let acknowledged = if repair.generation() != generation {
            feedback.obsolete.fetch_add(1, Ordering::Relaxed);
            FrameReceipt::Obsolete
        } else { match result {
            Ok(Ok(evidence @ (ReceiptEvidence::Transport | ReceiptEvidence::Payload))) => {
                if evidence == ReceiptEvidence::Payload {
                    // The reader consumed EOF and proved the exact body. Retire
                    // redundant retransmission state without waiting for FIN ACK.
                    let _ = sending.stream.reset(rds_core::DESKTOP_FRAME_RECEIVED.into());
                }
                sending.finished = true;
                if let Some(key) = key.as_mut() { key.confirmed = true; }
                feedback.acknowledged(payload_bytes, transfer_started.elapsed(), delay_budget);
                if late.is_some() {
                    // Complete the soft-delay record at ordinary diagnostic
                    // verbosity; the initial crossing alone hid its duration.
                    tracing::info!(
                        frame_seq = seq,
                        keyframe,
                        payload_bytes,
                        enqueue_ms = started.duration_since(transfer_started).as_millis(),
                        ack_ms = started.elapsed().as_millis(),
                        transfer_ms = transfer_started.elapsed().as_millis(),
                        receipt_evidence=?evidence,
                        "desktop delayed frame delivery acknowledged"
                    );
                }
                tracing::trace!(target:"rds_desktop::frame_timing",frame_seq=seq,payload_bytes,ack_ms=started.elapsed().as_millis(),receipt_evidence=?evidence,"desktop frame delivery acknowledged");
                FrameReceipt::Delivered
            }
            Ok(Ok(ReceiptEvidence::Obsolete)) => {
                if payload_ticket.is_some() { let _ = sending.stream.reset(rds_core::DESKTOP_FRAME_OBSOLETE.into()); }
                sending.finished = true;
                feedback.obsolete.fetch_add(1, Ordering::Relaxed);
                tracing::info!(frame_seq=seq,payload_bytes,ack_ms=started.elapsed().as_millis(),"desktop obsolete frame receipt");
                FrameReceipt::Obsolete
            }
            result => {
                feedback.failed.fetch_add(1, Ordering::Relaxed);
                request_frame_repair(&latest_key_seq, &idr, seq);
                tracing::warn!(frame_seq=seq,keyframe,payload_bytes,ack_ms=started.elapsed().as_millis(),outcome=?result,"desktop frame delivery unconfirmed");
                FrameReceipt::Failed
            }
        }};
        drop(late);
        // Capture the complete reset owner, including its Drop implementation,
        // rather than allowing disjoint field captures in the async closure.
        drop(sending);
        drop(payload_ticket);
        drop(key);
        drop(permit);
        (seq, acknowledged)
    }.in_current_span());
    outcome
}

/// Completed payloads may FIN; abandoned ones must RESET.
enum PayloadOutcome<T> {
    Sent,
    Superseded(T),
    ProducerEnded,
    Abandoned(T),
}

/// Hash payload bytes as the async writer actually accepts them. Receipt
/// hashing used to scan the complete payload before the write and therefore
/// paid a second Full HD memory pass on every frame.
struct DigestingWriter<'a, W> {
    inner: &'a mut W,
    hasher: Option<&'a mut blake3::Hasher>,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for DigestingWriter<'_, W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut *this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(written)) => {
                if let Some(hasher) = this.hasher.as_deref_mut() {
                    hasher.update(&buf[..written]);
                }
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_shutdown(cx)
    }
}

async fn send_payload<W: AsyncWrite + Unpin, T: Borrow<Produced>>(
    stream: &mut W,
    produced: &Produced,
    rx: &mut mpsc::Receiver<T>,
    hasher: Option<&mut blake3::Hasher>,
) -> std::io::Result<PayloadOutcome<T>> {
    let mut stream = DigestingWriter {
        inner: stream,
        hasher,
    };
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
            Some(newer) if produced.header.keyframe || !newer.borrow().header.keyframe => {
                writing.await?;
                Ok(PayloadOutcome::Superseded(newer))
            }
            Some(newer) => Ok(PayloadOutcome::Abandoned(newer)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn pacing_feedback_waits_for_its_first_observation_window() {
        let started = tokio::time::Instant::now();
        let mut tick = pacing_interval();
        tick.tick().await;
        assert_eq!(started.elapsed(), PACING_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn pacing_feedback_does_not_compress_pressure_samples_after_a_stall() {
        let mut tick = pacing_interval();
        tick.tick().await;
        // Wake shortly before an original schedule boundary. Skipping old
        // ticks must not turn this single late observation into two signals
        // of sustained delivery pressure only ten milliseconds apart.
        tokio::time::advance(Duration::from_millis(740)).await;
        tick.tick().await;
        let observed_at = tokio::time::Instant::now();
        let mut pressure = DeliveryPressure::default();
        assert!(!pressure.sample(1, 0, 0, false, false));
        tick.tick().await;
        assert!(
            observed_at.elapsed() >= PACING_INTERVAL,
            "delivery pressure was sampled again after {:?}",
            observed_at.elapsed()
        );
        assert!(pressure.sample(1, 0, 0, false, false));
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_clipboard_reply_obeys_control_deadline_after_partial_header() {
        let (mut send, mut receive) = tokio::io::duplex(2);
        let started = tokio::time::Instant::now();
        let result = write_control_reply(
            &mut send,
            &DesktopEvent::ClipboardReady { id: 7, bytes: 4096 },
        )
        .await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), CONTROL_REPLY_TIMEOUT);
        drop(send); // A real serving session resets SessionSend at this boundary.
        use tokio::io::AsyncReadExt;
        let mut partial = Vec::new();
        receive.read_to_end(&mut partial).await.unwrap();
        assert_eq!(partial.len(), 2, "the control frame was partially written");
    }

    #[tokio::test]
    async fn timely_clipboard_reply_preserves_following_input_ack_and_heartbeat() {
        let (mut send, mut receive) = tokio::io::duplex(256);
        for event in [
            DesktopEvent::ClipboardReady { id: 7, bytes: 4096 },
            DesktopEvent::InputAck {
                seq: 8,
                handled_ts_ms: 9,
            },
            DesktopEvent::Heartbeat { seq: 10, ts_ms: 11 },
        ] {
            write_control_reply(&mut send, &event).await.unwrap();
            let received: DesktopEvent = read_frame(&mut receive).await.unwrap();
            match (received, event) {
                (
                    DesktopEvent::ClipboardReady { id: a, bytes: b },
                    DesktopEvent::ClipboardReady { id: c, bytes: d },
                ) => assert_eq!((a, b), (c, d)),
                (
                    DesktopEvent::InputAck {
                        seq: a,
                        handled_ts_ms: b,
                    },
                    DesktopEvent::InputAck {
                        seq: c,
                        handled_ts_ms: d,
                    },
                ) => assert_eq!((a, b), (c, d)),
                (
                    DesktopEvent::Heartbeat { seq: a, ts_ms: b },
                    DesktopEvent::Heartbeat { seq: c, ts_ms: d },
                ) => assert_eq!((a, b), (c, d)),
                _ => panic!("control reply changed type or order"),
            }
        }
    }

    #[test]
    fn accepted_input_wake_survives_capture_backpressure_then_expires() {
        let controls = ProducerControls::new(4_000_000);
        assert!(!controls.input_refresh_active(100));
        controls
            .input_refresh_until_ms
            .store(350, Ordering::Release);
        controls
            .input_refresh_pending
            .store(true, Ordering::Release);
        // Admission remains blocked beyond the original 250 ms hint.
        assert!(controls.input_refresh_active(2_000));
        assert!(!controls.input_refresh_pending.load(Ordering::Acquire));
        assert!(controls.input_refresh_active(2_249));
        assert!(!controls.input_refresh_active(2_250));
        assert!(!controls.input_refresh_active(3_000));
        assert!(controls.idr.swap(false, Ordering::Relaxed));
        controls
            .input_refresh_pending
            .store(true, Ordering::Release);
        assert!(controls.input_refresh_active(3_000));
        assert!(!controls.idr.load(Ordering::Relaxed));
        assert!(!controls.input_refresh_active(3_250));
    }

    #[test]
    fn one_blocked_receipt_does_not_repeat_penalties_while_capture_is_paused() {
        let mut pressure = DeliveryPressure::default();
        let mut controller = BitrateController::new(4_000_000, 8_000_000);
        controller.step_with_delivery(Some(path(1000, 0, 140, 0)), 0, false, true);
        for tick in 0..16 {
            let impaired = pressure.sample(1, 0, 0, false, false);
            controller.step_with_delivery(Some(path(1001 + tick, 0, 140, 0)), 0, impaired, false);
        }
        assert_eq!(
            controller.current(),
            2_800_000,
            "a single outstanding keyframe cannot repeatedly penalize future frames that admission has paused"
        );
        assert!(
            pressure.sample(1, 0, 0, true, true),
            "a new hard failure must still react"
        );
    }

    #[test]
    fn delivery_pressure_requires_ongoing_blockage_and_preserves_hard_failures() {
        let mut pressure = DeliveryPressure::default();
        assert!(!pressure.sample(1, 0, 0, true, false));
        assert!(!pressure.sample(1, 0, 0, false, false));
        assert!(pressure.sample(1, 0, 0, false, false));
        assert!(!pressure.sample(1, 0, 0, false, false));
        assert!(!pressure.sample(0, 0, 0, false, false));
        assert!(!pressure.sample(1, 0, 0, false, false));
        assert!(!pressure.sample(1, 0, 0, true, false));
        assert!(
            pressure.sample(0, 0, 0, true, true),
            "hard failure must react immediately"
        );
    }

    #[test]
    fn sustained_substantial_delayed_payload_reduces_load_with_fresh_receipts() {
        let mut pressure = DeliveryPressure::default();
        assert!(!pressure.sample(0, 1, MIN_SOFT_DELIVERY_LOAD_BYTES, true, false));
        assert!(pressure.sample(0, 1, MIN_SOFT_DELIVERY_LOAD_BYTES, true, false));
        assert!(!pressure.sample(0, 0, 0, true, false));
        // A tiny-payload observation ends the sustained-load sequence.
        assert!(!pressure.sample(0, 1, MIN_SOFT_DELIVERY_LOAD_BYTES, true, false));
        assert!(!pressure.sample(0, 1, 1024, true, false));
        assert!(!pressure.sample(0, 1, MIN_SOFT_DELIVERY_LOAD_BYTES, true, false));
    }

    #[test]
    fn repeated_small_delayed_receipts_do_not_destroy_encoder_quality() {
        let mut pressure = DeliveryPressure::default();
        let mut controller = BitrateController::new(4_000_000, 8_000_000);
        let feedback = DeliveryFeedback::default();
        let (mut last_count, mut last_bytes) = (0, 0);
        for tick in 0..240 {
            // One small desktop update per pacing observation. Delivery
            // progresses despite jitter; reducing the encoder cannot fix
            // the transit time of an approximately one-packet update.
            feedback.mark_delayed(1024);
            let count = feedback.delayed.load(Ordering::Relaxed);
            let bytes = feedback.delayed_bytes.load(Ordering::Relaxed);
            let impaired = pressure.sample(0, count - last_count, bytes - last_bytes, true, false);
            (last_count, last_bytes) = (count, bytes);
            controller.step_with_delivery(
                Some(path(1000 + tick * 10, 0, 140, 0)),
                0,
                impaired,
                true,
            );
        }
        assert!(
            controller.current() >= 4_000_000,
            "timing-only pressure on tiny updates must not drive Full HD to the 100 kbps floor"
        );
    }

    #[test]
    fn recovered_jitter_does_not_collapse_a_clean_delivering_stream() {
        let mut controller = BitrateController::new(4_000_000, 8_000_000);
        let mut pressure = DeliveryPressure::default();
        for tick in 0..240 {
            // Roughly one percent of a frame-rate stream has a soft delay,
            // while receipts continue. This cannot justify the 100 kbps floor.
            let late = u64::from(tick % 20 == 0);
            let impaired = pressure.sample(late, 0, 0, true, false);
            controller.step_with_delivery(
                Some(path(1000 + tick * 100, 0, 140, 0)),
                0,
                impaired,
                true,
            );
        }
        assert_eq!(controller.current(), 8_000_000);
    }

    #[tokio::test]
    async fn canceling_a_late_receipt_releases_its_current_pressure() {
        let feedback = Arc::new(DeliveryFeedback::default());
        let task_feedback = feedback.clone();
        let (tx, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _late = LateReceipt::new(task_feedback);
            tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        assert_eq!(feedback.late_pending.load(Ordering::Relaxed), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(feedback.late_pending.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn idle_wakes_and_subslot_jitter_do_not_collapse_bitrate() {
        let interval = Duration::from_millis(16);
        let mut due = Instant::now();
        let mut controller = BitrateController::new(4_000_000, 8_000_000);
        for _ in 0..64 {
            let now = due + Duration::from_secs(1);
            let missed = advance_cadence(&mut due, interval, now, true);
            assert_eq!(due, now, "damage wake must capture immediately");
            assert!(!missed, "idle wake must not count as encoder starvation");
            controller.step(None, u64::from(missed));
            let now = due + interval + Duration::from_micros(100);
            assert!(!advance_cadence(&mut due, interval, now, false));
        }
        assert_eq!(controller.current(), 4_000_000);
        let now = due + interval * 3;
        assert!(advance_cadence(&mut due, interval, now, false));
        assert_eq!(due, now, "real starvation starts a fresh schedule");
        assert!(controller.step(None, 1) < 4_000_000);
    }

    #[test]
    fn software_work_slower_than_max_fps_preserves_network_bitrate() {
        let maximum_fps_interval = Duration::from_millis(16);
        let mut due = Instant::now();
        let mut controller = BitrateController::new(4_000_000, 8_000_000);
        for work in [22, 24, 55, 3].into_iter().cycle().take(64) {
            let work = Duration::from_millis(work);
            let now = due + work;
            let missed = advance_cadence(&mut due, maximum_fps_interval.max(work), now, false);
            assert!(
                !missed,
                "known capture/codec cost is an achievable cadence, not network congestion"
            );
            assert!(due >= now);
            controller.step(None, u64::from(missed));
        }
        assert_eq!(controller.current(), 4_000_000);
    }

    #[test]
    fn admission_pause_does_not_report_encoder_starvation() {
        let now = Instant::now();
        let interval = Duration::from_millis(16);
        let mut due = now - Duration::from_secs(2);
        // Exercise the production resume/advance functions on one explicit
        // clock. OS preemption between two calls is actual scheduling delay,
        // not proof that admission itself counted as encoder starvation.
        resume_cadence(&mut due, interval, now);
        assert!(!advance_cadence(&mut due, interval, now, false));
        assert_eq!(due, now);
        // Real lateness without a deliberate admission pause still reports.
        assert!(advance_cadence(
            &mut due,
            interval,
            now + interval * 3,
            false
        ));
    }

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

    async fn partial_write(closed: bool, keyframe: bool) {
        use std::future::{Future, poll_fn};
        use std::task::Poll;
        use tokio::io::AsyncReadExt;

        tokio::time::timeout(Duration::from_secs(3), async {
            let (mut writer, mut reader) = tokio::io::duplex(64);
            let (tx, mut rx) = mpsc::channel(1);
            let frame = produced(0, keyframe);
            let mut wire = vec![0; 17];
            {
                let mut sending = Box::pin(send_payload(&mut writer, &frame, &mut rx, None));
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
        partial_write(false, true).await;
    }

    #[tokio::test]
    async fn partial_final_frame_preserves_exact_bytes() {
        partial_write(true, true).await;
        partial_write(true, false).await;
    }

    #[tokio::test]
    async fn partial_delta_finishes_before_its_dependent_successor() {
        partial_write(false, false).await;
    }

    #[tokio::test]
    async fn interrupted_payload_preserves_write_errors() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;

        for closed in [false, true] {
            let (mut writer, reader) = tokio::io::duplex(64);
            let (tx, mut rx) = mpsc::channel(1);
            let frame = produced(0, true);
            let mut sending = Box::pin(send_payload(&mut writer, &frame, &mut rx, None));
            poll_fn(|cx| {
                assert!(sending.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            if !closed {
                tx.send(produced(1, true)).await.unwrap();
            }
            drop(tx);
            // Select the producer event before causing the write to fail.
            poll_fn(|cx| {
                assert!(sending.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(reader);
            assert!(
                tokio::time::timeout(Duration::from_secs(1), sending)
                    .await
                    .unwrap()
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn independent_keyframe_replaces_delta_without_waiting_for_peer() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;

        {
            let (mut writer, _reader) = tokio::io::duplex(64);
            let (tx, mut rx) = mpsc::channel(1);
            let frame = produced(0, false);
            let mut sending = Box::pin(send_payload(&mut writer, &frame, &mut rx, None));
            poll_fn(|cx| {
                assert!(sending.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            tx.send(produced(1, true)).await.unwrap();
            let result = tokio::time::timeout(Duration::from_secs(1), sending)
                .await
                .unwrap()
                .unwrap();
            let PayloadOutcome::Abandoned(next) = result else {
                panic!("stale delta was not abandoned");
            };
            assert_eq!(next.header.seq, 1);
            assert!(next.header.keyframe);
        }
    }

    #[test]
    fn reference_chain_waits_for_keyframe_after_gap_empty_or_sequence_end() {
        let mut chain = FrameChain::default();
        assert!(!chain.admit(&produced(0, false)));
        assert!(chain.admit(&produced(1, true)));
        assert!(chain.admit(&produced(2, false)));
        assert!(!chain.admit(&produced(4, false))); // Missing reference 3.
        assert!(!chain.admit(&produced(5, false)));
        assert!(chain.admit(&produced(6, true)));
        let mut empty = produced(7, false);
        empty.payload = Bytes::new();
        assert!(!chain.admit(&empty));
        assert!(!chain.admit(&produced(8, false)));
        assert!(chain.admit(&produced(u64::MAX - 1, true)));
        assert!(!chain.admit(&produced(u64::MAX, false)));
        assert!(!chain.admit(&produced(0, false)));
    }

    #[cfg(feature = "x11")]
    #[test]
    fn native_h264_chain_recovers_after_a_lost_reference() {
        use crate::{Decoder, EncodedFrame, Encoder, H264Decoder, H264Encoder, RawFrame};

        let mut encoder = H264Encoder::new(4_000_000, 30.0).unwrap();
        let mut decoder = H264Decoder::new().unwrap();
        let mut chain = FrameChain::default();
        let mut decoded = Vec::new();
        for seq in 0..7 {
            let mut bgra = vec![0; 64 * 64 * 4];
            for (i, pixel) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let value = if (i / 64 + seq as usize * 4) % 32 < 16 {
                    220
                } else {
                    40
                };
                pixel.copy_from_slice(&[value, value, value, 255]);
            }
            if seq == 5 {
                encoder.request_idr();
            }
            let encoded = encoder
                .encode(&RawFrame {
                    width: 64,
                    height: 64,
                    stride: 256,
                    data: bgra.into(),
                })
                .unwrap();
            assert!(!encoded.data.is_empty());
            assert_eq!(encoded.keyframe, seq == 0 || seq == 5);
            let mut frame = produced(seq, encoded.keyframe);
            frame.header.width = 64;
            frame.header.height = 64;
            frame.payload = encoded.data;
            if seq == 2 {
                // Simulate an encoded frame lost to backpressure.
                continue;
            }
            if chain.admit(&frame) {
                let raw = decoder
                    .decode(&EncodedFrame {
                        codec: frame.header.codec,
                        keyframe: frame.header.keyframe,
                        data: frame.payload,
                    })
                    .unwrap()
                    .expect("admitted H.264 frame must decode");
                assert_eq!((raw.width, raw.height), (64, 64));
                // Check a native decoded pixel after recovery, not just a header.
                let expected = if (seq * 4) % 32 < 16 { 220i16 } else { 40 };
                assert!((i16::from(raw.data[0]) - expected).abs() < 12);
                decoded.push(seq);
            }
        }
        assert_eq!(decoded, [0, 1, 5, 6]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_supersession_installs_its_payload_receipt_digest() {
        for backend in [rds_net::Backend::Iroh, rds_net::Backend::Noq] {
            tokio::time::timeout(Duration::from_secs(15), async {
                let config = rds_net::EndpointConfig {
                    backend,
                    discovery: false,
                    bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
                    ..Default::default()
                };
                let client = rds_net::bind_endpoint(config.clone()).await.unwrap();
                let server = rds_net::bind_endpoint(config).await.unwrap();
                let (a, b) = tokio::join!(client.connect(server.addr(), rds_core::ALPN), async {
                    server.accept().await.unwrap().await
                });
                let (a, b) = (a.unwrap(), b.unwrap());
                let admission = Arc::new(AtomicU64::new(2));
                let payload = Bytes::from(vec![7; 32 * 1024 * 1024]);
                let expected = *blake3::hash(&payload).as_bytes();
                let mut first = produced(0, true);
                first.payload = payload;
                let first = AdmittedFrame {
                    produced: first,
                    generation: 0,
                    key: None,
                    permit: CapturePermit(admission.clone()),
                };
                let (tx, mut rx) = mpsc::channel(1);
                let mut acknowledgements = JoinSet::new();
                let proofs = crate::receipts::Receipts::new();
                let delivery = FrameDelivery {
                    repair: crate::media_repair::MediaRepair::new().0,
                    idr: crate::ProducerSignal::new(false, Arc::default()),
                    feedback: Arc::new(DeliveryFeedback::default()),
                    latest_key_seq: Arc::new(AtomicU64::new(u64::MAX)),
                    payload_receipts: Some(proofs.clone()),
                };
                let (outcome, ()) = tokio::join!(
                    send_frame_inner(
                        &a,
                        rds_core::UniHello::Desktop,
                        first,
                        &mut rx,
                        &mut acknowledgements,
                        delivery
                    ),
                    async {
                        let mut stream = b.accept_uni().await.unwrap();
                        let _: rds_core::UniHello = read_frame(&mut stream).await.unwrap();
                        let header: FrameHeader = read_frame(&mut stream).await.unwrap();
                        assert_eq!(header.seq, 0);
                        // Keep the large original payload blocked until the
                        // producer event has selected its completed-supersession
                        // path. The receiver then drains it exactly once.
                        tx.send(AdmittedFrame {
                            produced: produced(1, false),
                            generation: 0,
                            key: None,
                            permit: CapturePermit(admission.clone()),
                        })
                        .await
                        .unwrap();
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        let bytes = stream.read_to_end(32 * 1024 * 1024).await.unwrap();
                        assert_eq!(*blake3::hash(&bytes).as_bytes(), expected);
                        assert!(
                            proofs.confirm(0, &expected, false),
                            "a completed superseded frame rejected its exact receipt"
                        );
                    }
                );
                let SendOutcome::Superseded(next) = outcome else {
                    panic!("fixture did not take the completed-supersession path");
                };
                assert_eq!(next.produced.header.seq, 1);
                drop(next);
                assert!(matches!(
                    acknowledgements.join_next().await.unwrap().unwrap(),
                    (0, FrameReceipt::Delivered)
                ));
                assert_eq!(admission.load(Ordering::Acquire), 0);
                a.close(0u32.into(), b"test done");
                tokio::join!(client.close(), server.close());
            })
            .await
            .expect("completed supersession hung");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_frame_writer_resets_stream_and_preserves_connection() {
        for backend in [rds_net::Backend::Iroh, rds_net::Backend::Noq] {
            tokio::time::timeout(Duration::from_secs(10), async {
                let config = rds_net::EndpointConfig {
                    backend,
                    discovery: false,
                    bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
                    ..Default::default()
                };
                let client = rds_net::bind_endpoint(config.clone()).await.unwrap();
                let server = rds_net::bind_endpoint(config).await.unwrap();
                let (a, b) = tokio::join!(client.connect(server.addr(), rds_core::ALPN), async {
                    server.accept().await.unwrap().await
                });
                let (a, b) = (a.unwrap(), b.unwrap());
                let stream = a.open_uni().await.unwrap();
                let writer = tokio::spawn(async move {
                    let mut sending = FrameSend {
                        stream,
                        finished: false,
                    };
                    sending.stream.write_all(b"prefix").await.unwrap();
                    std::future::pending::<()>().await;
                });
                let mut recv = b.accept_uni().await.unwrap();
                let mut prefix = [0; 6];
                recv.read_exact(&mut prefix).await.unwrap();
                assert_eq!(&prefix, b"prefix");
                writer.abort();
                assert!(writer.await.unwrap_err().is_cancelled());
                assert!(matches!(recv.read(&mut prefix).await,
                    Err(rds_net::ReadError::Reset(code)) if code == STALE_FRAME_RESET.into()));

                // Reset belongs to the abandoned frame, not the connection.
                let mut next = a.open_uni().await.unwrap();
                next.write_all(b"usable").await.unwrap();
                next.finish().unwrap();
                let mut recv = b.accept_uni().await.unwrap();
                assert_eq!(recv.read_to_end(6).await.unwrap(), b"usable");
                a.close(0u32.into(), b"done");
                client.close().await;
                server.close().await;
            })
            .await
            .expect("frame cancellation did not release transport resources");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn obsolete_stop_during_payload_does_not_end_writer_or_count_fresh_delivery() {
        for (backend, stop_code) in [rds_net::Backend::Iroh, rds_net::Backend::Noq]
            .into_iter()
            .flat_map(|backend| {
                [rds_core::DESKTOP_FRAME_OBSOLETE, 42].map(move |code| (backend, code))
            })
        {
            tokio::time::timeout(Duration::from_secs(10), async {
                let config = rds_net::EndpointConfig {
                    backend,
                    discovery: false,
                    bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
                    ..Default::default()
                };
                let client = rds_net::bind_endpoint(config.clone()).await.unwrap();
                let server = rds_net::bind_endpoint(config).await.unwrap();
                let (a, b) = tokio::join!(client.connect(server.addr(), rds_core::ALPN), async {
                    server.accept().await.unwrap().await
                });
                let (a, b) = (a.unwrap(), b.unwrap());
                let admission = Arc::new(AtomicU64::new(1));
                let feedback = Arc::new(DeliveryFeedback::default());
                let idr = crate::ProducerSignal::new(false, Arc::default());
                let produce = AdmittedFrame {
                    produced: Produced {
                        header: FrameHeader {
                            seq: 1,
                            keyframe: false,
                            capture_ts_ms: 0,
                            encode_done_ts_ms: 0,
                            send_ts_ms: 0,
                            codec: rds_core::Codec::H264,
                            width: 32,
                            height: 32,
                        },
                        payload: Bytes::from(vec![1; 32 * 1024 * 1024]),
                    },
                    generation: 0,
                    key: None,
                    permit: CapturePermit(admission.clone()),
                };
                let (_tx, mut rx) = mpsc::channel(2);
                let mut receipts = JoinSet::new();
                let delivery = FrameDelivery {
                    repair: crate::media_repair::MediaRepair::new().0,
                    idr: idr.clone(),
                    feedback: feedback.clone(),
                    latest_key_seq: Arc::new(AtomicU64::new(u64::MAX)),
                    payload_receipts: None,
                };
                let (outcome, ()) = tokio::join!(
                    send_frame(
                        &a,
                        rds_core::UniHello::Desktop,
                        produce,
                        &mut rx,
                        &mut receipts,
                        delivery
                    ),
                    async {
                        let mut stream = b.accept_uni().await.unwrap();
                        let _: rds_core::UniHello = read_frame(&mut stream).await.unwrap();
                        let h: FrameHeader = read_frame(&mut stream).await.unwrap();
                        assert_eq!(h.seq, 1);
                        stream.stop(stop_code.into()).unwrap();
                    }
                );
                let obsolete = stop_code == rds_core::DESKTOP_FRAME_OBSOLETE;
                assert_eq!(
                    matches!(outcome, SendOutcome::Failed),
                    !obsolete,
                    "only the exact obsolete disposition may continue the writer"
                );
                while let Some(result) = receipts.join_next().await {
                    assert_eq!(result.unwrap(), (1, FrameReceipt::Obsolete));
                }
                assert_eq!(admission.load(Ordering::Acquire), 0);
                assert_eq!(
                    feedback.acknowledged.load(Ordering::Acquire),
                    0,
                    "obsolete disposal cannot justify bitrate growth"
                );
                assert_eq!(feedback.failed.load(Ordering::Acquire), 0);
                assert_eq!(feedback.timely_bytes.load(Ordering::Acquire), 0);
                assert_eq!(feedback.timely_receipts.load(Ordering::Acquire), 0);
                assert_eq!(
                    feedback.obsolete.load(Ordering::Acquire),
                    u64::from(obsolete)
                );
                assert!(!idr.load(Ordering::Acquire));
                let mut next = a.open_uni().await.unwrap();
                next.write_all(b"still usable").await.unwrap();
                next.finish().unwrap();
                let mut stream = b.accept_uni().await.unwrap();
                assert_eq!(stream.read_to_end(32).await.unwrap(), b"still usable");
                a.close(0u32.into(), b"done");
                tokio::join!(client.close(), server.close());
            })
            .await
            .expect("obsolete stop did not release resources");
        }
    }

    #[test]
    fn older_failed_receipt_cannot_invalidate_an_admitted_recovery_or_repeat_repair() {
        let mut chain = FrameChain::default();
        let mut frame = Produced {
            header: FrameHeader {
                seq: 20,
                keyframe: true,
                capture_ts_ms: 0,
                encode_done_ts_ms: 0,
                send_ts_ms: 0,
                codec: rds_core::Codec::H264,
                width: 32,
                height: 32,
            },
            payload: Bytes::from_static(b"independent picture"),
        };
        assert!(chain.admit(&frame));
        // The receipt worker requested repair; the producer consumed that flag
        // and the writer already admitted its replacement. The older failed
        // receipt may join afterward while that newer key is travelling.
        let idr = crate::ProducerSignal::new(false, Arc::default());
        request_frame_repair(&AtomicU64::new(20), &idr, 10);
        chain.failed_receipt(10);
        assert!(
            !idr.load(Ordering::Relaxed),
            "one failed receipt requested a second expensive recovery"
        );
        frame.header.seq = 21;
        frame.header.keyframe = false;
        assert!(
            chain.admit(&frame),
            "an old failure broke the independently recovered reference chain"
        );
        request_frame_repair(&AtomicU64::new(20), &idr, 21);
        assert!(
            idr.load(Ordering::Relaxed),
            "a current failure still requests repair"
        );
        chain.failed_receipt(21);
        frame.header.seq = 22;
        assert!(
            !chain.admit(&frame),
            "a failed current reference must still invalidate its successors"
        );
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
            relay_slot: None,
        }
    }

    #[test]
    fn controller_drops_bitrate_on_loss() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step(Some(path(1000, 0, 20, 0)), 0); // prime baseline
        let bps = c.step(Some(path(2000, 160, 20, 0)), 0); // 16% of the sample
        assert!(bps < 4_000_000, "loss must cut bitrate, got {bps}");
    }

    #[test]
    fn moderate_loss_with_timely_receipts_holds_quality_without_starvation() {
        for lost_per_sample in [3, 8] {
            let mut controller = BitrateController::new(4_000_000, 8_000_000);
            let mut sample = path(1000, 0, 120, 0);
            controller.step_with_delivery(Some(sample), 0, false, true);
            for _ in 0..240 {
                sample.sent += 100;
                sample.lost += lost_per_sample;
                sample.congestion_events += 1;
                controller.step_with_delivery(Some(sample), 0, false, true);
            }
            assert!(
                controller.current() >= 4_000_000,
                "successful media at {lost_per_sample}% random loss must not force the codec floor"
            );
            let previous = controller.current();
            let reduced = controller.step_with_delivery(Some(sample), 0, true, false);
            assert!(
                reduced < previous,
                "real media blockage must still reduce load"
            );
        }
    }

    #[test]
    fn moderate_loss_hold_clears_after_clean_feedback_or_path_change() {
        let mut controller = BitrateController::new(4_000_000, 8_000_000);
        controller.step(Some(path(1000, 0, 120, 0)), 0);
        assert_eq!(controller.step(Some(path(1100, 3, 120, 1)), 0), 4_000_000);
        assert_eq!(controller.step(Some(path(1105, 3, 120, 1)), 0), 4_000_000);
        assert!(controller.step(Some(path(1200, 3, 120, 1)), 0) > 4_000_000);
        controller.step(Some(path(1300, 6, 120, 2)), 0);
        let held = controller.current();
        let mut replacement = path(100, 0, 120, 0);
        replacement.path_id = 99;
        controller.step(Some(replacement), 0);
        replacement.sent += 100;
        assert!(controller.step(Some(replacement), 0) > held);
    }

    #[test]
    fn moderate_loss_recovery_requires_receipts_and_bounds_each_probe() {
        let mut controller = BitrateController::new(100_000, 8_000_000);
        let mut sample = path(1000, 0, 120, 0);
        controller.step_with_delivery(Some(sample), 0, false, true);
        sample.sent += 100;
        sample.lost += 3;
        assert_eq!(
            controller.step_with_delivery(Some(sample), 0, false, false),
            100_000,
            "moderate-loss recovery cannot grow without fresh receipts"
        );
        for _ in 0..240 {
            sample.sent += 100;
            sample.lost += 3;
            let previous = controller.current();
            let next = controller.step_with_delivery(Some(sample), 0, false, true);
            assert_eq!(next, (previous + previous / 100).min(8_000_000));
        }
        assert!(controller.current() > 1_000_000);
        controller.steer(8_000_000);
        assert_eq!(
            controller.step_with_delivery(Some(sample), 0, false, true),
            8_000_000,
            "successful probes cannot exceed the negotiated ceiling"
        );
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
    fn delayed_media_reduces_load_despite_clean_relay_counters_and_recovers_cautiously() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        let mut sample = path(1000, 0, 80, 0);
        sample.via_relay = true;
        c.step_with_delivery(Some(sample), 0, false, true);
        sample.sent += 100;
        let reduced = c.step_with_delivery(Some(sample), 0, true, false);
        assert_eq!(reduced, 2_800_000);
        // Fresh ACKs must not undo the cut during its five-second hold.
        for _ in 0..20 {
            sample.sent += 100;
            assert_eq!(c.step_with_delivery(Some(sample), 0, false, true), reduced);
        }
        // Packet activity without a media receipt cannot justify growth.
        sample.sent += 100;
        assert_eq!(c.step_with_delivery(Some(sample), 0, false, false), reduced);
        sample.sent += 100;
        assert_eq!(
            c.step_with_delivery(Some(sample), 0, false, true),
            2_828_000
        );
        assert!(c.step_with_delivery(None, 0, true, false) < 2_828_000);
    }

    #[test]
    fn repeated_media_failure_honors_the_existing_floor_and_ceiling() {
        let mut c = BitrateController::new(200_000, 300_000);
        for _ in 0..30 {
            c.step_with_delivery(None, 0, true, false);
        }
        assert_eq!(c.current(), 100_000);
        c.steer(1_000_000);
        assert_eq!(c.current(), 300_000);
    }

    #[test]
    fn one_delivery_burst_does_not_compound_frame_and_path_penalties() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step_with_delivery(Some(path(1000, 0, 80, 0)), 0, false, true);
        // A path event and a delayed receipt describe the same congestion.
        let reduced = c.step_with_delivery(Some(path(1100, 0, 160, 1)), 0, true, false);
        assert_eq!(reduced, 2_800_000, "one event must not apply two 30% cuts");
        // Up to three outstanding frames can report the same burst on
        // successive pacing ticks. Keep their first cut rather than cubing it.
        for i in 0..3 {
            assert_eq!(
                c.step_with_delivery(Some(path(1200 + i * 100, 0, 160, 1)), 0, true, false),
                reduced,
                "correlated receipts over-penalized image quality"
            );
        }
        // Continued pressure after a full second must still reduce load.
        assert_eq!(
            c.step_with_delivery(Some(path(1600, 0, 160, 1)), 0, true, false),
            1_960_000
        );
    }

    #[test]
    fn media_delay_budget_accounts_for_propagation_but_remains_bounded() {
        assert_eq!(delivery_delay_budget(None), Duration::from_millis(250));
        assert_eq!(
            delivery_delay_budget(Some(path(0, 0, 200, 0))),
            Duration::from_millis(600)
        );
        assert_eq!(
            delivery_delay_budget(Some(path(0, 0, 9000, 0))),
            Duration::from_secs(1)
        );
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
        c.step(Some(path(2000, 0, 60, 0)), 0); // first elevated sample
        let bps = c.step(Some(path(2100, 0, 60, 0)), 0); // confirmed 3× baseline RTT
        assert!(bps < 4_000_000, "RTT growth must cut bitrate, got {bps}");
    }

    #[test]
    fn sparse_idle_rtt_changes_do_not_drive_healthy_media_to_floor() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step_with_delivery(Some(path(1000, 0, 90, 0)), 0, false, true);
        for i in 1..=160 {
            let rtt = [90, 240, 240, 90][i as usize % 4];
            let mut p = path(1000 + i, 0, rtt, 0);
            p.sent_bytes = 1_200_000 + i * 128;
            c.step_with_delivery(Some(p), 0, false, true);
        }
        assert_eq!(
            c.current(),
            8_000_000,
            "tiny acknowledged updates are not offered media congestion"
        );
    }

    #[test]
    fn a_cached_rtt_sample_without_new_offered_bytes_cannot_confirm_a_cut() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step_with_delivery(Some(path(1000, 0, 20, 0)), 0, false, false);
        let elevated = path(2000, 0, 60, 0);
        c.step_with_delivery(Some(elevated), 0, false, false);
        c.step_with_delivery(Some(elevated), 0, false, false);
        assert_eq!(
            c.current(),
            4_000_000,
            "one unchanged path snapshot is not two RTT observations"
        );
    }

    #[test]
    fn sparse_successful_delivery_does_not_treat_one_packet_as_mass_loss() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        // Captured WAN pattern: 3–8 datagrams per 250 ms, approximately
        // one lost packet per 100. Every frame is still acknowledged.
        let mut p = path(1000, 0, 140, 0);
        c.step_with_delivery(Some(p), 0, false, true);
        for i in 1..=320 {
            p.sent += 5;
            if i % 20 == 0 {
                p.lost += 1;
                p.congestion_events += 1;
            }
            c.step_with_delivery(Some(p), 0, false, true);
        }
        assert_eq!(
            c.current(),
            8_000_000,
            "sparse random loss must not starve an acknowledged desktop"
        );
    }

    #[test]
    fn a_severely_lossy_short_burst_reacts_before_a_full_sample() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step(Some(path(1000, 0, 140, 0)), 0);
        assert_eq!(c.step(Some(path(1010, 5, 140, 1)), 0), 2_800_000);
    }

    #[test]
    fn switching_paths_discards_the_previous_partial_loss_sample() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step(Some(path(1000, 0, 140, 0)), 0);
        c.step(Some(path(1040, 4, 140, 1)), 0);
        let mut replacement = path(10, 0, 120, 0);
        replacement.path_id = 2;
        c.step(Some(replacement), 0);
        let before = c.current();
        replacement.sent += 60;
        assert!(c.step(Some(replacement), 0) > before);
    }

    #[test]
    fn isolated_rtt_spikes_do_not_collapse_successful_delivery() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        for i in 0..320 {
            let rtt = if i % 20 == 10 { 260 } else { 140 };
            c.step_with_delivery(Some(path(1000 + i * 5, 0, rtt, 0)), 0, false, true);
        }
        assert_eq!(
            c.current(),
            8_000_000,
            "one delayed RTT sample must not repeatedly punish fresh receipts"
        );
    }

    #[test]
    fn low_rtt_rounding_noise_does_not_penalize_a_clean_path() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        for i in 0..40 {
            c.step(
                Some(path(1000 + i * 100, 0, [1, 2, 3][i as usize % 3], 0)),
                0,
            );
        }
        assert_eq!(
            c.current(),
            8_000_000,
            "minor RTT noise cannot justify a codec drought"
        );
        assert!(
            {
                c.step(Some(path(5100, 0, 40, 0)), 0);
                c.step(Some(path(5200, 0, 40, 0)), 0) < 8_000_000
            },
            "material RTT growth still reacts"
        );
    }

    #[test]
    fn sustained_rtt_change_without_loss_does_not_collapse_bitrate() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step(Some(path(1000, 0, 90, 0)), 0);
        c.step(Some(path(2000, 0, 240, 0)), 0);
        let reduced = c.step(Some(path(2100, 0, 240, 0)), 0);
        assert!(reduced < 4_000_000, "a new delay increase must react");
        for i in 0..40 {
            c.step(Some(path(3000 + i * 1000, 0, 240, 0)), 0);
        }
        assert_eq!(c.current(), 8_000_000, "stable delay is not new congestion");
        c.step(Some(path(44000, 0, 500, 0)), 0);
        assert!(c.step(Some(path(44100, 0, 500, 0)), 0) < 8_000_000);
    }

    #[test]
    fn first_sample_never_penalizes() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        // A path that already lost packets before we started watching.
        let bps = c.step(Some(path(1_000_000, 500_000, 20, 0)), 0);
        assert_eq!(bps, 4_000_000, "first sample establishes baseline only");
    }

    #[test]
    fn steer_sets_target_inside_bounds() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.steer(2_500_000);
        assert_eq!(c.current(), 2_500_000);
        // Out-of-range requests clamp to the controller's own contract.
        c.steer(50_000);
        assert_eq!(c.current(), 100_000);
        c.steer(50_000_000);
        assert_eq!(c.current(), 8_000_000);
    }

    #[test]
    fn loss_only_cuts_respect_recent_timely_confirmed_goodput() {
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step_with_delivery_floor(Some(path(1000, 0, 120, 0)), 0, false, true, None);
        for i in 1..=40 {
            let bps = c.step_with_delivery_floor(
                Some(path(1000 + i * 100, i * 15, 120, i)),
                0,
                false,
                true,
                Some(1_600_000),
            );
            assert!(
                bps >= 1_600_000,
                "confirmed timely media must not collapse: {bps}"
            );
            assert!(bps <= 8_000_000);
        }
        // No receipt evidence keeps the conservative original loss response.
        let mut old = BitrateController::new(4_000_000, 8_000_000);
        old.step_with_delivery(Some(path(1000, 0, 120, 0)), 0, false, true);
        for i in 1..=40 {
            old.step_with_delivery(Some(path(1000 + i * 100, i * 15, 120, i)), 0, false, true);
        }
        assert!(
            old.current() < 1_600_000,
            "fixture must distinguish the old response"
        );
    }

    #[test]
    fn delivery_rate_credits_only_complete_timely_receipts() {
        let feedback = DeliveryFeedback::default();
        feedback.acknowledged(
            10_000,
            Duration::from_millis(100),
            Duration::from_millis(250),
        );
        feedback.acknowledged(90_000, Duration::from_secs(2), Duration::from_millis(250));
        assert_eq!(feedback.acknowledged.load(Ordering::Relaxed), 2);
        assert_eq!(feedback.timely_bytes.load(Ordering::Relaxed), 10_000);
        assert_eq!(feedback.timely_receipts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn empty_media_key_starts_without_artificial_wait_but_retains_bounded_debt() {
        let mut budget = 0.0;
        let rate = 100_000.0 / 8.0;
        let old_wait = Duration::from_secs_f64((50_000.0f64 / rate).min(0.5));
        assert_eq!(
            old_wait,
            Duration::from_millis(500),
            "fixture must distinguish prior delay"
        );
        assert_eq!(
            frame_pacing_wait(&mut budget, rate, true, 50_000, 0),
            Duration::ZERO
        );
        assert_eq!(budget, -rate * 0.5);
        assert_eq!(
            frame_pacing_wait(&mut budget, rate, false, 1000, 0),
            Duration::from_millis(500)
        );
        assert_eq!(budget, -rate * 0.5, "a later frame cannot grow debt");
        let mut full = 50_000.0;
        assert_eq!(
            frame_pacing_wait(&mut full, rate, false, 1000, 0),
            Duration::ZERO
        );
        assert_eq!(full, 48_936.0);
        for (keyframe, bytes, pending) in [(false, 50_000, 0), (true, 50_000, 1), (true, 65_537, 0)]
        {
            let mut budget = 0.0;
            assert_eq!(
                frame_pacing_wait(&mut budget, rate, keyframe, bytes, pending),
                Duration::from_millis(500),
                "nonempty/large/dependent frames retain pacing"
            );
        }
    }

    #[tokio::test]
    async fn receipt_completed_during_idle_wait_does_not_delay_the_next_independent_key() {
        let mut receipts = JoinSet::new();
        let done = receipts.spawn(async { (1, FrameReceipt::Delivered) });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !done.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            receipts.len(),
            1,
            "completed task still needs to be retired"
        );
        let (mut acknowledged, mut obsolete, mut failed) = (0, 0, 0);
        let mut chain = FrameChain::default();
        let idr = crate::ProducerSignal::new(false, Arc::default());
        drain_frame_receipts(
            &mut receipts,
            &mut chain,
            &idr,
            &mut acknowledged,
            &mut obsolete,
            &mut failed,
        );
        assert_eq!((acknowledged, obsolete, failed), (1, 0, 0));
        assert!(!idr.load(Ordering::Relaxed));
        let mut budget = 0.0;
        assert_eq!(
            frame_pacing_wait(&mut budget, 12_500.0, true, 50_000, receipts.len()),
            Duration::ZERO
        );
    }

    #[test]
    fn confirmed_goodput_cannot_override_delivery_rtt_or_producer_pressure() {
        for reason in 0..4 {
            let mut c = BitrateController::new(4_000_000, 8_000_000);
            c.step_with_delivery_floor(Some(path(1000, 0, 120, 0)), 0, false, true, None);
            let (impaired, delivered, misses, rtt) = match reason {
                0 => (true, false, 0, 120),
                1 => (false, false, 0, 120),
                2 => (false, true, 1, 120),
                _ => (false, true, 0, 240),
            };
            let bps = c.step_with_delivery_floor(
                Some(path(1100, 15, rtt, 1)),
                misses,
                impaired,
                delivered,
                Some(8_000_000),
            );
            assert!(
                bps < 4_000_000,
                "real pressure was overridden: reason={reason} bps={bps}"
            );
        }
        let mut capped = BitrateController::new(4_000_000, 4_000_000);
        capped.step_with_delivery_floor(Some(path(1000, 0, 120, 0)), 0, false, true, None);
        assert_eq!(
            capped.step_with_delivery_floor(
                Some(path(1100, 15, 120, 1)),
                0,
                false,
                true,
                Some(u64::MAX)
            ),
            4_000_000
        );
    }

    #[test]
    fn steer_survives_adaptation_ticks() {
        // The SetBitrate semantics the viewer sees: a filed target is not
        // overwritten by the next pacing step — adaptation resumes *from*
        // the steered point.
        let mut c = BitrateController::new(4_000_000, 8_000_000);
        c.step(Some(path(1000, 0, 20, 0)), 0); // prime baseline
        c.steer(1_000_000);
        let bps = c.step(Some(path(2000, 0, 20, 0)), 0);
        // Clean window: one recovery step from 1Mbps, not a snap back to 4.
        assert!(
            bps > 1_000_000 && bps < 2_000_000,
            "adaptation must resume from the steered target, got {bps}"
        );
    }
}

#[cfg(all(test, target_os = "linux", feature = "x11"))]
mod native_idle_qa;
