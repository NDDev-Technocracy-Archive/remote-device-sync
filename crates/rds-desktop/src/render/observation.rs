//! Bounded native API wall time; no GPU execution or scanout claim.
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RendererInfo {
    pub backend: String,
    pub device_type: String,
    pub present_mode: String,
    pub surface_format: String,
    pub desired_maximum_frame_latency: u32,
}

#[derive(Clone, Default, Debug, serde::Serialize)]
pub struct RenderStageTiming {
    pub calls: u64,
    pub last_us: u64,
    pub maximum_us: u64,
    pub total_us: u64,
}
impl RenderStageTiming {
    fn record(&mut self, micros: u64) {
        self.calls = self.calls.saturating_add(1);
        self.last_us = micros;
        self.maximum_us = self.maximum_us.max(micros);
        self.total_us = self.total_us.saturating_add(micros);
    }
}

#[derive(Clone, Default, Debug, serde::Serialize)]
pub struct RenderTimings {
    pub configuration: RenderStageTiming,
    pub acquisition: RenderStageTiming,
    pub upload: RenderStageTiming,
    pub commands: RenderStageTiming,
    pub submission: RenderStageTiming,
    pub presentation: RenderStageTiming,
    pub cleanup: RenderStageTiming,
}
impl RenderTimings {
    pub(super) fn record(&mut self, phase: Phase, micros: u64) {
        match phase {
            Phase::Configuration => &mut self.configuration,
            Phase::Acquisition => &mut self.acquisition,
            Phase::Upload => &mut self.upload,
            Phase::Commands => &mut self.commands,
            Phase::Submission => &mut self.submission,
            Phase::Presentation => &mut self.presentation,
            Phase::Cleanup => &mut self.cleanup,
        }
        .record(micros);
    }
}

#[derive(Clone, Copy)]
pub(super) enum Phase {
    Configuration,
    Acquisition,
    Upload,
    Commands,
    Submission,
    Presentation,
    Cleanup,
}
impl Phase {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Configuration => "configuring surface",
            Self::Acquisition => "acquiring surface",
            Self::Upload => "uploading picture",
            Self::Commands => "recording draw commands",
            Self::Submission => "submitting commands",
            Self::Presentation => "presenting surface",
            Self::Cleanup => "releasing renderer resources",
        }
    }
}

/// One phase transition and the preceding returned attempt, if any.
pub(super) struct DrawUpdate {
    pub current: Option<(Phase, Instant)>,
    pub completed: Option<(Phase, u64)>,
}

/// Publish a phase before entering its native call, outside the viewer lock.
/// Readers can identify a blocked call while it is still running. Drop accounts
/// the last returned attempt on every early return/error, not only on a frame
/// which was presented. The caller owns the final draw outcome independently.
pub(super) struct DrawObservation<'a> {
    sink: &'a mut dyn FnMut(DrawUpdate),
    phase: Option<Phase>,
    started: Instant,
}
impl<'a> DrawObservation<'a> {
    pub(super) fn new(sink: &'a mut dyn FnMut(DrawUpdate)) -> Self {
        Self {
            sink,
            phase: None,
            started: Instant::now(),
        }
    }
    fn sample(&self, now: Instant) -> Option<(Phase, u64)> {
        self.phase.map(|phase| {
            (
                phase,
                now.saturating_duration_since(self.started)
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64,
            )
        })
    }
    pub(super) fn enter(&mut self, phase: Phase) {
        let now = Instant::now();
        let completed = self.sample(now);
        (self.sink)(DrawUpdate {
            current: Some((phase, now)),
            completed,
        });
        self.phase = Some(phase);
        self.started = now;
    }
}
impl Drop for DrawObservation<'_> {
    fn drop(&mut self) {
        let completed = self.sample(Instant::now());
        (self.sink)(DrawUpdate {
            current: None,
            completed,
        });
    }
}
