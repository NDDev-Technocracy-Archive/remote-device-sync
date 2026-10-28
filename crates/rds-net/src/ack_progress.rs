//! Passive per-controller acknowledgement progress, for opt-in path policy.
//! Every congestion callback and metric delegates unchanged to the selected CCA.
use noq_proto::RttEstimator;
use noq_proto::congestion::{Controller, ControllerFactory, ControllerMetrics};
use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Observation {
    progress: Progress,
    now: Instant,
}
impl Observation {
    pub(crate) fn confirmed(self, rtt: Duration) -> bool {
        self.progress.confirmed(self.now, rtt)
    }
    pub(crate) fn stalled(self, rtt: Duration) -> bool {
        self.progress.stalled(self.now, rtt)
    }
    /// Closing a path gives up its in-flight work, so only a path without ACK progress for the
    /// longer confirmation budget is retired; a merely stalled one stays open, only ineligible.
    pub(crate) fn unresponsive(self, rtt: Duration) -> bool {
        self.progress.unresponsive(self.now, rtt)
    }
    /// Retiring reliable work needs proof from the sibling during the failure,
    /// not an idle ACK that happened before both routes could have stopped.
    pub(crate) fn can_replace(self, failed: Self, rtt: Duration) -> bool {
        let Some(pending_since) = failed.progress.pending_since else {
            return false;
        };
        self.progress
            .confirmed_at
            .is_some_and(|confirmed_at| confirmed_at > pending_since)
            && self.confirmed(rtt)
            && !self.stalled(rtt)
    }
    pub(crate) fn needs_probe(self) -> bool {
        self.progress.needs_probe(self.now)
    }
    pub(crate) fn pending_age(self) -> Option<Duration> {
        self.progress
            .pending_since
            .map(|at| self.now.saturating_duration_since(at))
    }
    pub(crate) fn confirmation_age(self) -> Option<Duration> {
        self.progress
            .confirmed_at
            .map(|at| self.now.saturating_duration_since(at))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Progress {
    pending_since: Option<Instant>,
    confirmed_at: Option<Instant>,
}
impl Progress {
    fn sent(&mut self, now: Instant, bytes: u64) {
        if bytes > 0 && self.pending_since.is_none() {
            self.pending_since = Some(now);
        }
    }
    fn ack(&mut self, now: Instant, bytes: u64) {
        if bytes > 0 {
            self.confirmed_at = Some(now);
            if self.pending_since.is_some() {
                self.pending_since = Some(now);
            }
        }
    }
    fn end_acks(&mut self, now: Instant, in_flight: u64) {
        if in_flight == 0 {
            self.pending_since = None;
        } else if self.pending_since.is_none() {
            self.pending_since = Some(now);
        }
    }
    pub(crate) fn confirmed(self, now: Instant, rtt: Duration) -> bool {
        let budget = rtt.saturating_mul(8).max(Duration::from_secs(2));
        self.confirmed_at
            .is_some_and(|at| now.saturating_duration_since(at) < budget)
    }
    pub(crate) fn needs_probe(self, now: Instant) -> bool {
        self.confirmed_at
            .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(1))
    }
    pub(crate) fn stalled(self, now: Instant, rtt: Duration) -> bool {
        let budget = rtt.saturating_mul(4).max(Duration::from_millis(500));
        self.pending_since
            .is_some_and(|since| now.saturating_duration_since(since) >= budget)
    }
    pub(crate) fn unresponsive(self, now: Instant, rtt: Duration) -> bool {
        let budget = rtt.saturating_mul(8).max(Duration::from_secs(2));
        self.pending_since
            .is_some_and(|since| now.saturating_duration_since(since) >= budget)
    }
}

struct Factory(Arc<dyn ControllerFactory + Send + Sync>, Clock);
impl std::fmt::Debug for Factory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AckProgressFactory").finish_non_exhaustive()
    }
}
impl ControllerFactory for Factory {
    fn build(self: Arc<Self>, now: Instant, mtu: u16) -> Box<dyn Controller> {
        Box::new(Tracked {
            inner: self.0.clone().build(now, mtu),
            progress: Progress::default(),
            reset_on_mutation: false,
            clock: self.1.clone(),
        })
    }
}
pub(crate) fn factory(
    inner: Arc<dyn ControllerFactory + Send + Sync>,
    clock: Clock,
) -> Arc<dyn ControllerFactory + Send + Sync> {
    Arc::new(Factory(inner, clock))
}
pub(crate) fn snapshot(controller: Box<dyn Controller>) -> Option<Observation> {
    controller
        .into_any()
        .downcast::<Tracked>()
        .ok()
        .map(|tracked| Observation {
            progress: tracked.progress,
            now: (tracked.clock)(),
        })
}

