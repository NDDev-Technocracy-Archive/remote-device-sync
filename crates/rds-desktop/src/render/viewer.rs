use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use rds_core::{DesktopControl, InputEvent, InputKind};
use tokio::sync::Notify;
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    keyboard::PhysicalKey,
    window::{Icon, Window, WindowId, WindowLevel},
};

use super::{
    gpu::{DrawOutcome, Gpu},
    input::{Viewport, evdev, key_transition, modifier_changes},
};
use crate::{DesktopError, RawFrame};

mod frame;
use frame::{FrameMailbox, Pending};

mod spaces;
mod workspace;
pub use workspace::{WorkspaceEvent, WorkspaceHandle, WorkspaceUpdate};

#[derive(Debug)]
pub enum ViewerInput {
    Control(DesktopControl),
    Close,
    InputStalled,
}

struct InputState {
    queue: Mutex<VecDeque<ViewerInput>>,
    pointer_in_flight: Mutex<VecDeque<(u64, tokio::time::Instant)>>,
    ready: Notify,
    pointer_coalesced: AtomicU64,
    input_dropped: AtomicU64,
    max_depth: AtomicU64,
}
struct InputSender(Arc<InputState>);
const INPUT_QUEUE_CAPACITY: usize = 1024;
// Bound remote injection debt, not just the fast local/network writer queue.
// At 150ms RTT this allows about 27 pointer updates/sec without accumulating
// seconds of motion ahead of a click or the control heartbeat.
const POINTER_IN_FLIGHT: usize = 4;
const POINTER_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

impl InputState {
    fn pointer_ack(&self, seq: u64) {
        let mut pending = self
            .pointer_in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = pending.iter().position(|(sent, _)| *sent == seq) {
            pending.remove(index);
            drop(pending);
            self.ready.notify_one();
        }
    }

    fn cancel_pointers(&self) {
        self.pointer_in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.ready.notify_one();
    }
}

/// One bounded input consumer. Consecutive pointer positions collapse, while
/// keys/buttons retain their order relative to the final pointer before them.
/// A full queue fails closed. Motion before a button/key/scroll is a
/// position barrier, not stale data: removing it could redirect the action.
pub struct InputReceiver(Arc<InputState>);
impl InputReceiver {
    pub async fn recv(&mut self) -> Option<ViewerInput> {
        loop {
            let ready = self.0.ready.notified();
            if let Some(event) = self.next() {
                return Some(event);
            }
            let deadline = self
                .0
                .pointer_in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .front()
                .map(|(_, sent)| *sent + POINTER_ACK_TIMEOUT);
            match deadline {
                Some(deadline) => tokio::select! {
                    _ = ready => {},
                    _ = tokio::time::sleep_until(deadline) => {},
                },
                None => ready.await,
            }
        }
    }
    fn next(&self) -> Option<ViewerInput> {
        let mut queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(queue.front(), Some(ViewerInput::Close)) {
            return queue.pop_front();
        }
        let mut pending = self
            .0
            .pointer_in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .front()
            .is_some_and(|(_, sent)| sent.elapsed() >= POINTER_ACK_TIMEOUT)
        {
            // A missing handled ACK must not leave a healthy-video session
            // silently unable to accept pointer input forever. Fail the local
            // attempt; never manufacture credit or replay the old motion.
            return Some(ViewerInput::InputStalled);
        }
        if let Some(ViewerInput::Control(DesktopControl::Input(event))) = queue.front()
            && matches!(event.kind, InputKind::PointerMove { .. })
        {
            if pending.len() >= POINTER_IN_FLIGHT {
                // Retain the queued tail so new absolute positions can replace
                // it. Never skip across a key/button/scroll/display barrier.
                return None;
            }
            pending.push_back((event.seq, tokio::time::Instant::now()));
        }
        queue.pop_front()
    }
    /// Unpaced dequeue is only for discarding disconnected input, not dispatch.
    pub fn try_recv(&self) -> Option<ViewerInput> {
        self.0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }
}
impl InputSender {
    fn send(&self, message: ViewerInput) -> Result<(), ()> {
        let mut queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(message, ViewerInput::Close) {
            // Closing cancels queued controls, just like dropping the sender.
            // It must not wait for an ACK from an unresponsive remote sink.
            queue.clear();
            queue.push_back(message);
            drop(queue);
            self.0.ready.notify_one();
            return Ok(());
        }
        if let (
            ViewerInput::Control(DesktopControl::Input(next)),
            Some(ViewerInput::Control(DesktopControl::Input(previous))),
        ) = (&message, queue.back())
            && next.display_id == previous.display_id
            && matches!(next.kind, InputKind::PointerMove { .. })
            && matches!(previous.kind, InputKind::PointerMove { .. })
        {
            queue.pop_back();
            self.0.pointer_coalesced.fetch_add(1, Ordering::Relaxed);
        }
        if queue.len() >= INPUT_QUEUE_CAPACITY {
            self.0.input_dropped.fetch_add(1, Ordering::Relaxed);
            return Err(());
        }
        queue.push_back(message);
        self.0
            .max_depth
            .fetch_max(queue.len() as u64, Ordering::Relaxed);
        drop(queue);
        self.0.ready.notify_one();
        Ok(())
    }
}
impl Drop for InputSender {
    fn drop(&mut self) {
        let mut queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.clear();
        queue.push_back(ViewerInput::Close);
        drop(queue);
        self.0.ready.notify_one();
    }
}

#[derive(Clone, Default, Debug, serde::Serialize)]
pub struct ViewerReport {
    pub session_epoch: u64,
    pub frames_received: u64,
    pub frames_submitted: u64,
    pub gpu_uploads: u64,
    pub last_submission_ms: Option<u64>,
    pub frames_replaced: u64,
    pub redraws: u64,
    pub surface_skips: u64,
    pub first_frame_ms: Option<u64>,
    pub receive_to_submit_p50_ms: Option<f64>,
    pub receive_to_submit_p95_ms: Option<f64>,
    pub capture_encode_p50_ms: Option<f64>,
    pub capture_encode_p95_ms: Option<f64>,
    pub encode_to_send_p50_ms: Option<f64>,
    pub encode_to_send_p95_ms: Option<f64>,
    pub control_rtt_ms: Option<u64>,
    pub last_control_echo_ms: Option<u64>,
    pub input_acks: u64,
    pub inputs_dispatched: u64,
    pub input_queue_depth: usize,
    pub input_acks_canceled: u64,
    pub slow_input_acks: u64,
    pub last_input_ack_ms: Option<f64>,
    pub input_acks_matched: u64,
    pub input_acks_unmatched: u64,
    pub input_ack_tracking_evicted: u64,
    pub managed_events_separated: bool,
    pub input_ack_p50_ms: Option<f64>,
    pub input_ack_p95_ms: Option<f64>,
    pub keyboard_input_ack_p50_ms: Option<f64>,
    pub keyboard_input_ack_p95_ms: Option<f64>,
    pub keyboard_input_ack_max_ms: Option<f64>,
    pub button_input_ack_p50_ms: Option<f64>,
    pub button_input_ack_p95_ms: Option<f64>,
    pub button_input_ack_max_ms: Option<f64>,
    pub input_queue_p95_ms: Option<f64>,
    pub input_pointer_coalesced: u64,
    pub input_events_dropped: u64,
    pub input_queue_max_depth: u64,
    pub pending_input_acks: usize,
    pub oldest_input_ack_age_ms: Option<u64>,
    pub clipboard_transfers: u64,
    pub last_clipboard_bytes: u32,
    pub clipboard_transfer_p50_ms: Option<f64>,
    pub clipboard_transfer_p95_ms: Option<f64>,
    pub clipboard_transfer_max_ms: Option<f64>,
    pub last_clipboard_transfer_ms: Option<f64>,
    pub slow_clipboard_transfers: u64,
    pub clipboard_pending_transfers: usize,
    pub clipboard_oldest_pending_age_ms: Option<u64>,
    pub clipboard_unmatched_replies: u64,
    pub clipboard_tracking_evicted: u64,
    pub clipboard_transfers_canceled: u64,
    pub reconnects: u64,
    pub video_repair_requests: u64,
    pub last_recovery_ms: Option<u64>,
    pub last_frame_ms: Option<u64>,
    pub video_width: u32,
    pub video_height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visual_probe: Option<super::VisualProbeReport>,
}

#[derive(serde::Serialize)]
pub struct ViewerSnapshot {
    pub pid: u32,
    pub elapsed_ms: u64,
    pub status: String,
    pub network_stage: String,
    pub render_stage: String,
    pub ui_event_age_ms: u64,
    pub decoded_frame_age_ms: Option<u64>,
    pub encoded_frame_age_ms: Option<u64>,
    pub control_echo_age_ms: Option<u64>,
    pub submission_age_ms: Option<u64>,
    /// Time with a decoded image awaiting submission, preserved across replacement.
    /// Unlike submission_age_ms, a quiet screen with no new image has no debt.
    pub unpresented_frame_age_ms: Option<u64>,
    pub pending_frame_bytes: usize,
    pub occluded: bool,
    pub native_window: Option<super::NativeWindowState>,
    pub native_window_sample_age_ms: Option<u64>,
    pub report: ViewerReport,
}

