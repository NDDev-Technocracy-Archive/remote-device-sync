use super::*;
use std::io::{self, Write};
use std::sync::Mutex;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn records(&self) -> Vec<serde_json::Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| {
                assert!(line.len() < MAX_RECORD_BYTES);
                serde_json::from_str(line).unwrap()
            })
            .collect()
    }
}

#[test]
fn health_observer_is_non_owning_and_reports_sink_failure() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic sink failure"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (subscriber, telemetry) = subscriber(
        Service::Cli,
        Config::new(Format::Json, "off").unwrap(),
        Broken,
    )
    .unwrap();
    let observer = telemetry.health_observer();
    let dispatch = tracing::Dispatch::new(subscriber);
    tracing::dispatcher::with_default(&dispatch, || emit(Event::ProcessStarted));
    let result = telemetry.shutdown();
    assert!(result.drained);
    assert_eq!(observer.snapshot().unwrap().telemetry_write_errors_total, 1);
    drop(dispatch);
    assert!(observer.snapshot().is_none());
}

#[test]
fn json_is_allowlisted_without_invoking_private_formatters() {
    struct MustNotFormat;
    impl std::fmt::Debug for MustNotFormat {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("private formatting must not run");
        }
    }
    let capture = Capture::default();
    let (subscriber, telemetry) = subscriber(
        Service::Agent,
        Config::new(Format::Json, "trace").unwrap(),
        capture.clone(),
    )
    .unwrap();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "rds.conn",
            session_id = 42u64,
            peer = "SECRET_PEER",
            path = "SECRET_PATH"
        );
        let _entered = span.enter();
        tracing::error!(secret = ?MustNotFormat, token = "SECRET_TOKEN", "SECRET_MESSAGE");
        emit(Event::PeerRejected);
        tracing::info!(target: "rds_telemetry", event = "SECRET_EVENT", outcome = "SECRET_OUTCOME");
    });
    assert!(telemetry.shutdown().drained);
    let records = capture.records();
    assert_eq!(records.len(), 3, "no duplicate event from the two filters");
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["sequence"], index);
        assert_eq!(record["session_id"], 42);
        assert_eq!(record["schema_version"], 1);
        assert_eq!(record["service"], "rds-agent");
        assert_eq!(record["run_id"].as_str().unwrap().len(), 32);
        assert!(!record.to_string().contains("SECRET"));
    }
    assert_eq!(records[0]["event"], "diagnostic");
    assert_eq!(records[1]["event"], "peer_rejected");
    assert!(records[2]["event"].is_null());
}

#[tokio::test]
async fn ssh_operations_export_only_fixed_names_and_lifecycle_outcomes() {
    let capture = Capture::default();
    let (subscriber, telemetry) = subscriber(
        Service::Cli,
        Config::new(Format::Json, "off").unwrap(),
        capture.clone(),
    )
    .unwrap();
    let _default = tracing::subscriber::set_default(subscriber);
    let _: Result<(), ()> = observe(Operation::SshConnect, async { Ok(()) }).await;
    let _: Result<(), ()> = observe(Operation::GrantAuthorize, async { Ok(()) }).await;
    let _: Result<(), ()> = observe(Operation::GrantRenew, async { Err(()) }).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            observe(
                Operation::SshSession,
                std::future::pending::<Result<(), ()>>()
            )
        )
        .await
        .is_err()
    );
    assert!(telemetry.shutdown().drained);
    let records = capture.records();
    assert_eq!(records.len(), 4);
    assert_eq!(records[1]["operation"], "grant_authorize");
    assert_eq!(records[1]["outcome"], "ok");
    assert_eq!(records[2]["operation"], "grant_renew");
    assert_eq!(records[2]["outcome"], "error");
    assert_eq!(records[0]["operation"], "ssh_connect");
    assert_eq!(records[0]["outcome"], "ok");
    assert_eq!(records[3]["operation"], "ssh_session");
    assert_eq!(records[3]["outcome"], "cancelled");
}

#[test]
fn warning_diagnostics_keep_connection_context_when_info_is_filtered() {
    let capture = Capture::default();
    let (subscriber, telemetry) = subscriber(
        Service::Agent,
        Config::new(Format::Json, "warn").unwrap(),
        capture.clone(),
    )
    .unwrap();
    tracing::subscriber::with_default(subscriber, || {
        let span = conn_span(43);
        let _entered = span.enter();
        tracing::warn!(target:"fixture", "slow frame");
        tracing::info!(target:"fixture", "must stay filtered");
        emit(Event::PeerAccepted);
    });
    assert!(telemetry.shutdown().drained);
    let records = capture.records();
    assert_eq!(
        records.len(),
        2,
        "diagnostic and operational layers must not duplicate events"
    );
    assert_eq!(records[0]["session_id"], 43);
    assert_eq!(records[1]["session_id"], 43);
}

