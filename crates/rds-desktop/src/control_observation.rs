//! Bounded local transmit observations, outside native input admission.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rds_net::{PathStats, WeakPathObserver};
use tracing::Instrument;

use crate::{DesktopError, mailbox};

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);
const MAX_PATHS: usize = 8;
const SLOW_RESPONSE: Duration = Duration::from_millis(250);

pub(crate) struct ControlObservation {
    pub instance: u64,
    observer: WeakPathObserver,
    previous: Vec<PathStats>,
    sampled: Option<Instant>,
}

struct SampleRequest {
    seq: u64,
    rtt: Duration,
    received: Instant,
}

pub(crate) struct ObservationWorker {
    tx: mailbox::Sender<SampleRequest>,
    evicted: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl ObservationWorker {
    pub fn heartbeat(&self, seq: u64, rtt: Duration) {
        if self
            .tx
            .send(SampleRequest {
                seq,
                rtt,
                received: Instant::now(),
            })
            .is_some()
        {
            self.evicted.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for ObservationWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ControlObservation {
    pub fn new(observer: WeakPathObserver) -> Result<Self, DesktopError> {
        let instance = NEXT_INSTANCE
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| DesktopError::Capture("desktop observation IDs exhausted".into()))?;
        Ok(Self {
            instance,
            observer,
            previous: Vec::new(),
            sampled: None,
        })
    }

    pub fn spawn(self) -> ObservationWorker {
        let (tx, mut rx) = mailbox::channel::<SampleRequest>(1);
        let evicted = Arc::new(AtomicU64::new(0));
        let drops = evicted.clone();
        let instance = self.instance;
        let task = tokio::spawn(
            async move {
                let mut observation = self;
                while let Some(request) = rx.recv().await {
                    let evictions = drops.load(Ordering::Relaxed);
                    let span = tracing::Span::current();
                    // Transport metadata may take backend locks. Never query it on
                    // the control reader or an input/receipt writer. One in-flight
                    // query and one newest queued request bound work and memory.
                    let result = tokio::task::spawn_blocking(move || {
                        let _entered = span.enter();
                        observation.sample(request, evictions);
                        observation
                    })
                    .await;
                    match result {
                        Ok(next) => observation = next,
                        Err(_) => {
                            tracing::warn!(target:"rds_desktop::control_timing",
                            control_instance=instance,
                            "desktop path observation worker ended; control continues");
                            break;
                        }
                    }
                }
            }
            .in_current_span(),
        );
        ObservationWorker { tx, evicted, task }
    }

    fn sample(&mut self, request: SampleRequest, evictions: u64) {
        let SampleRequest { seq, rtt, received } = request;
        let now = Instant::now();
        let mut snapshot = self.observer.snapshot();
        let observed = snapshot.paths.len();
        // Prefer current selected paths when the backend has more than the
        // diagnostic budget. Truncation must remain explicit.
        snapshot.paths.sort_by_key(|path| !path.selected);
        snapshot.paths.truncate(MAX_PATHS);
        if selection_changed(&snapshot.paths, &self.previous) {
            tracing::info!(target:"rds_desktop::control_timing", control_instance=self.instance,
                heartbeat_seq=seq, coverage=?snapshot.coverage, observed_paths=observed,
                paths_truncated=observed>MAX_PATHS,
                "desktop selected transmit paths changed");
            for path in snapshot.paths.iter().filter(|path| path.selected) {
                tracing::info!(target:"rds_desktop::control_timing", control_instance=self.instance,
                    heartbeat_seq=seq, path_id=path.path_id, via_relay=path.via_relay,
                    path_rtt_ms=path.rtt.as_millis(),
                    "desktop selected transmit path");
            }
        }
        if rtt >= SLOW_RESPONSE {
            tracing::warn!(target:"rds_desktop::control_timing", control_instance=self.instance,
                heartbeat_seq=seq, rtt_ms=rtt.as_millis(),
                sample_interval_ms=self.sampled.map(|t| now.duration_since(t).as_millis()),
                observation_age_ms=received.elapsed().as_millis(),
                observation_work_ms=now.elapsed().as_millis(), diagnostic_evictions_total=evictions,
                observed_paths=observed, recorded_paths=snapshot.paths.len(),
                paths_truncated=observed>MAX_PATHS, coverage=?snapshot.coverage,
                "desktop control response delayed; local transmit observations follow");
            for path in &snapshot.paths {
                let previous = self.previous.iter().find(|p| p.path_id == path.path_id);
                let delta = deltas(path, previous);
                tracing::warn!(target:"rds_desktop::control_timing", control_instance=self.instance,
                    heartbeat_seq=seq, path_id=path.path_id, selected=path.selected,
                    via_relay=path.via_relay, path_rtt_ms=path.rtt.as_millis(), cwnd_bytes=path.cwnd,
                    tx_datagrams=path.sent, tx_lost=path.lost, tx_bytes=path.sent_bytes,
                    rx_bytes=path.recv_bytes, congestion_events=path.congestion_events,
                    tx_datagrams_delta=delta.sent, tx_lost_delta=delta.lost,
                    tx_bytes_delta=delta.bytes, congestion_events_delta=delta.congestion,
                    "desktop local transmit path at delayed control response");
            }
        }
        // Fast responses are also baselines, so a later slow response does not
        // misattribute all session history to one interval.
        self.previous = snapshot.paths;
        self.sampled = Some(now);
    }
}

fn selection_changed(current: &[PathStats], previous: &[PathStats]) -> bool {
    fn same(path: &PathStats, candidates: &[PathStats]) -> bool {
        candidates.iter().any(|other| {
            other.selected && path.path_id == other.path_id && path.via_relay == other.via_relay
        })
    }
    current
        .iter()
        .filter(|path| path.selected)
        .any(|path| !same(path, previous))
        || previous
            .iter()
            .filter(|path| path.selected)
            .any(|path| !same(path, current))
}

struct Deltas {
    sent: Option<u64>,
    lost: Option<u64>,
    bytes: Option<u64>,
    congestion: Option<u64>,
}

fn deltas(current: &PathStats, previous: Option<&PathStats>) -> Deltas {
    Deltas {
        sent: previous.and_then(|p| current.sent.checked_sub(p.sent)),
        lost: previous.and_then(|p| current.lost.checked_sub(p.lost)),
        bytes: previous.and_then(|p| current.sent_bytes.checked_sub(p.sent_bytes)),
        congestion: previous
            .and_then(|p| current.congestion_events.checked_sub(p.congestion_events)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(sent: u64, lost: u64) -> PathStats {
        PathStats {
            path_id: 1,
            rtt: Duration::ZERO,
            cwnd: 1,
            sent,
            lost,
            sent_bytes: sent * 10,
            recv_bytes: 0,
            congestion_events: lost,
            selected: true,
            via_relay: false,
            relay_slot: None,
        }
    }

    #[test]
    fn selection_observation_tracks_route_changes_without_rtt_chatter() {
        let direct = path(5, 0);
        let mut relay = path(10, 0);
        relay.path_id = 2;
        relay.via_relay = true;
        let mut newer_direct = path(20, 1);
        newer_direct.rtt = Duration::from_millis(150);
        assert!(selection_changed(std::slice::from_ref(&direct), &[]));
        assert!(!selection_changed(
            &[newer_direct],
            std::slice::from_ref(&direct)
        ));
        assert!(selection_changed(
            std::slice::from_ref(&relay),
            std::slice::from_ref(&direct)
        ));
        assert!(selection_changed(&[], std::slice::from_ref(&direct)));
        assert!(!selection_changed(&[], &[]));
        assert!(!selection_changed(&[direct, relay], &[relay, direct]));
    }

    #[test]
    fn unobserved_or_reset_counters_are_unknown_not_zero_loss() {
        let current = path(5, 2);
        let initial = deltas(&current, None);
        assert_eq!(initial.sent, None);
        assert_eq!(initial.lost, None);
        let reset = deltas(&current, Some(&path(9, 3)));
        assert_eq!(reset.sent, None);
        assert_eq!(reset.lost, None);
        assert_eq!(reset.bytes, None);
        assert_eq!(reset.congestion, None);
        let measured = deltas(&path(9, 3), Some(&current));
        assert_eq!(measured.sent, Some(4));
        assert_eq!(measured.lost, Some(1));
        assert_eq!(measured.bytes, Some(40));
        assert_eq!(measured.congestion, Some(1));
    }
}