#[derive(Default)]
struct SubmissionDebt(Option<u64>);
impl SubmissionDebt {
    fn queued(&mut self, now_ms: u64) {
        self.0.get_or_insert(now_ms);
    }
    fn presented(&mut self, next_queued_ms: Option<u64>) {
        self.0 = next_queued_ms;
    }
    fn hidden(&mut self) {
        self.0 = None;
    }
    fn age(&self, now_ms: u64) -> Option<u64> {
        self.0.map(|since| now_ms.saturating_sub(since))
    }
}
/// Local event-to-ack measurements never compare clocks on different hosts.
/// Coalesced pointer events are tracked only after dequeue; the event's own
/// local creation time still includes time spent waiting in the UI queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputClass {
    Keyboard,
    Button,
    Other,
}
impl InputClass {
    fn of(kind: &InputKind) -> Self {
        match kind {
            InputKind::KeyDown { .. } | InputKind::KeyUp { .. } => Self::Keyboard,
            InputKind::PointerButton { .. } => Self::Button,
            _ => Self::Other,
        }
    }
}

#[derive(Default)]
struct InputLatency {
    pending: VecDeque<(u64, u64, InputClass)>,
    queued: Vec<f64>,
    acknowledged: Vec<f64>,
    keyboard: Vec<f64>,
    buttons: Vec<f64>,
    evicted: u64,
    last_slow_log_ms: Option<u64>,
}

#[derive(Default)]
struct ClipboardLatency {
    pending: VecDeque<(u64, u32, u64)>,
    acknowledged: Vec<f64>,
    evicted: u64,
}
impl ClipboardLatency {
    fn sent(&mut self, id: u64, bytes: u32, now_ms: u64) {
        if self.pending.len() == 8 {
            self.pending.pop_front();
            self.evicted += 1;
        }
        self.pending.push_back((id, bytes, now_ms));
    }
    fn ack(&mut self, id: u64, bytes: u32, now_ms: u64) -> Option<f64> {
        let index = self
            .pending
            .iter()
            .position(|(pending, size, _)| *pending == id && *size == bytes)?;
        let (_, _, sent_ms) = self.pending.remove(index)?;
        let latency = now_ms.saturating_sub(sent_ms) as f64;
        sample(&mut self.acknowledged, latency);
        Some(latency)
    }
}
impl InputLatency {
    fn sent(&mut self, seq: u64, created_ms: u64, now_ms: u64, class: InputClass) {
        if self.pending.len() == INPUT_QUEUE_CAPACITY {
            self.pending.pop_front();
            self.evicted += 1;
        }
        self.pending.push_back((seq, created_ms, class));
        sample(&mut self.queued, now_ms.saturating_sub(created_ms) as f64);
    }
    fn ack(&mut self, seq: u64, now_ms: u64) -> Option<f64> {
        if let Some(index) = self
            .pending
            .iter()
            .position(|(pending, _, _)| *pending == seq)
            && let Some((_, created_ms, class)) = self.pending.remove(index)
        {
            let latency = now_ms.saturating_sub(created_ms) as f64;
            sample(&mut self.acknowledged, latency);
            match class {
                InputClass::Keyboard => sample(&mut self.keyboard, latency),
                InputClass::Button => sample(&mut self.buttons, latency),
                InputClass::Other => {}
            }
            return Some(latency);
        }
        None
    }
}