#[test]
fn operational_events_and_session_context_survive_diagnostic_filter_off() {
    let capture = Capture::default();
    let (subscriber, telemetry) = subscriber(
        Service::Agent,
        Config::new(Format::Json, "off").unwrap(),
        capture.clone(),
    )
    .unwrap();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("rds.conn", session_id = 7u64);
        let _entered = span.enter();
        tracing::warn!("should be filtered");
        emit(Event::PeerAccepted);
    });
    assert!(telemetry.shutdown().drained);
    let records = capture.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["session_id"], 7);
}

#[test]
fn oversized_debug_output_is_dropped_whole_and_later_logs_survive() {
    let capture = Capture::default();
    let (subscriber, telemetry) = subscriber(
        Service::Cli,
        Config::new(Format::Text, "info").unwrap(),
        capture.clone(),
    )
    .unwrap();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("{}", "x".repeat(MAX_RECORD_BYTES * 4));
        tracing::info!("still alive");
    });
    let health = telemetry.shutdown();
    assert!(health.drained);
    assert_eq!(health.health.telemetry_oversize_total, 1);
    let bytes = capture.0.lock().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(text.contains("still alive"));
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_and_failures_never_report_operation_success() {
    let capture = Capture::default();
    let (subscriber, telemetry) = subscriber(
        Service::Cli,
        Config::new(Format::Json, "off").unwrap(),
        capture.clone(),
    )
    .unwrap();
    let _dispatch = tracing::subscriber::set_default(subscriber);
    let error: Result<(), &str> = observe(Operation::Connect, async { Err("PRIVATE") }).await;
    assert!(error.is_err());
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            observe(Operation::Connect, std::future::pending::<Result<(), ()>>())
        )
        .await
        .is_err()
    );
    let ok: Result<(), ()> = telemetry.run(async { Ok(()) }).await;
    assert!(ok.is_ok());
    let fail: Result<(), &str> = telemetry.run(async { Err("PRIVATE") }).await;
    assert!(fail.is_err());
    assert!(telemetry.shutdown().drained);
    let records = capture.records();
    assert_eq!(records[0]["outcome"], "error");
    assert_eq!(records[1]["outcome"], "cancelled");
    assert_eq!(records[2]["event"], "process_started");
    assert_eq!(records[3]["event"], "process_completed");
    assert_eq!(records[4]["event"], "process_started");
    assert_eq!(records[5]["event"], "process_failed");
    assert!(!serde_json::to_string(&records).unwrap().contains("PRIVATE"));
}

#[test]
fn config_rejects_invalid_filters_and_process_ids_do_not_repeat() {
    assert!(matches!(
        Config::new(Format::Json, "some_target=invalid"),
        Err(InitError::Filter)
    ));
    let mut ids = Vec::new();
    for _ in 0..2 {
        let capture = Capture::default();
        let (subscriber, telemetry) = subscriber(
            Service::Bench,
            Config::new(Format::Json, "warn").unwrap(),
            capture.clone(),
        )
        .unwrap();
        tracing::subscriber::with_default(subscriber, || emit(Event::Heartbeat));
        assert!(telemetry.shutdown().drained);
        ids.push(capture.records()[0]["run_id"].clone());
    }
    assert_ne!(ids[0], ids[1]);
}

#[tokio::test(start_paused = true)]
async fn heartbeat_continues_while_the_product_future_is_pending() {
    let capture = Capture::default();
    let (subscriber, telemetry) = subscriber(
        Service::Server,
        Config::new(Format::Json, "off").unwrap(),
        capture.clone(),
    )
    .unwrap();
    let _dispatch = tracing::subscriber::set_default(subscriber);
    let result: Result<(), ()> = telemetry
        .run(async {
            tokio::time::sleep(Duration::from_secs(46)).await;
            Ok(())
        })
        .await;
    assert!(result.is_ok());
    assert!(telemetry.shutdown().drained);
    let records = capture.records();
    assert_eq!(
        records.iter().filter(|r| r["event"] == "heartbeat").count(),
        3
    );
    assert_eq!(records.last().unwrap()["event"], "process_completed");
}

#[test]
fn terminal_failure_cannot_reintroduce_private_error_text_or_a_source_chain() {
    for format in [Format::Json, Format::Text] {
        let capture = Capture::default();
        let (subscriber, telemetry) = subscriber(
            Service::Cli,
            Config::new(format, "off").unwrap(),
            capture.clone(),
        )
        .unwrap();
        let status = tracing::subscriber::with_default(subscriber, || {
            telemetry.finish(Err::<(), _>("PRIVATE_SENTINEL\n{\"schema_version\":1}"))
        });
        assert_eq!(status, std::process::ExitCode::FAILURE);
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert_eq!(output.contains("PRIVATE_SENTINEL"), format == Format::Text);
        if format == Format::Json {
            assert!(output.is_empty());
        }
    }
}

