//! Bounded metadata flight recorder for slow input and stalled video.
use rds_desktop::render::ViewerSnapshot;
use std::collections::VecDeque;

const HISTORY: usize = 30; // Two-second snapshots: one minute before a fault.
const AFTER_MS: u64 = 10_000;
const COOLDOWN_MS: u64 = 30_000;

struct Incident {
    triggered_ms: u64,
    reasons: Vec<&'static str>,
    snapshots: Vec<serde_json::Value>,
}

#[derive(Default)]
pub(super) struct Recorder {
    history: VecDeque<serde_json::Value>,
    pending: Option<Incident>,
    previous_reconnects: Option<u64>,
    previous_slow_acks: Option<u64>,
    previous_slow_clipboards: Option<u64>,
    previous_repairs: Option<u64>,
    previous_log_errors: u64,
    previous_storage_errors: u64,
    cooldown_until_ms: u64,
}

impl Recorder {
    #[cfg(test)]
    fn observe(&mut self, snapshot: &ViewerSnapshot) -> Option<Vec<u8>> {
        self.observe_with_health(
            snapshot,
            None,
            &super::diagnostic_storage::Health::default(),
        )
        .1
    }

    pub(super) fn observe_with_health(
        &mut self,
        snapshot: &ViewerSnapshot,
        telemetry: Option<rds_observe::Health>,
        storage: &super::diagnostic_storage::Health,
    ) -> (serde_json::Value, Option<Vec<u8>>) {
        let mut value = serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null);
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "diagnostics".into(),
                serde_json::json!({"telemetry":telemetry,"storage":storage}),
            );
        }
        let incident = self.observe_value(snapshot, value.clone(), telemetry, storage);
        (value, incident)
    }

    fn observe_value(
        &mut self,
        snapshot: &ViewerSnapshot,
        value: serde_json::Value,
        telemetry: Option<rds_observe::Health>,
        storage: &super::diagnostic_storage::Health,
    ) -> Option<Vec<u8>> {
        self.history.push_back(value.clone());
        if self.history.len() > HISTORY {
            self.history.pop_front();
        }
        let reconnected = self
            .previous_reconnects
            .is_some_and(|previous| snapshot.report.reconnects > previous);
        self.previous_reconnects = Some(snapshot.report.reconnects);
        let slow_ack = self
            .previous_slow_acks
            .is_some_and(|previous| snapshot.report.slow_input_acks > previous);
        self.previous_slow_acks = Some(snapshot.report.slow_input_acks);
        let slow_clipboard = self
            .previous_slow_clipboards
            .is_some_and(|previous| snapshot.report.slow_clipboard_transfers > previous);
        self.previous_slow_clipboards = Some(snapshot.report.slow_clipboard_transfers);
        let repaired = self
            .previous_repairs
            .is_some_and(|n| snapshot.report.video_repair_requests > n);
        self.previous_repairs = Some(snapshot.report.video_repair_requests);
        let log_errors = telemetry.map(|h| {
            h.telemetry_dropped_total
                .saturating_add(h.telemetry_oversize_total)
                .saturating_add(h.telemetry_write_errors_total)
        });
        let lost_logs = log_errors.is_some_and(|n| n > self.previous_log_errors);
        if let Some(n) = log_errors {
            self.previous_log_errors = n;
        }
        let storage_errors = storage
            .snapshot_write_errors_total
            .saturating_add(storage.incident_write_errors_total)
            .saturating_add(storage.incident_queue_evicted_total)
            .saturating_add(storage.incident_oversize_total);
        let storage_errors = storage_errors
            .saturating_add(storage.live_query_errors_total)
            .saturating_add(storage.incident_admission_dropped_total)
            .saturating_add(storage.snapshot_admission_dropped_total);
        let failed_storage = storage_errors > self.previous_storage_errors;
        self.previous_storage_errors = storage_errors;
        let mut reasons = Vec::new();
        if slow_ack {
            // Completed stalls can fall entirely between periodic snapshots.
            reasons.push("input_ack_delayed");
        }
        if slow_clipboard {
            reasons.push("clipboard_ready_delayed");
        }
        if snapshot
            .report
            .oldest_input_ack_age_ms
            .is_some_and(|ms| ms >= 250)
        {
            reasons.push("input_ack_pending");
        }
        if snapshot
            .report
            .clipboard_oldest_pending_age_ms
            .is_some_and(|ms| ms >= 250)
        {
            reasons.push("clipboard_ready_pending");
        }
        if snapshot.decoded_frame_age_ms.is_some_and(|ms| ms >= 3000) {
            reasons.push("decoded_video_stalled");
        }
        if snapshot.status == "Connected"
            && snapshot.control_echo_age_ms.is_some_and(|ms| ms >= 3000)
        {
            reasons.push("control_echo_stalled");
        }
        // Occlusion is a normal OS policy, not a frozen visible screen.
        if !snapshot.occluded
            && snapshot.render_stage != "surface occluded"
            && snapshot.status == "Connected"
            && snapshot
                .unpresented_frame_age_ms
                .is_some_and(|ms| ms >= 1000)
        {
            reasons.push("visible_submission_stalled");
        }
        if reconnected {
            reasons.push("desktop_reconnected");
        }
        if repaired {
            reasons.push("video_repair_requested");
        }
        if lost_logs {
            reasons.push("telemetry_records_lost");
        }
        if failed_storage {
            reasons.push("diagnostic_storage_failed");
        }
        if let Some(incident) = &mut self.pending {
            // Later control loss/recovery must not disappear behind the first
            // symptom. The reason vocabulary and snapshot count remain bounded.
            for reason in reasons {
                if !incident.reasons.contains(&reason) {
                    incident.reasons.push(reason);
                }
            }
            if incident.snapshots.len() < HISTORY + 6 {
                incident.snapshots.push(value);
            }
            if snapshot.elapsed_ms.saturating_sub(incident.triggered_ms) >= AFTER_MS {
                self.cooldown_until_ms = snapshot.elapsed_ms.saturating_add(COOLDOWN_MS);
                return self.finish(false);
            }
            return None;
        }
        // A new recovery or evidence-loss event is distinct from repeated age
        // samples and must survive the ordinary symptom cooldown.
        if snapshot.elapsed_ms < self.cooldown_until_ms
            && !(reconnected || repaired || lost_logs || failed_storage)
        {
            return None;
        }
        if !reasons.is_empty() {
            tracing::warn!(
                ?reasons,
                elapsed_ms = snapshot.elapsed_ms,
                "desktop incident recording started"
            );
            self.pending = Some(Incident {
                triggered_ms: snapshot.elapsed_ms,
                reasons,
                snapshots: self.history.iter().cloned().collect(),
            });
        }
        None
    }

    /// Preserve an in-progress fault window when the viewer shuts down.
    pub(super) fn finish(&mut self, interrupted: bool) -> Option<Vec<u8>> {
        let incident = self.pending.take()?;
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "triggered_elapsed_ms": incident.triggered_ms,
            "reasons": incident.reasons,
            "interrupted": interrupted,
            "snapshots": incident.snapshots,
        }))
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rds_desktop::render::ViewerReport;
    fn snapshot(ms: u64) -> ViewerSnapshot {
        ViewerSnapshot {
            pid: 42,
            elapsed_ms: ms,
            status: "Connected".into(),
            network_stage: "receiving".into(),
            render_stage: "presented".into(),
            render_stage_age_ms: None,
            ui_event_age_ms: 0,
            decoded_frame_age_ms: Some(10),
            encoded_frame_age_ms: Some(10),
            control_echo_age_ms: Some(10),
            submission_age_ms: Some(10),
            unpresented_frame_age_ms: None,
            pending_frame_bytes: 0,
            occluded: false,
            window_visibility: Default::default(),
            native_window: None,
            native_window_sample_age_ms: None,
            report: ViewerReport::default(),
        }
    }
    #[test]
    fn slow_input_preserves_bounded_before_and_after_history_and_shutdown_fault() {
        let mut recorder = Recorder::default();
        for ms in (0..80_000).step_by(2000) {
            assert!(recorder.observe(&snapshot(ms)).is_none());
        }
        let mut slow = snapshot(80_000);
        slow.report.oldest_input_ack_age_ms = Some(500);
        assert!(recorder.observe(&slow).is_none());
        let mut result = None;
        for ms in (82_000..=90_000).step_by(2000) {
            result = recorder.observe(&snapshot(ms));
        }
        let data: serde_json::Value = serde_json::from_slice(&result.unwrap()).unwrap();
        let rows = data["snapshots"].as_array().unwrap();
        assert_eq!(rows.len(), 35);
        assert_eq!(rows.first().unwrap()["elapsed_ms"], 22_000);
        assert_eq!(rows.last().unwrap()["elapsed_ms"], 90_000);
        assert_eq!(data["reasons"][0], "input_ack_pending");
        slow.elapsed_ms = 92_000;
        assert!(recorder.observe(&slow).is_none());
        assert!(recorder.finish(true).is_none());
        slow.elapsed_ms = 122_000;
        recorder.observe(&slow);
        let data: serde_json::Value =
            serde_json::from_slice(&recorder.finish(true).unwrap()).unwrap();
        assert_eq!(data["interrupted"], true);
    }
    #[test]
    fn normal_occlusion_is_not_a_fault_but_video_loss_and_reconnect_are() {
        let mut recorder = Recorder::default();
        let mut hidden = snapshot(0);
        hidden.occluded = true;
        hidden.unpresented_frame_age_ms = Some(90_000);
        hidden.submission_age_ms = Some(90_000);
        recorder.observe(&hidden);
        assert!(recorder.finish(true).is_none());
        hidden.occluded = false; // Metal can report occlusion before the window event.
        hidden.render_stage = "surface occluded".into();
        recorder.observe(&hidden);
        assert!(recorder.finish(true).is_none());
        hidden.occluded = true;
        hidden.elapsed_ms = 2000;
        hidden.decoded_frame_age_ms = Some(4000);
        hidden.report.reconnects = 1;
        recorder.observe(&hidden);
        let data: serde_json::Value =
            serde_json::from_slice(&recorder.finish(true).unwrap()).unwrap();
        assert_eq!(
            data["reasons"],
            serde_json::json!(["decoded_video_stalled", "desktop_reconnected"])
        );
    }
    #[test]
    fn a_completed_slow_ack_between_snapshots_still_records_an_incident() {
        let mut recorder = Recorder::default();
        recorder.observe(&snapshot(0));
        let mut completed = snapshot(2000);
        completed.report.slow_input_acks = 2;
        assert_eq!(completed.report.pending_input_acks, 0);
        recorder.observe(&completed);
        let data: serde_json::Value =
            serde_json::from_slice(&recorder.finish(true).unwrap()).unwrap();
        assert_eq!(data["reasons"][0], "input_ack_delayed");
    }

    #[test]
    fn pending_and_completed_slow_clipboard_transfers_preserve_an_incident() {
        for completed in [false, true] {
            let mut recorder = Recorder::default();
            recorder.observe(&snapshot(0));
            let mut delayed = snapshot(2000);
            let reason = if completed {
                delayed.report.slow_clipboard_transfers = 1;
                delayed.report.last_clipboard_transfer_ms = Some(600.);
                "clipboard_ready_delayed"
            } else {
                delayed.report.clipboard_pending_transfers = 1;
                delayed.report.clipboard_oldest_pending_age_ms = Some(600);
                "clipboard_ready_pending"
            };
            recorder.observe(&delayed);
            let data: serde_json::Value =
                serde_json::from_slice(&recorder.finish(true).unwrap()).unwrap();
            assert_eq!(data["reasons"], serde_json::json!([reason]));
        }
    }

    #[test]
    fn idle_and_a_fresh_update_do_not_report_a_visible_renderer_stall() {
        let mut recorder = Recorder::default();
        let mut idle = snapshot(90_000);
        idle.submission_age_ms = Some(90_000);
        idle.decoded_frame_age_ms = Some(1050); // Damage-driven idle refresh cadence.
        recorder.observe(&idle);
        assert!(recorder.finish(true).is_none());
        idle.elapsed_ms += 2000;
        idle.unpresented_frame_age_ms = Some(1);
        idle.pending_frame_bytes = 4096;
        recorder.observe(&idle);
        assert!(recorder.finish(true).is_none());
        idle.elapsed_ms += 2000;
        idle.decoded_frame_age_ms = Some(0); // Fresh replacements must not hide debt.
        idle.unpresented_frame_age_ms = Some(2001);
        recorder.observe(&idle);
        let data: serde_json::Value =
            serde_json::from_slice(&recorder.finish(true).unwrap()).unwrap();
        assert_eq!(
            data["reasons"],
            serde_json::json!(["visible_submission_stalled"])
        );
    }

    #[test]
    fn later_control_loss_and_recovery_survive_an_open_window_and_cooldown() {
        let mut recorder = Recorder::default();
        recorder.observe(&snapshot(0));
        let mut s = snapshot(2000);
        s.decoded_frame_age_ms = Some(4000);
        recorder.observe(&s);
        s.elapsed_ms = 4000;
        s.control_echo_age_ms = Some(5000);
        s.report.video_repair_requests = 1;
        recorder.observe(&s);
        s.elapsed_ms = 12_000;
        s.report.reconnects = 1;
        let data: serde_json::Value =
            serde_json::from_slice(&recorder.observe(&s).unwrap()).unwrap();
        assert_eq!(
            data["reasons"],
            serde_json::json!([
                "decoded_video_stalled",
                "control_echo_stalled",
                "video_repair_requested",
                "desktop_reconnected"
            ])
        );
        s.elapsed_ms = 14_000;
        s.report.reconnects = 2;
        recorder.observe(&s); // Still in ordinary age-symptom cooldown.
        let data: serde_json::Value =
            serde_json::from_slice(&recorder.finish(true).unwrap()).unwrap();
        assert!(
            data["reasons"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("desktop_reconnected"))
        );
    }

    #[test]
    fn evidence_loss_is_recorded_independently_of_the_log_sink() {
        let mut recorder = Recorder::default();
        let storage = super::super::diagnostic_storage::Health::default();
        recorder.observe_with_health(&snapshot(0), Some(rds_observe::Health::default()), &storage);
        let telemetry = rds_observe::Health {
            telemetry_write_errors_total: 1,
            telemetry_dropped_total: 2,
            ..Default::default()
        };
        let failed_storage = super::super::diagnostic_storage::Health {
            incident_write_errors_total: 1,
            incident_queue_pending: 1,
            ..Default::default()
        };
        let (value, _) =
            recorder.observe_with_health(&snapshot(2000), Some(telemetry), &failed_storage);
        assert_eq!(
            value["diagnostics"]["telemetry"]["telemetry_dropped_total"],
            2
        );
        assert_eq!(value["diagnostics"]["storage"]["incident_queue_pending"], 1);
        let data: serde_json::Value =
            serde_json::from_slice(&recorder.finish(true).unwrap()).unwrap();
        assert_eq!(
            data["reasons"],
            serde_json::json!(["telemetry_records_lost", "diagnostic_storage_failed"])
        );
        let (value, _) = recorder.observe_with_health(&snapshot(4000), None, &storage);
        assert!(
            value["diagnostics"]["telemetry"].is_null(),
            "unavailable counters must not pretend to be zero"
        );
    }
}