struct State {
    frames: FrameMailbox,
    submission_debt: SubmissionDebt,
    wake_pending: bool,
    presenting: bool,
    remote_cursor_embedded: bool,
    status: String,
    // A problem beside the connection, such as the clipboard; never a connection state.
    notice: Option<String>,
    label: String,
    extent: (u32, u32),
    report: ViewerReport,
    delays: Vec<f64>,
    encoding: Vec<f64>,
    sending: Vec<f64>,
    input_latency: InputLatency,
    clipboard_latency: ClipboardLatency,
    clipboard: super::clipboard::Clipboard,
    close: bool,
    interrupted: Option<Instant>,
    network_stage: String,
    render_stage: String,
    last_ui_ms: u64,
    last_encoded_ms: Option<u64>,
    occluded: bool,
    native_window: Option<super::NativeWindowState>,
    visual_probe: Option<super::visual_probe::VisualProbe>,
    session_span: tracing::Span,
}
fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn sample(samples: &mut Vec<f64>, value: f64) {
    if samples.len() == 1024 {
        samples.remove(0);
    }
    samples.push(value);
}
fn quantiles(samples: &[f64]) -> (Option<f64>, Option<f64>) {
    if samples.is_empty() {
        return (None, None);
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    (
        Some(sorted[(sorted.len() - 1) * 50 / 100]),
        Some(sorted[(sorted.len() - 1) * 95 / 100]),
    )
}

#[derive(Clone)]
pub struct ViewerHandle {
    state: Arc<Mutex<State>>,
    input_state: Arc<InputState>,
    proxy: EventLoopProxy<()>,
    started: Instant,
}
impl ViewerHandle {
    /// An independent diagnostic tick probes UI dispatch even when no media
    /// arrives. The existing coalescing flag keeps a stalled loop bounded.
    pub fn heartbeat_ui(&self) {
        let mut state = lock(&self.state);
        self.wake(&mut state);
    }
    pub fn label(&self, label: String) {
        let mut state = lock(&self.state);
        state.label = label;
        self.wake(&mut state);
    }
    pub fn stage(&self, stage: &str) {
        lock(&self.state).network_stage = stage.into();
    }
    pub fn snapshot(&self) -> ViewerSnapshot {
        let report = self.report();
        let state = lock(&self.state);
        let elapsed = self.started.elapsed().as_millis() as u64;
        ViewerSnapshot {
            pid: std::process::id(),
            elapsed_ms: elapsed,
            status: state.status.clone(),
            network_stage: state.network_stage.clone(),
            render_stage: state.render_stage.clone(),
            ui_event_age_ms: elapsed.saturating_sub(state.last_ui_ms),
            decoded_frame_age_ms: report.last_frame_ms.map(|v| elapsed.saturating_sub(v)),
            encoded_frame_age_ms: state.last_encoded_ms.map(|v| elapsed.saturating_sub(v)),
            control_echo_age_ms: report
                .last_control_echo_ms
                .map(|v| elapsed.saturating_sub(v)),
            submission_age_ms: report.last_submission_ms.map(|v| elapsed.saturating_sub(v)),
            unpresented_frame_age_ms: state.submission_debt.age(elapsed),
            pending_frame_bytes: state
                .frames
                .pending
                .as_ref()
                .map_or(0, |p| p.raw.data.len()),
            occluded: state.occluded,
            native_window: state.native_window,
            native_window_sample_age_ms: state
                .native_window
                .map(|sample| elapsed.saturating_sub(sample.sampled_elapsed_ms)),
            report,
        }
    }
    fn wake(&self, state: &mut State) {
        if !state.wake_pending {
            state.wake_pending = true;
            let _ = self.proxy.send_event(());
        }
    }
    pub fn frame(&self, raw: RawFrame, received: Instant) {
        self.queue_frame(raw, received, None);
    }
    /// Preserve an encoded frame's sequence for causal stage correlation.
    pub fn frame_with_seq(&self, raw: RawFrame, received: Instant, seq: u64) {
        self.queue_frame(raw, received, Some(seq));
    }
    fn queue_frame(&self, raw: RawFrame, received: Instant, frame_seq: Option<u64>) {
        let mut state = lock(&self.state);
        let queued_ms = self.started.elapsed().as_millis() as u64;
        if !state.occluded && state.render_stage != "surface occluded" {
            state.submission_debt.queued(queued_ms);
        }
        state.report.frames_received += 1;
        state.report.video_width = raw.width;
        state.report.video_height = raw.height;
        state.report.last_frame_ms = Some(queued_ms);
        if let Some(interrupted) = state.interrupted.take() {
            state.report.last_recovery_ms = Some(interrupted.elapsed().as_millis() as u64);
        }
        state.status = "Connected".into();
        if state.frames.queue(Pending {
            raw: Arc::new(raw),
            received,
            frame_seq,
            queued_ms,
        }) {
            state.report.frames_replaced += 1;
        }
        if state.presenting {
            self.wake(&mut state);
        }
    }
    /// Show a problem beside the connection without changing its state, so
    /// keyboard and cursor ownership stay with a connected remote window.
    pub fn notice(&self, text: impl Into<String>) {
        let mut state = lock(&self.state);
        state.notice = Some(text.into());
        self.wake(&mut state);
    }
    /// Caller-confirmed capture property; legacy X11 video need not embed a cursor.
    pub fn remote_cursor_embedded(&self, enabled: bool) {
        let mut state = lock(&self.state);
        state.remote_cursor_embedded = enabled;
        self.wake(&mut state);
    }
    pub fn status(&self, text: impl Into<String>) {
        let mut state = lock(&self.state);
        state.status = text.into();
        state.notice = None;
        if state.status == "Reconnecting" {
            self.input_state.cancel_pointers();
            state.report.input_acks_canceled += state.input_latency.pending.len() as u64;
            state.input_latency.pending.clear();
            state.report.clipboard_transfers_canceled +=
                state.clipboard_latency.pending.len() as u64;
            state.clipboard_latency.pending.clear();
            state.clipboard.reset();
            if let Some(probe) = &mut state.visual_probe {
                probe.reset();
            }
        }
        if state.status == "Reconnecting" && state.interrupted.is_none() {
            state.interrupted = Some(Instant::now());
            state.report.reconnects += 1;
        }
        self.wake(&mut state);
    }

    /// Stage clipboard metadata or a bounded chunk for the owning window.
    /// Only its main-thread focus/generation state can request and publish it.
    pub fn clipboard_event(&self, event: rds_core::DesktopEvent) {
        let mut state = lock(&self.state);
        match event {
            rds_core::DesktopEvent::ClipboardOffer { id, format, bytes } => {
                state.clipboard.offer(id, format, bytes)
            }
            rds_core::DesktopEvent::ClipboardChunk {
                id,
                offset,
                total,
                data,
            } => {
                if let Err(error) = state.clipboard.chunk(id, offset, total, data) {
                    tracing::warn!(%error, "remote clipboard transfer refused");
                }
            }
            rds_core::DesktopEvent::ClipboardError { id, code } => {
                state.clipboard.failed(id);
                tracing::warn!(?code, "remote clipboard unavailable");
            }
            _ => return,
        }
        self.wake(&mut state);
    }

    pub fn display_extent(&self, width: u32, height: u32) {
        lock(&self.state).extent = (width, height);
    }
    /// A local epoch distinguishes frame/input sequence numbers after reconnect.
    pub fn begin_session(&self, span: tracing::Span) {
        self.input_state.cancel_pointers();
        let mut state = lock(&self.state);
        state.clipboard.reset();
        state.frames = FrameMailbox::default();
        state.submission_debt.hidden();
        state.session_span = span;
        state.report.session_epoch = state.report.session_epoch.saturating_add(1);
        tracing::info!(parent: &state.session_span,
            viewer_pid = std::process::id(),
            viewer_epoch = state.report.session_epoch,
            "native desktop attempt started"
        );
    }
    /// Configure an opt-in controlled marker; no pixels or text enter reports.
    pub fn visual_probe(&self, spec: super::VisualProbeSpec) -> Result<(), DesktopError> {
        lock(&self.state).visual_probe = Some(super::visual_probe::VisualProbe::new(spec)?);
        Ok(())
    }
    pub fn control_rtt(&self, ms: u64) {
        let mut state = lock(&self.state);
        state.report.control_rtt_ms = Some(ms);
        state.report.last_control_echo_ms = Some(self.started.elapsed().as_millis() as u64);
    }
    pub fn input_sent(&self, control: &DesktopControl) {
        if matches!(control, DesktopControl::RequestIdr) {
            self.video_repair_requested();
        }
        if let DesktopControl::Input(event) = control {
            let sent_ms = self.started.elapsed().as_millis() as u64;
            {
                let mut state = lock(&self.state);
                state.report.inputs_dispatched += 1;
                state.input_latency.sent(
                    event.seq,
                    event.event_ts_ms,
                    sent_ms,
                    InputClass::of(&event.kind),
                );
            }
            let event_class = match event.kind {
                InputKind::KeyDown { .. } => "key_down",
                InputKind::KeyUp { .. } => "key_up",
                InputKind::PointerMove { .. } | InputKind::PointerMotion { .. } => "pointer_move",
                InputKind::PointerButton { pressed: true, .. } => "button_down",
                InputKind::PointerButton { pressed: false, .. } => "button_up",
                InputKind::Scroll { .. } => "scroll",
            };
            let epoch = lock(&self.state).report.session_epoch;
            tracing::trace!(target: "rds_desktop::input_timing", viewer_epoch=epoch, input_seq=event.seq,event_class,event_created_ms=event.event_ts_ms,input_sent_ms=sent_ms,queue_ms=sent_ms.saturating_sub(event.event_ts_ms),"native input dispatched");
        }
    }
    pub fn video_repair_requested(&self) {
        lock(&self.state).report.video_repair_requests += 1;
    }
    pub fn managed_events_separated(&self) {
        lock(&self.state).report.managed_events_separated = true;
    }
    pub fn input_ack(&self, seq: u64) {
        self.input_state.pointer_ack(seq);
        let mut state = lock(&self.state);
        state.report.input_acks += 1;
        let ack_ms = self.started.elapsed().as_millis() as u64;
        let latency = state.input_latency.ack(seq, ack_ms);
        state.report.last_input_ack_ms = latency;
        let epoch = state.report.session_epoch;
        let slow = latency.is_some_and(|ms| ms >= 250.);
        if slow {
            state.report.slow_input_acks += 1;
        }
        let log_slow = slow
            && state
                .input_latency
                .last_slow_log_ms
                .is_none_or(|last| ack_ms.saturating_sub(last) >= 1000);
        if log_slow {
            state.input_latency.last_slow_log_ms = Some(ack_ms);
        }
        if latency.is_some() {
            state.report.input_acks_matched += 1;
        } else {
            state.report.input_acks_unmatched += 1;
        }
        drop(state);
        if log_slow {
            tracing::warn!(target: "rds_desktop::input_timing", viewer_epoch=epoch, input_seq=seq, event_to_ack_ms=?latency, "native input acknowledgement delayed");
        }
        tracing::trace!(target: "rds_desktop::input_timing", viewer_epoch=epoch, input_seq=seq, input_ack_ms=ack_ms,
            event_to_ack_ms=?latency, "native input acknowledgement observed");
    }
    pub fn clipboard_ready(&self, bytes: u32) {
        let mut state = lock(&self.state);
        state.report.clipboard_transfers += 1;
        state.report.last_clipboard_bytes = bytes;
    }
    /// Correlate explicit paste publication using only metadata and local clocks.
    pub fn clipboard_ack(&self, id: u64, bytes: u32) {
        let now_ms = self.started.elapsed().as_millis() as u64;
        let mut state = lock(&self.state);
        let latency = state.clipboard_latency.ack(id, bytes, now_ms);
        if latency.is_some() {
            state.report.clipboard_transfers += 1;
            state.report.last_clipboard_bytes = bytes;
            state.report.last_clipboard_transfer_ms = latency;
            if latency.is_some_and(|ms| ms >= 250.) {
                state.report.slow_clipboard_transfers += 1;
            }
        } else {
            state.report.clipboard_unmatched_replies += 1;
        }
        drop(state);
        tracing::info!(transfer_id=id, bytes, transfer_ms=?latency, "native clipboard ready observed");
    }
    /// Sender stage durations use only that sender's monotonic clock. They
    /// are separate from network transit and local receive-to-submit timing.
    pub fn media_timing(&self, header: &rds_core::FrameHeader) {
        let mut state = lock(&self.state);
        state.last_encoded_ms = Some(self.started.elapsed().as_millis() as u64);
        sample(
            &mut state.encoding,
            header
                .encode_done_ts_ms
                .saturating_sub(header.capture_ts_ms) as f64,
        );
        sample(
            &mut state.sending,
            header.send_ts_ms.saturating_sub(header.encode_done_ts_ms) as f64,
        );
    }
    pub fn close(&self) {
        let mut state = lock(&self.state);
        state.close = true;
        self.wake(&mut state);
    }
    pub fn report(&self) -> ViewerReport {
        let state = lock(&self.state);
        let mut report = state.report.clone();
        (
            report.receive_to_submit_p50_ms,
            report.receive_to_submit_p95_ms,
        ) = quantiles(&state.delays);
        (report.capture_encode_p50_ms, report.capture_encode_p95_ms) = quantiles(&state.encoding);
        (report.encode_to_send_p50_ms, report.encode_to_send_p95_ms) = quantiles(&state.sending);
        (report.input_ack_p50_ms, report.input_ack_p95_ms) =
            quantiles(&state.input_latency.acknowledged);
        (
            report.keyboard_input_ack_p50_ms,
            report.keyboard_input_ack_p95_ms,
        ) = quantiles(&state.input_latency.keyboard);
        report.keyboard_input_ack_max_ms = state
            .input_latency
            .keyboard
            .iter()
            .copied()
            .reduce(f64::max);
        (
            report.button_input_ack_p50_ms,
            report.button_input_ack_p95_ms,
        ) = quantiles(&state.input_latency.buttons);
        report.button_input_ack_max_ms =
            state.input_latency.buttons.iter().copied().reduce(f64::max);
        report.input_queue_p95_ms = quantiles(&state.input_latency.queued).1;
        (
            report.clipboard_transfer_p50_ms,
            report.clipboard_transfer_p95_ms,
        ) = quantiles(&state.clipboard_latency.acknowledged);
        report.clipboard_transfer_max_ms = state
            .clipboard_latency
            .acknowledged
            .iter()
            .copied()
            .reduce(f64::max);
        report.clipboard_pending_transfers = state.clipboard_latency.pending.len();
        report.clipboard_oldest_pending_age_ms =
            state
                .clipboard_latency
                .pending
                .front()
                .map(|(_, _, since)| {
                    (self.started.elapsed().as_millis() as u64).saturating_sub(*since)
                });
        report.clipboard_tracking_evicted = state.clipboard_latency.evicted;
        report.input_ack_tracking_evicted = state.input_latency.evicted;
        report.pending_input_acks = state.input_latency.pending.len();
        report.oldest_input_ack_age_ms =
            state.input_latency.pending.front().map(|(_, created, _)| {
                (self.started.elapsed().as_millis() as u64).saturating_sub(*created)
            });
        report.input_pointer_coalesced = self.input_state.pointer_coalesced.load(Ordering::Relaxed);
        report.input_events_dropped = self.input_state.input_dropped.load(Ordering::Relaxed);
        report.input_queue_max_depth = self.input_state.max_depth.load(Ordering::Relaxed);
        report.input_queue_depth = self
            .input_state
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        report.visual_probe = state.visual_probe.as_ref().map(|probe| probe.report());
        report
    }
}

pub struct Viewer {
    event_loop: EventLoop<()>,
    app: App,
    screens: Option<spaces::Screens>,
}
impl Viewer {
    /// Must be constructed and run on the OS main thread. No identity or socket
    /// is owned by the viewer; the caller owns its asynchronous session task.
    pub fn new(display: u32) -> Result<(Self, ViewerHandle, InputReceiver), DesktopError> {
        let event_loop =
            super::platform::event_loop().map_err(|e| DesktopError::Capture(e.to_string()))?;
        let (session, handle, receiver) = SessionView::new(display, event_loop.create_proxy());
        let app = App {
            window: None,
            gpu: None,
            session,
            error: None,
            last_window_status: None,
            workspace: None,
            activity: None,
            modifiers: Default::default(),
            scoped_window: false,
            window_closed: false,
            fullscreen: false,
            native_input: NativeInputPolicy::default(),
            shortcuts: None,
        };
        Ok((
            Self {
                event_loop,
                app,
                screens: None,
            },
            handle,
            receiver,
        ))
    }
    pub fn run(mut self) -> Result<(), DesktopError> {
        if let Some(mut screens) = self.screens.take() {
            self.event_loop
                .run_app(&mut screens)
                .map_err(|e| DesktopError::Capture(e.to_string()))?;
            return screens.result();
        }
        if self.app.workspace.is_none() {
            self.app.activity = Some(super::platform::remote_activity());
        }
        self.event_loop
            .run_app(&mut self.app)
            .map_err(|e| DesktopError::Capture(e.to_string()))?;
        match self.app.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Input ownership and one bounded image/clipboard mailbox belong to one
/// session, independently of the native window used to present it.
struct SessionView {
    tab: Option<super::workspace::TabId>,
    handle: ViewerHandle,
    input: InputSender,
    display: u32,
    seq: u64,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<i32>,
    pointer: bool,
    pointer_point: Option<(f64, f64)>,
}
impl SessionView {
    fn clipboard_work(&mut self) -> Result<bool, DesktopError> {
        if !lock(&self.handle.state).clipboard.needs_work() {
            return Ok(false);
        }
        let Ok(generation) = super::platform::clipboard_generation() else {
            return Ok(false);
        };
        let text = lock(&self.handle.state).clipboard.publish(generation);
        let mut published = false;
        if let Some(text) = text {
            match super::platform::publish_text(&text) {
                Ok(()) => published = true,
                Err(error) => {
                    lock(&self.handle.state).clipboard.publication_failed();
                    self.handle
                        .notice(format!("Clipboard unavailable: {error}"));
                }
            }
        }
        // Publication changes native ownership; observe it before the next request.
        let Ok(generation) = super::platform::clipboard_generation() else {
            lock(&self.handle.state).clipboard.publication_failed();
            return Ok(published);
        };
        if published {
            lock(&self.handle.state).clipboard.published(generation);
        }
        let request = lock(&self.handle.state).clipboard.request(generation);
        if let Some(control) = request {
            self.input
                .send(ViewerInput::Control(control))
                .map_err(|_| DesktopError::Input("clipboard request queue full".into()))?;
        }
        Ok(published)
    }
    fn new(display: u32, proxy: EventLoopProxy<()>) -> (Self, ViewerHandle, InputReceiver) {
        let input_state = Arc::new(InputState {
            queue: Mutex::new(VecDeque::new()),
            pointer_in_flight: Mutex::new(VecDeque::new()),
            ready: Notify::new(),
            pointer_coalesced: AtomicU64::new(0),
            input_dropped: AtomicU64::new(0),
            max_depth: AtomicU64::new(0),
        });
        let (input, receiver) = (
            InputSender(input_state.clone()),
            InputReceiver(input_state.clone()),
        );
        let handle = ViewerHandle {
            state: Arc::new(Mutex::new(State {
                frames: FrameMailbox::default(),
                submission_debt: SubmissionDebt::default(),
                wake_pending: false,
                presenting: true,
                remote_cursor_embedded: false,
                status: "Connecting".into(),
                notice: None,
                label: "RDS".into(),
                extent: (0, 0),
                report: ViewerReport::default(),
                delays: Vec::new(),
                encoding: Vec::new(),
                sending: Vec::new(),
                input_latency: InputLatency::default(),
                clipboard_latency: ClipboardLatency::default(),
                clipboard: super::clipboard::Clipboard::default(),
                close: false,
                interrupted: None,
                network_stage: "starting".into(),
                render_stage: "starting".into(),
                last_ui_ms: 0,
                last_encoded_ms: None,
                occluded: false,
                native_window: None,
                visual_probe: None,
                session_span: tracing::Span::none(),
            })),
            input_state,
            proxy,
            started: Instant::now(),
        };
        let session = Self {
            tab: None,
            handle: handle.clone(),
            input,
            display,
            seq: 0,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            pointer: false,
            pointer_point: None,
        };
        (session, handle, receiver)
    }
}

/// Whether the remote video is live; a notice such as a clipboard failure does not end it.
fn remote_connected(status: &str, presenting: bool, close: bool) -> bool {
    status == "Connected" && presenting && !close
}

fn window_title(label: &str, status: &str, notice: Option<&str>) -> String {
    match notice {
        Some(notice) => format!("{label} — {status} — {notice}"),
        None => format!("{label} — {status}"),
    }
}

/// Pointer visibility follows video hover; shortcut ownership follows keyboard focus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NativeInputPolicy {
    hide_cursor: bool,
    inhibit_shortcuts: bool,
}
impl NativeInputPolicy {
    fn for_remote(
        focused: bool,
        connected: bool,
        interactive: bool,
        local_ui: bool,
        video_hover: bool,
        embedded_cursor: bool,
    ) -> Self {
        let remote_keyboard = focused && connected && interactive && !local_ui;
        Self {
            hide_cursor: remote_keyboard && video_hover && embedded_cursor,
            inhibit_shortcuts: remote_keyboard,
        }
    }
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    session: SessionView,
    error: Option<DesktopError>,
    last_window_status: Option<String>,
    workspace: Option<workspace::Deck>,
    activity: Option<super::platform::RemoteActivity>,
    // Physical modifier flags belong to the window, not a replaceable tab.
    modifiers: winit::event::Modifiers,
    scoped_window: bool,
    window_closed: bool,
    fullscreen: bool,
    native_input: NativeInputPolicy,
    shortcuts: Option<super::platform::KeyboardShortcuts>,
}
impl App {
    fn sync_native_input(&mut self, after_ui_output: bool) {
        let Some(window) = &self.window else {
            return;
        };
        let (connected, embedded_cursor) = {
            let state = lock(&self.session.handle.state);
            (
                remote_connected(&state.status, state.presenting, state.close),
                state.remote_cursor_embedded,
            )
        };
        let (interactive, local_ui) = self.workspace.as_ref().map_or((true, false), |deck| {
            (
                deck.active
                    .and_then(|id| deck.model.tab(id))
                    .is_some_and(|tab| tab.spec.profile.interactive),
                deck.chrome
                    .as_ref()
                    .is_some_and(super::workspace::chrome::Chrome::editing),
            )
        });
        let policy = NativeInputPolicy::for_remote(
            window.has_focus(),
            connected,
            interactive,
            local_ui,
            self.session.pointer,
            embedded_cursor,
        );
        // egui may restore visibility when its cursor icon changes during paint.
        // Reassert remote-video ownership after native UI output was applied.
        if after_ui_output || policy.hide_cursor != self.native_input.hide_cursor {
            window.set_cursor_visible(!policy.hide_cursor);
        }
        if let Some(shortcuts) = &mut self.shortcuts {
            shortcuts.set_active(policy.inhibit_shortcuts);
        }
        self.native_input = policy;
    }
    fn clipboard_work(&mut self, event_loop: &ActiveEventLoop) {
        if self.workspace_clipboard_work(event_loop) {
            return;
        }
        if let Err(error) = self.session.clipboard_work() {
            self.fail_input(event_loop, error);
        }
    }
    fn paste_text(&mut self, event_loop: &ActiveEventLoop, command_paste: bool) -> bool {
        match super::platform::paste_text() {
            Ok(Some(text)) => {
                let id = rand::random();
                let total = text.len() as u32;
                lock(&self.session.handle.state).clipboard_latency.sent(
                    id,
                    total,
                    self.session.handle.started.elapsed().as_millis() as u64,
                );
                tracing::info!(
                    transfer_id = id,
                    bytes = total,
                    command_paste,
                    "explicit local clipboard text queued"
                );
                let chunks = text.as_bytes().chunks(crate::clipboard::SEND_CHUNK_BYTES);
                for (index, chunk) in chunks.enumerate() {
                    if self
                        .session
                        .input
                        .send(ViewerInput::Control(DesktopControl::ClipboardChunk {
                            id,
                            offset: (index * crate::clipboard::SEND_CHUNK_BYTES) as u32,
                            total,
                            data: chunk.to_vec(),
                        }))
                        .is_err()
                    {
                        self.fail_input(
                            event_loop,
                            DesktopError::Input("clipboard input queue full".into()),
                        );
                        return false;
                    }
                }
                if total == 0
                    && self
                        .session
                        .input
                        .send(ViewerInput::Control(DesktopControl::ClipboardChunk {
                            id,
                            offset: 0,
                            total,
                            data: vec![],
                        }))
                        .is_err()
                {
                    self.fail_input(
                        event_loop,
                        DesktopError::Input("clipboard input queue full".into()),
                    );
                    return false;
                }
            }
            Ok(None) if command_paste => {
                tracing::info!("explicit local clipboard paste has no text");
                return false;
            }
            Ok(None) => {}
            Err(error) => {
                self.session
                    .handle
                    .notice(format!("Clipboard unavailable: {error}"));
                return false;
            }
        }
        true
    }
    fn sample_window(&self) {
        if let Some(window) = &self.window {
            let elapsed = self.session.handle.started.elapsed().as_millis() as u64;
            let sample = super::platform::window_state(window, elapsed);
            lock(&self.session.handle.state).native_window = sample;
        }
    }
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: DesktopError) {
        self.error = Some(error);
        self.close_window(event_loop);
    }
    fn close_window(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.set_cursor_visible(true);
        }
        if let Some(shortcuts) = &mut self.shortcuts {
            shortcuts.set_active(false);
        }
        self.native_input = NativeInputPolicy::default();
        self.release(event_loop);
        lock(&self.session.handle.state).clipboard.reset();
        let _ = self.session.input.send(ViewerInput::Close);
        if self.scoped_window {
            self.window_closed = true;
        } else {
            event_loop.exit();
        }
    }
    fn fail_input(&mut self, event_loop: &ActiveEventLoop, error: DesktopError) {
        if self.session.tab.is_some() {
            self.session
                .handle
                .status(format!("Input stopped: {error}"));
            self.session.handle.close();
            let _ = self.session.input.send(ViewerInput::Close);
        } else {
            self.fail(event_loop, error);
        }
    }
    fn sync_modifiers(&mut self, event_loop: &ActiveEventLoop) {
        for (code, pressed) in modifier_changes(&self.session.keys, self.modifiers) {
            if !key_transition(&mut self.session.keys, code, pressed) {
                continue;
            }
            self.input(
                event_loop,
                if pressed {
                    InputKind::KeyDown { code }
                } else {
                    InputKind::KeyUp { code }
                },
            );
        }
    }
    fn input(&mut self, event_loop: &ActiveEventLoop, kind: InputKind) {
        let Some(next) = self.session.seq.checked_add(1) else {
            self.fail_input(
                event_loop,
                DesktopError::Input("input sequence exhausted".into()),
            );
            return;
        };
        let message = DesktopControl::Input(InputEvent {
            seq: self.session.seq,
            event_ts_ms: self.session.handle.started.elapsed().as_millis() as u64,
            display_id: self.session.display,
            kind,
        });
        if matches!(
            &message,
            DesktopControl::Input(InputEvent {
                kind: InputKind::PointerButton {
                    button: 0x110,
                    pressed: true
                },
                ..
            })
        ) && let Some(point) = self.session.pointer_point
            && let Some(probe) = &mut lock(&self.session.handle.state).visual_probe
        {
            if self.modifiers.state().is_empty() {
                probe.click(point, self.session.seq, Instant::now());
            } else {
                probe.modified_click(point);
            }
        }
        if matches!(&message, DesktopControl::Input(InputEvent {
            kind: InputKind::PointerButton { button, pressed: true }, ..
        }) if *button != 0x110)
            && let Some(probe) = &mut lock(&self.session.handle.state).visual_probe
        {
            probe.keyboard_focus_lost();
        }
        if let DesktopControl::Input(InputEvent {
            kind: InputKind::KeyDown { code },
            ..
        }) = &message
            && let Some(probe) = &mut lock(&self.session.handle.state).visual_probe
        {
            probe.key(
                *code,
                self.modifiers.state().is_empty(),
                self.session.seq,
                Instant::now(),
            );
        }
        if let DesktopControl::Input(InputEvent {
            kind: InputKind::KeyDown { code } | InputKind::KeyUp { code },
            ..
        }) = &message
            && matches!(code, 29 | 42 | 54 | 56 | 97 | 100 | 125 | 126)
        {
            let pressed = matches!(
                &message,
                DesktopControl::Input(InputEvent {
                    kind: InputKind::KeyDown { .. },
                    ..
                })
            );
            tracing::trace!(target: "rds_desktop::input_timing", input_seq=self.session.seq, modifier_code=*code, pressed,
                "native modifier transition queued");
        }
        self.session.seq = next;
        if self
            .session
            .input
            .send(ViewerInput::Control(message))
            .is_err()
        {
            // Closing the remote session releases its held keys/buttons. Never
            // silently discard a key-up and leave a modifier stuck remotely.
            self.fail_input(
                event_loop,
                DesktopError::Input("input channel unavailable".into()),
            );
        }
    }
    fn release(&mut self, event_loop: &ActiveEventLoop) {
        for code in std::mem::take(&mut self.session.keys) {
            self.input(event_loop, InputKind::KeyUp { code });
        }
        for button in std::mem::take(&mut self.session.buttons) {
            self.input(
                event_loop,
                InputKind::PointerButton {
                    button,
                    pressed: false,
                },
            );
        }
    }
    fn update_window_title(&mut self) {
        let state = lock(&self.session.handle.state);
        let status = window_title(&state.label, &state.status, state.notice.as_deref());
        drop(state);
        if self.last_window_status.as_deref() != Some(status.as_str())
            && let Some(window) = &self.window
        {
            window.set_title(&status);
            self.last_window_status = Some(status);
        }
    }
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        if let (Some(deck), Some(window)) = (&mut self.workspace, &self.window)
            && let Some(chrome) = &mut deck.chrome
        {
            let status = lock(&self.session.handle.state).status.clone();
            chrome.prepare(window, &deck.model, &deck.devices, &status);
        }
        self.workspace_actions(event_loop);
        self.sync_native_input(self.workspace.is_some());
        self.update_window_title();
        lock(&self.session.handle.state).report.redraws += 1;
        let Some(gpu) = &mut self.gpu else {
            return;
        };
        let (pending, restore) = lock(&self.session.handle.state)
            .frames
            .take(gpu.has_picture());
        {
            let mut state = lock(&self.session.handle.state);
            if pending.is_some() {
                state
                    .submission_debt
                    .queued(self.session.handle.started.elapsed().as_millis() as u64);
            }
            state.render_stage = "acquiring surface".into();
        }
        let ui = self
            .workspace
            .as_mut()
            .and_then(|deck| deck.chrome.as_mut())
            .map(|chrome| &mut chrome.frame);
        let uploads_before = gpu.uploads();
        let result = gpu.draw(
            pending
                .as_ref()
                .map(|frame| frame.raw.as_ref())
                .or(restore.as_deref()),
            ui,
        );
        lock(&self.session.handle.state).report.gpu_uploads += gpu.uploads() - uploads_before;
        match result {
            Err(error) => self.fail(event_loop, error),
            Ok(DrawOutcome::Presented) => {
                lock(&self.session.handle.state).render_stage = "presented".into();
                if let Some(frame) = pending {
                    let mut state = lock(&self.session.handle.state);
                    let next_queued_ms = state.frames.pending.as_ref().map(|next| next.queued_ms);
                    state.submission_debt.presented(next_queued_ms);
                    let parent = state.session_span.clone();
                    tracing::trace!(target:"rds_desktop::frame_timing", parent:&parent, frame_seq=frame.frame_seq,
                        viewer_epoch=state.report.session_epoch, receive_to_submit_ms=frame.received.elapsed().as_secs_f64()*1000.,
                        "native frame submitted");
                    if let Some(probe) = &mut state.visual_probe {
                        parent.in_scope(|| {
                            probe.presented_with_seq(&frame.raw, Instant::now(), frame.frame_seq)
                        });
                    }
                    state.report.frames_submitted += 1;
                    state.report.last_submission_ms =
                        Some(self.session.handle.started.elapsed().as_millis() as u64);
                    state.render_stage = "presented".into();
                    state
                        .report
                        .first_frame_ms
                        .get_or_insert(self.session.handle.started.elapsed().as_millis() as u64);
                    sample(
                        &mut state.delays,
                        frame.received.elapsed().as_secs_f64() * 1000.,
                    );
                }
            }
            Ok(outcome) => {
                if matches!(outcome, DrawOutcome::Occluded) {
                    lock(&self.session.handle.state).submission_debt.hidden();
                }
                if matches!(outcome, DrawOutcome::Occluded)
                    && let Some(probe) = &mut lock(&self.session.handle.state).visual_probe
                {
                    probe.unavailable();
                }
                lock(&self.session.handle.state).render_stage = outcome.stage().into();
                lock(&self.session.handle.state).report.surface_skips += 1;
                if let Some(frame) = pending {
                    let mut state = lock(&self.session.handle.state);
                    if state.frames.pending.is_none() {
                        state.frames.pending = Some(frame);
                    }
                }
            }
        }
    }
}