#[tokio::test]
async fn sync_operations_export_only_fixed_names_and_lifecycle_outcomes() {
    let capture = Capture::default();
    let (subscriber, telemetry) = subscriber(
        Service::Cli,
        Config::new(Format::Json, "off").unwrap(),
        capture.clone(),
    )
    .unwrap();
    let _default = tracing::subscriber::set_default(subscriber);
    let _: Result<(), ()> = observe(Operation::SyncSend, async { Ok(()) }).await;
    let _: Result<(), ()> = observe(Operation::SyncRecv, async { Ok(()) }).await;
    let _: Result<(), ()> = observe(Operation::SyncSend, async { Err(()) }).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            observe(
                Operation::SyncRecv,
                std::future::pending::<Result<(), ()>>()
            )
        )
        .await
        .is_err()
    );
    assert!(telemetry.shutdown().drained);
    let records = capture.records();
    assert_eq!(records.len(), 4);
    assert_eq!(records[1]["operation"], "sync_recv");
    assert_eq!(records[1]["outcome"], "ok");
    assert_eq!(records[2]["operation"], "sync_send");
    assert_eq!(records[2]["outcome"], "error");
    assert_eq!(records[0]["operation"], "sync_send");
    assert_eq!(records[0]["outcome"], "ok");
    assert_eq!(records[3]["operation"], "sync_recv");
    assert_eq!(records[3]["outcome"], "cancelled");
}

#[test]
fn session_guard_emits_reasoned_open_and_close_correlated_by_session_id() {
    let capture = Capture::default();
    let (subscriber, telemetry) = subscriber(
        Service::Agent,
        Config::new(Format::Json, "trace").unwrap(),
        capture.clone(),
    )
    .unwrap();
    tracing::subscriber::with_default(subscriber, || {
        let session = SessionGuard::open(conn_span(41));
        session.span().in_scope(|| emit(Event::PathMigrated));
        session.close(Reason::Timeout);
    });
    assert!(telemetry.shutdown().drained);
    let records = capture.records();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["event"], "session_opened");
    assert_eq!(records[0]["session_id"], 41);
    assert_eq!(records[1]["event"], "path_migrated");
    assert_eq!(records[1]["session_id"], 41);
    assert_eq!(records[2]["event"], "session_closed");
    assert_eq!(records[2]["reason"], "timeout");
    assert_eq!(records[2]["session_id"], 41);
    assert!(records[2]["elapsed_us"].is_u64());
    for record in &records {
        assert!(record.get("peer").is_none(), "peer never leaves the span");
    }
}

#[test]
fn session_guard_drop_without_reason_reports_aborted() {
    let capture = Capture::default();
    let (subscriber, telemetry) = subscriber(
        Service::Cli,
        Config::new(Format::Json, "trace").unwrap(),
        capture.clone(),
    )
    .unwrap();
    tracing::subscriber::with_default(subscriber, || {
        drop(SessionGuard::open(conn_span(9)));
    });
    assert!(telemetry.shutdown().drained);
    let records = capture.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["event"], "session_opened");
    assert_eq!(records[1]["event"], "session_closed");
    assert_eq!(records[1]["reason"], "aborted");
    assert_eq!(records[1]["session_id"], 9);
}

#[test]
fn session_ids_mint_monotonic() {
    let first = next_session_id();
    assert!(next_session_id() > first);
}

#[test]
fn handshake_failure_exports_its_reason_and_shows_its_code_only_in_text() {
    for format in [Format::Json, Format::Text] {
        let capture = Capture::default();
        let (subscriber, telemetry) = subscriber(
            Service::Agent,
            Config::new(format, "info").unwrap(),
            capture.clone(),
        )
        .unwrap();
        tracing::subscriber::with_default(subscriber, || {
            handshake_failed(Reason::Protocol, Some(0x178));
            handshake_failed(Reason::Timeout, None);
        });
        assert!(telemetry.shutdown().drained);
        if format == Format::Json {
            let records = capture.records();
            assert_eq!(records.len(), 2);
            for (record, reason) in records.iter().zip(["protocol", "timeout"]) {
                assert_eq!(record["event"], "handshake_failed");
                assert_eq!(record["reason"], reason);
                assert!(record.get("code").is_none());
            }
        } else {
            let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
            assert!(output.contains("reason=\"protocol\" code=376"), "{output}");
            assert!(output.contains("reason=\"timeout\""), "{output}");
        }
    }
}