struct Tracked {
    inner: Box<dyn Controller>,
    progress: Progress,
    // Noq uses clone_box both for snapshots and live migration. Reading the
    // clone preserves snapshot evidence; its first live mutation starts a
    // new proof domain without guessing across QUIC packet-number spaces.
    reset_on_mutation: bool,
    clock: Clock,
}
impl std::fmt::Debug for Tracked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AckProgressController")
            .field("inner", &self.inner)
            .field("progress", &self.progress)
            .finish_non_exhaustive()
    }
}
impl Tracked {
    fn prepare(&mut self) {
        if self.reset_on_mutation {
            self.progress = Progress::default();
            self.reset_on_mutation = false;
        }
    }
}
impl Controller for Tracked {
    fn on_sent(&mut self, now: Instant, bytes: u64, packet: u64) {
        self.prepare();
        self.inner.on_sent(now, bytes, packet);
    }
    fn on_packet_sent(&mut self, now: Instant, bytes: u16, packet: u64) {
        self.prepare();
        self.inner.on_packet_sent(now, bytes, packet);
        // Noq invokes on_sent for every transmit, including ACK-only packets.
        // on_packet_sent is invoked only for ACK-eliciting packets. Observing
        // pure ACK transmission would create debt that has no required reply.
        self.progress.sent(now, bytes.into());
    }
    fn on_cwnd_limited(&mut self) {
        self.prepare();
        self.inner.on_cwnd_limited();
    }
    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        packet: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.prepare();
        self.inner
            .on_ack(now, sent, bytes, packet, app_limited, rtt);
        self.progress.ack(now, bytes);
    }
    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest: Option<u64>,
    ) {
        self.prepare();
        self.inner.on_end_acks(now, in_flight, app_limited, largest);
        self.progress.end_acks(now, in_flight);
    }
    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        persistent: bool,
        ecn: bool,
        bytes: u64,
        packet: u64,
    ) {
        self.prepare();
        self.inner
            .on_congestion_event(now, sent, persistent, ecn, bytes, packet);
    }
    fn on_packet_lost(&mut self, bytes: u16, packet: u64, now: Instant) {
        self.prepare();
        self.inner.on_packet_lost(bytes, packet, now);
    }
    fn on_spurious_congestion_event(&mut self) {
        self.prepare();
        self.inner.on_spurious_congestion_event();
    }
    fn on_mtu_update(&mut self, mtu: u16) {
        self.prepare();
        self.inner.on_mtu_update(mtu);
    }
    fn on_ack_frequency_update(&mut self, threshold: u64, delay: Duration) {
        self.prepare();
        self.inner.on_ack_frequency_update(threshold, delay);
    }
    fn window(&self) -> u64 {
        self.inner.window()
    }
    fn metrics(&self) -> ControllerMetrics {
        self.inner.metrics()
    }
    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Self {
            inner: self.inner.clone_box(),
            progress: self.progress,
            reset_on_mutation: true,
            clock: self.clock.clone(),
        })
    }
    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_requires_progress_after_the_outstanding_work_interval_began() {
        let epoch = Instant::now();
        let now = epoch + Duration::from_millis(1600);
        let rtt = Duration::from_millis(70);
        let failed = Observation {
            progress: Progress {
                pending_since: Some(epoch + Duration::from_secs(1)),
                confirmed_at: Some(epoch),
            },
            now,
        };
        assert!(failed.stalled(rtt));
        let mut standby = Observation {
            progress: Progress {
                pending_since: None,
                confirmed_at: Some(epoch),
            },
            now,
        };
        // Both paths can fail together while the idle standby still has a
        // nominally fresh pre-failure ACK. It is not a proved escape route.
        assert!(standby.confirmed(rtt));
        assert!(!standby.stalled(rtt));
        assert!(!standby.can_replace(failed, rtt));
        standby.progress.confirmed_at = failed.progress.pending_since;
        assert!(!standby.can_replace(failed, rtt));
        standby.progress.confirmed_at = Some(epoch + Duration::from_millis(1500));
        assert!(standby.can_replace(failed, rtt));
        standby.progress.pending_since = Some(epoch + Duration::from_secs(1));
        assert!(!standby.can_replace(failed, rtt));
        standby.progress.pending_since = None;
        standby.now += Duration::from_secs(2);
        assert!(!standby.can_replace(failed, rtt));
    }

    #[test]
    fn a_queued_path_is_ineligible_before_it_may_be_retired() {
        // A relay path shared with a desktop stream: 145 ms RTT and an 819 ms ACK gap.
        let epoch = Instant::now();
        let rtt = Duration::from_millis(145);
        let progress = Progress {
            pending_since: Some(epoch),
            confirmed_at: Some(epoch),
        };
        assert!(progress.stalled(epoch + Duration::from_millis(819), rtt));
        assert!(!progress.unresponsive(epoch + Duration::from_millis(819), rtt));
        assert!(!progress.unresponsive(epoch + Duration::from_millis(1999), rtt));
        assert!(progress.unresponsive(epoch + Duration::from_secs(2), rtt));
        // A high RTT keeps its own longer budget.
        let slow = Duration::from_secs(1);
        assert!(!progress.unresponsive(epoch + Duration::from_millis(7999), slow));
        assert!(progress.unresponsive(epoch + Duration::from_secs(8), slow));
        assert!(!Progress::default().unresponsive(epoch + Duration::from_secs(60), rtt));
    }

    #[test]
    fn high_rtt_does_not_extend_a_pre_failure_confirmation_into_replacement_proof() {
        let epoch = Instant::now();
        let now = epoch + Duration::from_secs(20);
        let standby = Observation {
            progress: Progress {
                pending_since: None,
                confirmed_at: Some(epoch),
            },
            now,
        };
        let failed = Observation {
            progress: Progress {
                pending_since: Some(epoch + Duration::from_secs(19)),
                confirmed_at: None,
            },
            now,
        };
        assert!(standby.confirmed(Duration::from_secs(3)));
        assert!(!standby.can_replace(failed, Duration::from_secs(3)));
        assert!(!standby.can_replace(
            Observation {
                progress: Progress::default(),
                now
            },
            Duration::from_secs(3)
        ));
    }
    #[test]
    fn a_high_rtt_path_is_not_failed_before_its_own_round_trip_budget() {
        let now = Instant::now();
        let rtt = Duration::from_secs(3);
        let mut progress = Progress::default();
        progress.sent(now, 100);
        assert!(!progress.stalled(now + Duration::from_secs(11), rtt));
        assert!(progress.stalled(now + Duration::from_secs(12), rtt));
        progress.ack(now, 100);
        assert!(progress.confirmed(now + Duration::from_secs(23), rtt));
        assert!(!progress.confirmed(now + Duration::from_secs(24), rtt));
    }
    #[test]
    fn observations_use_the_controller_runtime_clock_not_the_callers_wall_clock() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let epoch = Instant::now() + Duration::from_secs(3600);
        let elapsed = Arc::new(AtomicU64::new(0));
        let ticks = elapsed.clone();
        let clock: Clock =
            Arc::new(move || epoch + Duration::from_millis(ticks.load(Ordering::Relaxed)));
        let mut controller =
            factory(crate::CongestionControl::Cubic.factory(), clock).build(epoch, 1200);
        controller.on_packet_sent(epoch, 100, 0);
        controller.on_sent(epoch, 100, 0);
        elapsed.store(499, Ordering::Relaxed);
        assert!(
            !snapshot(controller.clone_box())
                .unwrap()
                .stalled(Duration::from_millis(70))
        );
        elapsed.store(500, Ordering::Relaxed);
        assert!(
            snapshot(controller.clone_box())
                .unwrap()
                .stalled(Duration::from_millis(70))
        );
    }
    #[test]
    fn snapshots_keep_evidence_but_a_live_clone_requires_its_own_confirmation() {
        let now = Instant::now();
        let inner = crate::CongestionControl::Cubic.factory().build(now, 1200);
        let mut tracked = Tracked {
            inner,
            progress: Progress::default(),
            reset_on_mutation: false,
            clock: Arc::new(move || now),
        };
        tracked.progress.ack(now, 100);
        let expected_window = tracked.window();
        let mut cloned = tracked
            .clone_box()
            .into_any()
            .downcast::<Tracked>()
            .unwrap();
        assert!(cloned.progress.confirmed(now, Duration::from_millis(70)));
        assert_eq!(cloned.window(), expected_window);
        cloned.on_mtu_update(1200);
        assert!(!cloned.progress.confirmed(now, Duration::from_millis(70)));
        assert!(tracked.progress.confirmed(now, Duration::from_millis(70)));
        assert_eq!(cloned.window(), tracked.window());
    }
    #[test]
    fn stale_idle_confirmation_requires_a_probe_and_cannot_retire_a_sibling() {
        let now = Instant::now();
        let mut progress = Progress::default();
        progress.ack(now, 100);
        assert!(!progress.needs_probe(now));
        assert!(progress.needs_probe(now + Duration::from_secs(1)));
        assert!(!progress.confirmed(now + Duration::from_secs(2), Duration::from_millis(70)));
        assert!(!progress.stalled(now + Duration::from_secs(30), Duration::from_millis(70)));
    }

    #[test]
    fn unacknowledged_probe_does_not_suppress_future_liveness_checks() {
        let now = Instant::now();
        let mut progress = Progress::default();
        progress.ack(now, 1200);
        progress.sent(now + Duration::from_millis(900), 1200);
        assert!(!progress.needs_probe(now + Duration::from_millis(999)));
        assert!(progress.needs_probe(now + Duration::from_secs(1)));
        assert!(progress.needs_probe(now + Duration::from_secs(2)));
    }

    #[test]
    fn ack_only_transmission_never_creates_pending_ack_debt() {
        let now = Instant::now();
        let clock: Clock = Arc::new(move || now + Duration::from_secs(3));
        let mut controller =
            factory(crate::CongestionControl::Cubic.factory(), clock).build(now, 1200);
        controller.on_sent(now, 1200, 1);
        let proof = snapshot(controller.clone_box()).unwrap();
        assert_eq!(proof.pending_age(), None);
        assert!(!proof.stalled(Duration::from_millis(70)));
        controller.on_packet_sent(now, 1200, 2);
        assert!(
            snapshot(controller.clone_box())
                .unwrap()
                .stalled(Duration::from_millis(70))
        );
        controller.on_end_acks(now, 0, true, Some(2));
        controller.on_sent(now, 100, 3);
        assert_eq!(
            snapshot(controller.clone_box()).unwrap().pending_age(),
            None
        );
    }
    #[test]
    fn only_outstanding_ack_eliciting_work_can_stall_and_idle_resumption_gets_its_own_budget() {
        let start = Instant::now();
        let mut progress = Progress::default();
        progress.sent(start, 0);
        assert!(!progress.stalled(start + Duration::from_secs(10), Duration::ZERO));
        progress.sent(start, 100);
        assert!(!progress.stalled(
            start + Duration::from_millis(499),
            Duration::from_millis(70)
        ));
        assert!(progress.stalled(
            start + Duration::from_millis(500),
            Duration::from_millis(70)
        ));
        progress.ack(start + Duration::from_millis(600), 100);
        progress.end_acks(start + Duration::from_millis(600), 0);
        assert!(progress.confirmed(
            start + Duration::from_millis(600),
            Duration::from_millis(70)
        ));
        progress.sent(start + Duration::from_secs(60), 100);
        assert!(!progress.stalled(start + Duration::from_secs(60), Duration::from_millis(70)));
    }
    #[test]
    fn fresh_ack_progress_and_migration_reset_are_explicit() {
        let start = Instant::now();
        let mut progress = Progress::default();
        progress.sent(start, 100);
        progress.ack(start + Duration::from_millis(450), 100);
        progress.end_acks(start + Duration::from_millis(450), 100);
        assert!(!progress.stalled(
            start + Duration::from_millis(900),
            Duration::from_millis(70)
        ));
        assert!(progress.stalled(
            start + Duration::from_millis(950),
            Duration::from_millis(70)
        ));
        progress = Progress::default();
        progress.sent(start + Duration::from_secs(2), 100);
        assert!(!progress.confirmed(start + Duration::from_secs(2), Duration::from_millis(70)));
        assert!(!progress.stalled(start + Duration::from_secs(2), Duration::from_millis(70)));
    }
}