impl ApplicationHandler<()> for App {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.sync_native_input(false);
        let paste_deadline = self.workspace_paste_deadline();
        let ui_deadline = self
            .workspace
            .as_mut()
            .and_then(|deck| deck.chrome.as_mut())
            .zip(self.window.as_ref())
            .and_then(|(chrome, window)| chrome.repaint_deadline(window));
        event_loop.set_control_flow(paste_deadline.into_iter().chain(ui_deadline).min().map_or(
            winit::event_loop::ControlFlow::Wait,
            winit::event_loop::ControlFlow::WaitUntil,
        ));
    }
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let icon = Icon::from_rgba(
            include_bytes!("../../assets/app-icon-256.rgba").to_vec(),
            256,
            256,
        );
        let result = icon
            .map_err(|e| DesktopError::Capture(format!("application icon: {e}")))
            .and_then(|icon| {
                event_loop
                    .create_window(
                        super::platform::window_attributes()
                            .with_title("RDS — Connecting")
                            .with_window_level(WindowLevel::Normal)
                            .with_window_icon(Some(icon))
                            .with_fullscreen(
                                self.fullscreen
                                    .then_some(winit::window::Fullscreen::Borderless(None)),
                            )
                            .with_inner_size(LogicalSize::new(1280., 720.)),
                    )
                    .map_err(|e| DesktopError::Capture(e.to_string()))
            })
            .and_then(|window| {
                let window = Arc::new(window);
                Gpu::new(window.clone(), event_loop.owned_display_handle()).map(|gpu| (window, gpu))
            });
        match result {
            Ok((window, gpu)) => {
                // Explicit viewer launches activate once like ordinary apps.
                // Reconnect/redraw never raises the window over other apps.
                if !self.scoped_window {
                    super::platform::activate_application();
                    window.focus_window();
                }
                window.request_redraw();
                if let Some(deck) = &mut self.workspace {
                    deck.chrome = Some(super::workspace::chrome::Chrome::new(&window));
                }
                self.shortcuts = super::platform::KeyboardShortcuts::new(
                    window.clone(),
                    event_loop.owned_display_handle(),
                );
                self.window = Some(window);
                self.gpu = Some(gpu);
                lock(&self.session.handle.state).clipboard.focus(true);
                self.workspace_updates(event_loop);
            }
            Err(error) => self.fail(event_loop, error),
        }
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _: ()) {
        if event_loop.exiting() {
            return;
        }
        self.workspace_updates(event_loop);
        // A queued workspace CloseWindow can exit during this same callback.
        // It must take precedence over a clipboard reply and deferred paste.
        if event_loop.exiting() {
            return;
        }
        self.sample_window();
        let mut state = lock(&self.session.handle.state);
        state.wake_pending = false;
        let close = state.close;
        state.last_ui_ms = self.session.handle.started.elapsed().as_millis() as u64;
        drop(state);
        if close {
            if self.workspace.is_some() {
                self.workspace_close_active(event_loop);
            } else {
                self.release(event_loop);
                event_loop.exit();
            }
            return;
        }
        self.clipboard_work(event_loop);
        self.update_window_title();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        if event_loop.exiting() {
            return;
        }
        // Focus-generated key snapshots are not fresh user gestures. Held
        // modifiers are reconciled when the next real input arrives.
        if matches!(
            event,
            WindowEvent::KeyboardInput {
                is_synthetic: true,
                ..
            }
        ) {
            return;
        }
        self.sample_window();
        lock(&self.session.handle.state).last_ui_ms =
            self.session.handle.started.elapsed().as_millis() as u64;
        if self.workspace_event(event_loop, &event) {
            self.sync_native_input(false);
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                self.close_window(event_loop);
            }
            WindowEvent::Resized(size) => {
                // The previous hit test belongs to the old video/letterbox geometry.
                self.session.pointer = false;
                self.session.pointer_point = None;
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            WindowEvent::Focused(false) => {
                self.session.pointer = false;
                self.session.pointer_point = None;
                self.modifiers = Default::default();
                lock(&self.session.handle.state).clipboard.focus(false);
                if let Some(probe) = &mut lock(&self.session.handle.state).visual_probe {
                    probe.keyboard_focus_lost();
                }
                self.release(event_loop);
            }
            WindowEvent::Occluded(hidden) => {
                let mut state = lock(&self.session.handle.state);
                state.occluded = hidden;
                if hidden {
                    state.submission_debt.hidden();
                } else if state.frames.pending.is_some() {
                    state
                        .submission_debt
                        .queued(self.session.handle.started.elapsed().as_millis() as u64);
                }
                if hidden && let Some(probe) = &mut state.visual_probe {
                    probe.unavailable();
                }
                drop(state);
                if !hidden && let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::Focused(true) => {
                lock(&self.session.handle.state).clipboard.focus(true);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
                self.sync_modifiers(event_loop);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(key) = event.physical_key
                    && let Some(code) = evdev(key)
                {
                    if event.repeat {
                        // Repeat only a key actually forwarded and still held.
                        // Never re-run clipboard/local shortcut side effects.
                        if event.state == ElementState::Pressed
                            && self.session.keys.contains(&code)
                            && !matches!(
                                code,
                                29 | 42 | 54 | 56 | 97 | 100 | 125 | 126 | 58 | 69 | 70
                            )
                        {
                            self.input(event_loop, InputKind::KeyDown { code });
                        }
                        return;
                    }
                    if !matches!(code, 29 | 42 | 54 | 56 | 97 | 100 | 125 | 126) {
                        self.sync_modifiers(event_loop);
                    }
                    if event.state == ElementState::Pressed {
                        if self.session.keys.contains(&code) {
                            return;
                        }
                        let command_paste = cfg!(target_os = "macos")
                            && code == 47
                            && (self.session.keys.contains(&125)
                                || self.session.keys.contains(&126));
                        if code == 47
                            && (command_paste
                                || self.session.keys.contains(&29)
                                || self.session.keys.contains(&97))
                        {
                            if self.workspace_defer_paste(event_loop) {
                                return;
                            }
                            if self.workspace_clipboard_enabled()
                                && !self.paste_text(event_loop, command_paste)
                            {
                                return;
                            }
                        }
                        let command_copy = cfg!(target_os = "macos")
                            && (self.session.keys.contains(&125)
                                || self.session.keys.contains(&126));
                        if matches!(code, 45 | 46)
                            && (command_copy
                                || self.session.keys.contains(&29)
                                || self.session.keys.contains(&97))
                            && self.workspace_clipboard_enabled()
                        {
                            self.workspace_copying();
                        }
                        if cfg!(target_os = "macos")
                            && matches!(code, 45 | 46)
                            && (self.session.keys.contains(&125)
                                || self.session.keys.contains(&126))
                        {
                            for kind in super::input::command_chord(&self.session.keys, code) {
                                self.input(event_loop, kind);
                            }
                            return;
                        }
                        if command_paste {
                            for kind in super::input::command_paste_chord(&self.session.keys) {
                                self.input(event_loop, kind);
                            }
                            // The physical V is consumed locally: its release
                            // and OS repeats must not replay paste effects.
                            return;
                        }
                        if key_transition(&mut self.session.keys, code, true) {
                            self.input(event_loop, InputKind::KeyDown { code });
                        }
                    } else if key_transition(&mut self.session.keys, code, false) {
                        self.input(event_loop, InputKind::KeyUp { code });
                    }
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.session.pointer = false;
                self.session.pointer_point = None;
            }
            WindowEvent::CursorMoved { position, .. } => {
                let extent = lock(&self.session.handle.state).extent;
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    let area = self
                        .workspace
                        .as_ref()
                        .and_then(|deck| deck.chrome.as_ref())
                        .map(|chrome| {
                            let rect = chrome.frame.content;
                            [
                                f64::from(rect.left()),
                                f64::from(rect.top()),
                                f64::from(rect.width()),
                                f64::from(rect.height()),
                            ]
                        });
                    let viewport =
                        Viewport::content(size.width, size.height, extent.0, extent.1, area);
                    let point = viewport.pointer(position.x, position.y, extent.0, extent.1);
                    self.session.pointer = point.is_some();
                    self.session.pointer_point = point;
                    if let Some((x, y)) = point {
                        self.input(event_loop, InputKind::PointerMove { x, y });
                    }
                }
            }
            WindowEvent::MouseInput { button, state, .. } => {
                self.sync_modifiers(event_loop);
                let button = match button {
                    MouseButton::Left => 0x110,
                    MouseButton::Right => 0x111,
                    MouseButton::Middle => 0x112,
                    _ => return,
                };
                if state == ElementState::Pressed && self.session.pointer {
                    self.session.buttons.insert(button);
                    self.input(
                        event_loop,
                        InputKind::PointerButton {
                            button,
                            pressed: true,
                        },
                    );
                } else if state == ElementState::Released && self.session.buttons.remove(&button) {
                    self.input(
                        event_loop,
                        InputKind::PointerButton {
                            button,
                            pressed: false,
                        },
                    );
                }
            }
            WindowEvent::MouseWheel { delta, .. } if self.session.pointer => {
                self.sync_modifiers(event_loop);
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (f64::from(x), f64::from(y)),
                    MouseScrollDelta::PixelDelta(p) => (p.x / 40., p.y / 40.),
                };
                self.input(event_loop, InputKind::Scroll { dx, dy });
            }
            _ => {}
        }
        self.sync_native_input(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_cursor_and_shortcut_policy_keep_distinct_pointer_and_keyboard_ownership() {
        let remote = NativeInputPolicy::for_remote(true, true, true, false, true, true);
        assert!(remote.hide_cursor && remote.inhibit_shortcuts);
        // Legacy X11 captures do not embed a cursor: retain the local pointer,
        // independently of forwarding the focused window's keyboard shortcuts.
        let legacy = NativeInputPolicy::for_remote(true, true, true, false, true, false);
        assert!(!legacy.hide_cursor && legacy.inhibit_shortcuts);
        // Moving onto toolbar/letterboxing restores the local cursor without
        // handing Alt+Shift to the compositor while remote keyboard focus remains.
        let chrome_hover = NativeInputPolicy::for_remote(true, true, true, false, false, true);
        assert!(!chrome_hover.hide_cursor && chrome_hover.inhibit_shortcuts);
        for (focused, connected, interactive, local_ui) in [
            (false, true, true, false), // another local app owns focus
            (true, false, true, false), // disconnect/reconnect or no first frame
            (true, true, false, false), // view-only remote tab
            (true, true, true, true),   // native text field/dialog owns keyboard
        ] {
            for hovered in [false, true] {
                assert_eq!(
                    NativeInputPolicy::for_remote(
                        focused,
                        connected,
                        interactive,
                        local_ui,
                        hovered,
                        true
                    ),
                    NativeInputPolicy::default()
                );
            }
        }
        // Re-entering connected interactive video reacquires only this window's policy.
        assert_eq!(
            NativeInputPolicy::for_remote(true, true, true, false, true, true),
            remote
        );
    }

    #[test]
    fn a_notice_beside_the_connection_keeps_remote_keyboard_ownership() {
        // A clipboard failure is shown as a notice; the status stays the connection's.
        assert!(remote_connected("Connected", true, false));
        assert_eq!(
            window_title("USA", "Connected", Some("Clipboard unavailable: busy")),
            "USA — Connected — Clipboard unavailable: busy"
        );
        assert_eq!(
            window_title("USA", "Reconnecting", None),
            "USA — Reconnecting"
        );
        for (status, presenting, close) in [
            ("Clipboard unavailable: busy", true, false),
            ("Reconnecting", true, false),
            ("Connected", false, false),
            ("Connected", true, true),
        ] {
            assert!(!remote_connected(status, presenting, close));
        }
    }

    #[test]
    fn clipboard_measurements_match_size_and_id_without_replaying_duplicates() {
        let mut latency = ClipboardLatency::default();
        latency.sent(7, 4096, 100);
        assert_eq!(latency.ack(8, 4096, 180), None);
        assert_eq!(latency.ack(7, 4095, 190), None);
        assert_eq!(latency.pending.len(), 1);
        assert_eq!(latency.ack(7, 4096, 200), Some(100.));
        assert_eq!(latency.ack(7, 4096, 250), None);
        assert_eq!(latency.acknowledged, vec![100.]);
        for id in 10..30 {
            latency.sent(id, 4096, 300);
        }
        assert_eq!(latency.pending.len(), 8);
        assert_eq!(latency.evicted, 12);
        assert_eq!(latency.ack(10, 4096, 400), None);
        latency.pending.clear(); // An old epoch cannot complete a fresh paste.
        assert_eq!(latency.ack(29, 4096, 500), None);
    }
    #[test]
    fn submission_debt_distinguishes_idle_replacement_and_concurrent_arrival() {
        let mut debt = SubmissionDebt::default();
        assert_eq!(debt.age(90_000), None); // A still screen is not a renderer stall.
        debt.queued(90_000);
        assert_eq!(debt.age(90_001), Some(1)); // First update after a long idle.
        debt.queued(91_000);
        debt.queued(92_000);
        assert_eq!(debt.age(92_000), Some(2000)); // Newest images cannot hide a stall.
        debt.presented(Some(91_900)); // Another image arrived while the GPU drew.
        assert_eq!(debt.age(92_000), Some(100));
        debt.presented(None);
        assert_eq!(debt.age(180_000), None);
        debt.queued(180_000);
        debt.hidden();
        assert_eq!(debt.age(360_000), None); // Covered windows owe no visible submission.
        debt.queued(360_000); // The retained image becomes eligible again.
        assert_eq!(debt.age(360_001), Some(1));
    }
    fn event(seq: u64, kind: InputKind) -> ViewerInput {
        ViewerInput::Control(DesktopControl::Input(InputEvent {
            seq,
            event_ts_ms: 0,
            display_id: 0,
            kind,
        }))
    }
    #[test]
    fn input_latency_matches_sequences_and_keeps_bounded_local_clock_history() {
        let mut latency = InputLatency::default();
        latency.sent(7, 100, 120, InputClass::Keyboard);
        latency.sent(8, 110, 125, InputClass::Button);
        assert_eq!(latency.ack(8, 310), Some(200.));
        assert_eq!(latency.ack(8, 410), None); // duplicate is not another sample
        assert_eq!(latency.ack(999, 510), None); // unrelated ACK cannot correlate
        assert_eq!(latency.acknowledged, vec![200.]);
        assert_eq!(latency.queued, vec![20., 15.]);
        assert_eq!(
            latency.pending.front(),
            Some(&(7, 100, InputClass::Keyboard))
        );
        for seq in 100..1500 {
            latency.sent(seq, seq, seq + 3, InputClass::Other);
        }
        assert_eq!(latency.pending.len(), INPUT_QUEUE_CAPACITY);
        assert_eq!(latency.queued.len(), 1024);
        assert_eq!(
            latency.pending.front(),
            Some(&(476, 476, InputClass::Other))
        );
        assert_eq!(latency.evicted, 377);
    }

    #[test]
    fn fast_pointer_acknowledgements_do_not_hide_slow_keyboard_and_buttons() {
        let mut latency = InputLatency::default();
        latency.sent(1, 0, 0, InputClass::Keyboard);
        assert_eq!(latency.ack(1, 900), Some(900.));
        latency.sent(2, 0, 0, InputClass::Button);
        assert_eq!(latency.ack(2, 700), Some(700.));
        for seq in 3..2000 {
            latency.sent(seq, 1000, 1000, InputClass::Other);
            latency.ack(seq, 1060);
        }
        assert_eq!(quantiles(&latency.acknowledged).1, Some(60.));
        assert_eq!(quantiles(&latency.keyboard).1, Some(900.));
        assert_eq!(quantiles(&latency.buttons).1, Some(700.));
        assert_eq!(latency.ack(1, 2000), None);
        assert_eq!(latency.keyboard.len(), 1);
        assert_eq!(latency.buttons.len(), 1);
    }

    #[tokio::test]
    async fn pointer_collapse_keeps_click_and_release_order_and_close_wakes_receiver() {
        let state = Arc::new(InputState {
            queue: Mutex::new(VecDeque::new()),
            pointer_in_flight: Mutex::new(VecDeque::new()),
            ready: Notify::new(),
            pointer_coalesced: AtomicU64::new(0),
            input_dropped: AtomicU64::new(0),
            max_depth: AtomicU64::new(0),
        });
        let sender = InputSender(state.clone());
        let mut receiver = InputReceiver(state);
        sender
            .send(event(0, InputKind::PointerMove { x: 1., y: 1. }))
            .unwrap();
        sender
            .send(event(1, InputKind::PointerMove { x: 2., y: 2. }))
            .unwrap();
        sender
            .send(event(
                2,
                InputKind::PointerButton {
                    button: 0x110,
                    pressed: true,
                },
            ))
            .unwrap();
        sender
            .send(event(3, InputKind::PointerMove { x: 3., y: 3. }))
            .unwrap();
        sender
            .send(event(
                4,
                InputKind::PointerButton {
                    button: 0x110,
                    pressed: false,
                },
            ))
            .unwrap();
        for expected in [1, 2, 3, 4] {
            let Some(ViewerInput::Control(DesktopControl::Input(event))) = receiver.recv().await
            else {
                panic!("missing input");
            };
            assert_eq!(event.seq, expected);
        }
        drop(sender);
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
                .await
                .unwrap(),
            Some(ViewerInput::Close)
        ));
    }

    #[tokio::test]
    async fn rapid_semantic_burst_preserves_every_button_event() {
        let state = Arc::new(InputState {
            queue: Mutex::new(VecDeque::new()),
            pointer_in_flight: Mutex::new(VecDeque::new()),
            ready: Notify::new(),
            pointer_coalesced: AtomicU64::new(0),
            input_dropped: AtomicU64::new(0),
            max_depth: AtomicU64::new(0),
        });
        let sender = InputSender(state.clone());
        let receiver = InputReceiver(state.clone());
        for seq in 0..400 {
            sender
                .send(event(
                    seq * 2,
                    InputKind::PointerButton {
                        button: 0x110,
                        pressed: true,
                    },
                ))
                .unwrap();
            sender
                .send(event(
                    seq * 2 + 1,
                    InputKind::PointerButton {
                        button: 0x110,
                        pressed: false,
                    },
                ))
                .unwrap();
        }
        let mut seen = Vec::with_capacity(800);
        while let Some(ViewerInput::Control(DesktopControl::Input(input))) = receiver.try_recv() {
            seen.push(input.seq);
        }
        assert_eq!(seen.len(), 800);
        assert_eq!(seen, (0..800).collect::<Vec<_>>());
        assert_eq!(state.input_dropped.load(Ordering::Relaxed), 0);
        assert_eq!(state.pointer_coalesced.load(Ordering::Relaxed), 0);
        assert_eq!(state.max_depth.load(Ordering::Relaxed), 800);
    }

    fn test_input_state() -> Arc<InputState> {
        Arc::new(InputState {
            queue: Mutex::new(VecDeque::new()),
            pointer_in_flight: Mutex::new(VecDeque::new()),
            ready: Notify::new(),
            pointer_coalesced: AtomicU64::new(0),
            input_dropped: AtomicU64::new(0),
            max_depth: AtomicU64::new(0),
        })
    }

    #[test]
    fn thousand_hz_motion_over_150ms_wan_has_bounded_debt_and_delivers_latest_position() {
        let state = test_input_state();
        let sender = InputSender(state.clone());
        let receiver = InputReceiver(state.clone());
        let mut replies = VecDeque::new();
        let mut latency = InputLatency::default();
        let mut sent = Vec::new();
        for now in 0..1300_u64 {
            while replies.front().is_some_and(|(due, _)| *due <= now) {
                let (_, seq) = replies.pop_front().unwrap();
                state.pointer_ack(seq);
                assert!(latency.ack(seq, now).is_some()); // Only real replies.
            }
            if now < 1000 {
                sender
                    .send(event(
                        now,
                        InputKind::PointerMove {
                            x: now as f64,
                            y: 1.,
                        },
                    ))
                    .unwrap();
            }
            while let Some(ViewerInput::Control(DesktopControl::Input(input))) = receiver.next() {
                latency.sent(input.seq, input.seq, now, InputClass::Other);
                replies.push_back((now + 150, input.seq));
                sent.push(input);
            }
            assert!(state.pointer_in_flight.lock().unwrap().len() <= POINTER_IN_FLIGHT);
            assert!(state.queue.lock().unwrap().len() <= 1);
        }
        assert!(
            sent.len() <= 29,
            "motion rate must follow remote handled ACKs"
        );
        assert_eq!(sent.last().unwrap().seq, 999);
        assert!(matches!(
            sent.last().unwrap().kind,
            InputKind::PointerMove { x: 999., .. }
        ));
        assert_eq!(
            state.pointer_coalesced.load(Ordering::Relaxed),
            1000 - sent.len() as u64
        );
        assert_eq!(state.input_dropped.load(Ordering::Relaxed), 0);
        assert!(state.pointer_in_flight.lock().unwrap().is_empty());
        assert!(latency.pending.is_empty());
        assert_eq!(latency.evicted, 0);
    }

    fn fill_pointer_window(sender: &InputSender, receiver: &InputReceiver, first: u64) {
        for seq in first..first + POINTER_IN_FLIGHT as u64 {
            sender
                .send(event(
                    seq,
                    InputKind::PointerMove {
                        x: seq as f64,
                        y: 1.,
                    },
                ))
                .unwrap();
            assert!(receiver.next().is_some());
        }
    }

    #[test]
    fn pointer_window_keeps_semantic_and_cross_display_position_barriers() {
        let state = test_input_state();
        let sender = InputSender(state.clone());
        let receiver = InputReceiver(state.clone());
        fill_pointer_window(&sender, &receiver, 0);
        sender
            .send(event(10, InputKind::PointerMove { x: 10., y: 1. }))
            .unwrap();
        for (seq, kind) in [
            (
                11,
                InputKind::PointerButton {
                    button: 0x110,
                    pressed: true,
                },
            ),
            (12, InputKind::KeyDown { code: 30 }),
            (13, InputKind::Scroll { dx: 0., dy: 1. }),
            (14, InputKind::PointerMotion { dx: 1., dy: 0. }),
        ] {
            sender.send(event(seq, kind)).unwrap();
        }
        let ViewerInput::Control(DesktopControl::Input(mut other)) =
            event(15, InputKind::PointerMove { x: 15., y: 1. })
        else {
            unreachable!()
        };
        other.display_id = 1;
        sender
            .send(ViewerInput::Control(DesktopControl::Input(other)))
            .unwrap();
        sender
            .send(event(16, InputKind::PointerMove { x: 16., y: 1. }))
            .unwrap();
        assert!(receiver.next().is_none());
        state.pointer_ack(0);
        for seq in 10..=14 {
            let Some(ViewerInput::Control(DesktopControl::Input(input))) = receiver.next() else {
                panic!("position/action barrier lost")
            };
            assert_eq!(input.seq, seq);
        }
        assert!(receiver.next().is_none());
        state.pointer_ack(1);
        let Some(ViewerInput::Control(DesktopControl::Input(input))) = receiver.next() else {
            panic!("display position lost")
        };
        assert_eq!((input.seq, input.display_id), (15, 1));
        assert!(receiver.next().is_none());
        state.pointer_ack(2);
        let Some(ViewerInput::Control(DesktopControl::Input(input))) = receiver.next() else {
            panic!("second display position lost")
        };
        assert_eq!((input.seq, input.display_id), (16, 0));
        assert_eq!(state.pointer_coalesced.load(Ordering::Relaxed), 0);
        assert_eq!(state.input_dropped.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn real_pointer_ack_wakes_latest_motion_but_unknown_ack_and_cancelled_wait_do_not_spend_credit()
     {
        let state = test_input_state();
        let sender = InputSender(state.clone());
        let mut receiver = InputReceiver(state.clone());
        fill_pointer_window(&sender, &receiver, 0);
        sender
            .send(event(10, InputKind::PointerMove { x: 10., y: 1. }))
            .unwrap();
        state.pointer_ack(999); // Unknown ACK cannot release an input slot.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), receiver.recv())
                .await
                .is_err()
        );
        sender
            .send(event(11, InputKind::PointerMove { x: 11., y: 1. }))
            .unwrap();
        let acknowledging = state.clone();
        let ack = tokio::spawn(async move {
            tokio::task::yield_now().await;
            acknowledging.pointer_ack(0);
        });
        let Some(ViewerInput::Control(DesktopControl::Input(input))) = receiver.recv().await else {
            panic!("ACK did not wake input")
        };
        ack.await.unwrap();
        assert_eq!(input.seq, 11);
        state.pointer_ack(0); // Duplicate ACK cannot release another slot.
        sender
            .send(event(12, InputKind::PointerMove { x: 12., y: 1. }))
            .unwrap();
        assert!(receiver.next().is_none());
        sender.send(ViewerInput::Close).unwrap();
        assert!(matches!(receiver.recv().await, Some(ViewerInput::Close)));
        assert_eq!(
            state.pointer_in_flight.lock().unwrap().len(),
            POINTER_IN_FLIGHT
        );
        assert_eq!(state.input_dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reconnect_cancels_old_pointer_window_without_granting_stale_ack_credit() {
        let state = test_input_state();
        let sender = InputSender(state.clone());
        let receiver = InputReceiver(state.clone());
        fill_pointer_window(&sender, &receiver, 0);
        sender
            .send(event(10, InputKind::PointerMove { x: 10., y: 1. }))
            .unwrap();
        state.cancel_pointers();
        assert!(receiver.try_recv().is_some()); // Discard disconnected input.
        fill_pointer_window(&sender, &receiver, 100);
        state.pointer_ack(0);
        sender
            .send(event(104, InputKind::PointerMove { x: 104., y: 1. }))
            .unwrap();
        assert!(receiver.next().is_none());
        state.pointer_ack(100);
        assert!(receiver.next().is_some());
        assert_eq!(state.input_dropped.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn missing_pointer_ack_fails_the_attempt_without_fabricating_credit_or_requiring_more_input()
     {
        let state = test_input_state();
        let sender = InputSender(state.clone());
        let mut receiver = InputReceiver(state.clone());
        fill_pointer_window(&sender, &receiver, 0);
        assert!(matches!(
            receiver.recv().await,
            Some(ViewerInput::InputStalled)
        ));
        assert_eq!(
            state.pointer_in_flight.lock().unwrap().len(),
            POINTER_IN_FLIGHT
        );
        assert_eq!(state.input_dropped.load(Ordering::Relaxed), 0);
        sender.send(ViewerInput::Close).unwrap();
        assert!(matches!(receiver.recv().await, Some(ViewerInput::Close)));
    }

    #[test]
    fn semantic_burst_acknowledgements_remain_correlated_when_replies_wait() {
        let mut latency = InputLatency::default();
        for seq in 0..800 {
            latency.sent(seq, seq, 800, InputClass::Button);
        }
        assert_eq!(latency.pending.len(), 800);
        assert_eq!(latency.evicted, 0);
        // Replies can be observed after dispatch has already sent the burst.
        for seq in (0..800).rev() {
            assert_eq!(latency.ack(seq, 1000), Some((1000 - seq) as f64));
        }
        assert!(latency.pending.is_empty());
        assert_eq!(latency.acknowledged.len(), 800);
        assert_eq!(latency.ack(799, 1001), None);
    }

    #[test]
    fn full_queue_never_removes_a_position_before_a_click() {
        for incoming in [
            InputKind::PointerMove { x: 999., y: 999. },
            InputKind::KeyDown { code: 30 },
            InputKind::PointerButton {
                button: 0x110,
                pressed: false,
            },
        ] {
            let state = test_input_state();
            let sender = InputSender(state.clone());
            let receiver = InputReceiver(state.clone());
            for seq in 0..INPUT_QUEUE_CAPACITY as u64 {
                let kind = if seq == INPUT_QUEUE_CAPACITY as u64 - 1 {
                    InputKind::KeyUp { code: 30 }
                } else {
                    match seq % 3 {
                        0 => InputKind::PointerMove {
                            x: seq as f64,
                            y: 1.,
                        },
                        1 => InputKind::PointerButton {
                            button: 0x110,
                            pressed: true,
                        },
                        _ => InputKind::PointerButton {
                            button: 0x110,
                            pressed: false,
                        },
                    }
                };
                sender.send(event(seq, kind)).unwrap();
            }
            // End on a semantic barrier; incoming motion must not remove
            // an earlier click's position to make room.
            assert!(
                sender
                    .send(event(INPUT_QUEUE_CAPACITY as u64, incoming))
                    .is_err()
            );
            for seq in 0..INPUT_QUEUE_CAPACITY as u64 {
                let Some(ViewerInput::Control(DesktopControl::Input(input))) = receiver.try_recv()
                else {
                    panic!("accepted action lost");
                };
                assert_eq!(input.seq, seq);
                if seq % 3 == 0 && seq < INPUT_QUEUE_CAPACITY as u64 - 1 {
                    assert!(
                        matches!(input.kind, InputKind::PointerMove { x, .. } if x == seq as f64)
                    );
                }
            }
            assert!(receiver.try_recv().is_none());
            assert_eq!(state.pointer_coalesced.load(Ordering::Relaxed), 0);
            assert_eq!(state.input_dropped.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn full_queue_can_still_replace_same_display_tail_motion() {
        let state = test_input_state();
        let sender = InputSender(state.clone());
        for seq in 0..INPUT_QUEUE_CAPACITY as u64 - 1 {
            sender
                .send(event(seq, InputKind::KeyDown { code: 30 }))
                .unwrap();
        }
        sender
            .send(event(1023, InputKind::PointerMove { x: 1., y: 1. }))
            .unwrap();
        sender
            .send(event(1024, InputKind::PointerMove { x: 2., y: 2. }))
            .unwrap();
        assert_eq!(state.queue.lock().unwrap().len(), INPUT_QUEUE_CAPACITY);
        assert_eq!(state.pointer_coalesced.load(Ordering::Relaxed), 1);
        assert_eq!(state.input_dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pointer_coalescing_does_not_cross_displays() {
        let state = test_input_state();
        let sender = InputSender(state.clone());
        let receiver = InputReceiver(state.clone());
        sender
            .send(event(0, InputKind::PointerMove { x: 1., y: 1. }))
            .unwrap();
        let ViewerInput::Control(DesktopControl::Input(mut other)) =
            event(1, InputKind::PointerMove { x: 2., y: 2. })
        else {
            unreachable!()
        };
        other.display_id = 1;
        sender
            .send(ViewerInput::Control(DesktopControl::Input(other)))
            .unwrap();
        assert!(receiver.try_recv().is_some());
        assert!(receiver.try_recv().is_some());
        assert_eq!(state.pointer_coalesced.load(Ordering::Relaxed), 0);
    }
}
