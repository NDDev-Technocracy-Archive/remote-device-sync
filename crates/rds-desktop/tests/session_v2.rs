//! WS5 session/media protocol v2 — in-process end-to-end over real QUIC.
//!
//! Two `rds_net` endpoints, one running `serve_desktop_with` against a
//! `SyntheticProducer`, the other a `DesktopSession`. A shared
//! `SessionClock` makes `FrameHeader` timestamps directly comparable to
//! client receive times. This measures complete header arrival, not decoded
//! pixels or viewer presentation.
//!
//! The impairment lane runs on the owned transport (`noq`) with
//! `impair::ImpairingSocket` wrapping each endpoint's UDP socket:
//! impairment is applied underneath QUIC, so every datagram is delayed,
//! jittered or dropped no matter which remote address the connection
//! selects — an in-line proxy would just be migrated around.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rds_bench::impair::{ImpairingSocket, Impairment, StatsHandle};
use rds_core::{Codec, DesktopEvent, DesktopHello, HelloAck, StreamHello};
use rds_desktop::client::{DesktopSession, SessionOpts};
use rds_desktop::{SessionClock, SessionConfig, SyntheticProducer, serve_desktop_with};
use rds_net::read_frame;
use rds_net::{Endpoint, EndpointAddr, EndpointConfig, bind_noq_with_socket};

/// Deterministic unique session IDs for the tests — uniqueness within a
/// connection is what the route isolates, not entropy.
fn next_session_id() -> [u8; 16] {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(
        &NEXT
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_be_bytes(),
    );
    id[8] = 0xD5;
    id
}

#[derive(Clone, Default)]
struct TestInput {
    view_only: bool,
    fail: bool,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl rds_desktop::InputSink for TestInput {
    fn inject(&mut self, _: &rds_core::InputEvent) -> Result<(), rds_desktop::DesktopError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail {
            Err(rds_desktop::DesktopError::Input(
                "synthetic injection failure".into(),
            ))
        } else {
            Ok(())
        }
    }
}

/// One endpoint pair with a synthetic desktop session live.
struct Harness {
    session: DesktopSession,
    clock: SessionClock,
    _server_ep: Endpoint,
    _client_ep: Endpoint,
    server_task: tokio::task::JoinHandle<()>,
    /// Impairment counters (server-out, client-out) when impaired.
    impair: Option<(StatsHandle, StatsHandle)>,
}

/// A noq endpoint whose UDP socket is wrapped in `imp` impairment.
async fn impaired_endpoint(imp: Impairment) -> (Endpoint, StatsHandle) {
    let runtime: Arc<dyn noq::Runtime> = Arc::new(noq::TokioRuntime);
    let std_sock = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    let inner = noq::Runtime::wrap_udp_socket(&*runtime, std_sock).unwrap();
    let local = inner.local_addr().unwrap();
    let (socket, stats) = ImpairingSocket::wrap(inner, imp);
    let ep = bind_noq_with_socket(
        EndpointConfig::default(),
        Box::new(socket),
        vec![local],
        runtime,
    )
    .await
    .unwrap();
    (ep, stats)
}

/// Bind a pair of endpoints and return the address the client dials.
/// `impair` switches both endpoints to the noq backend with socket-level
/// impairment — applied underneath QUIC, immune to path migration.
async fn endpoints(
    impair: Option<Impairment>,
) -> (
    Endpoint,
    Endpoint,
    Option<(StatsHandle, StatsHandle)>,
    EndpointAddr,
) {
    match impair {
        Some(cfg) => {
            let (server_ep, s_stats) = impaired_endpoint(cfg).await;
            let (client_ep, c_stats) = impaired_endpoint(cfg).await;
            let target = server_ep.addr();
            (server_ep, client_ep, Some((s_stats, c_stats)), target)
        }
        None => {
            // G5's clean in-process lane must not use public discovery or
            // bootstrap through an external relay while local addresses settle.
            let config = EndpointConfig {
                bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
                discovery: false,
                ..Default::default()
            };
            let server_ep = rds_net::bind_endpoint(config.clone()).await.unwrap();
            let client_ep = rds_net::bind_endpoint(config).await.unwrap();
            let target = server_ep.addr();
            (server_ep, client_ep, None, target)
        }
    }
}

/// Serve side: accept one connection, answer the Desktop hello, run
/// `serve_desktop_with` on a synthetic producer.
async fn spawn_serving(
    server_ep: Endpoint,
    fps: u32,
    frame_bytes: usize,
    keyframe_every: u64,
    clock: SessionClock,
    input: TestInput,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let conn = server_ep.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let greeting = read_frame::<_, StreamHello>(&mut recv).await.unwrap();
        let frame_route = match &greeting {
            StreamHello::DesktopV2 { session, .. } => {
                Some(rds_core::UniHello::DesktopFrames { id: *session })
            }
            _ => None,
        };
        match greeting {
            StreamHello::Desktop(hello) | StreamHello::DesktopV2 { hello, .. } => {
                rds_net::write_frame(
                    &mut send,
                    &HelloAck::Desktop(rds_core::DesktopCaps {
                        displays: vec![],
                        codecs: vec![Codec::H264],
                    }),
                )
                .await
                .unwrap();
                serve_desktop_with(
                    conn,
                    send,
                    recv,
                    hello,
                    SessionConfig {
                        view_only: input.view_only,
                        input_sink: Some(Box::new(input)),
                        producer: Some(Box::new(
                            SyntheticProducer::new(fps, 640, 480, frame_bytes)
                                .keyframe_every(keyframe_every),
                        )),
                        clock: Some(clock.clone()),
                        frame_route,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
            other => panic!("unexpected hello {other:?}"),
        }
    })
}

async fn harness(
    fps: u32,
    frame_bytes: usize,
    keyframe_every: u64,
    impair: Option<Impairment>,
    input_acks: bool,
) -> Harness {
    harness_with_input(
        fps,
        frame_bytes,
        keyframe_every,
        impair,
        input_acks,
        TestInput::default(),
    )
    .await
}

async fn harness_with_input(
    fps: u32,
    frame_bytes: usize,
    keyframe_every: u64,
    impair: Option<Impairment>,
    input_acks: bool,
    input: TestInput,
) -> Harness {
    let clock = SessionClock::default();
    let (server_ep, client_ep, impair_stats, target) = endpoints(impair).await;
    let server_task = spawn_serving(
        server_ep.clone(),
        fps,
        frame_bytes,
        keyframe_every,
        clock.clone(),
        input,
    )
    .await;
    let conn = client_ep.connect(target, rds_core::ALPN).await.unwrap();
    let session = DesktopSession::connect_opts(
        &conn,
        DesktopHello {
            display: 0,
            max_fps: fps,
            codec: Codec::H264,
            input_acks,
        },
        SessionOpts {
            clock: Some(clock.clone()),
            session: Some(next_session_id()),
        },
    )
    .await
    .unwrap();
    Harness {
        session,
        clock,
        _server_ep: server_ep,
        _client_ep: client_ep,
        server_task,
        impair: impair_stats,
    }
}

fn percentile(mut v: Vec<u64>, p: usize) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[(v.len() * p / 100).min(v.len() - 1)]
}

fn p50(v: Vec<u64>) -> u64 {
    percentile(v, 50)
}

fn p95(v: Vec<u64>) -> u64 {
    percentile(v, 95)
}

fn p99(v: Vec<u64>) -> u64 {
    percentile(v, 99)
}

/// C5/G5: keyframe request round-trips — the next produced frame after
/// `request_idr` must carry `keyframe = true`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keyframe_request_roundtrip() {
    let mut h = harness(60, 2048, 10_000, None, false).await;

    // The session-open frame is already a keyframe — request only
    // after a steady-state non-keyframe has landed, so the flip the
    // request causes is unambiguous.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_key_after_req = false;
    while Instant::now() < deadline {
        let Some(header) = h.session.frame_headers.recv().await else {
            break;
        };
        if header.keyframe {
            continue;
        }
        h.session.request_idr().await.unwrap();
        while Instant::now() < deadline {
            let Some(header) = h.session.frame_headers.recv().await else {
                break;
            };
            if header.keyframe {
                saw_key_after_req = true;
                break;
            }
        }
        break;
    }
    assert!(saw_key_after_req, "no keyframe landed after request_idr");
    h.server_task.abort();
}

/// C5: the viewer-facing queue is bounded and presents the newest frame —
/// a slow consumer must see depth never exceed the cap while `latest_seq`
/// keeps advancing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_queue_newest_wins() {
    // 240 fps of small frames — deliberately faster than we consume.
    let mut h = harness(240, 1024, 60, None, false).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Measure occupancy atomically. Counting a drain while the producer keeps
    // sending can exceed 64 without the queue ever exceeding its capacity.
    let depth = h.session.frame_headers.len();
    assert!(depth <= 64, "queue held {depth} headers, cap is 64");
    // Bound this batch to the observed depth; arrivals can evict older entries,
    // but sequence numbers must still increase and total occupancy stays capped.
    let mut last = 0u64;
    for _ in 0..depth {
        let depth = h.session.frame_headers.len();
        assert!(depth <= 64, "queue held {depth} headers, cap is 64");
        let Some(h) = h.session.frame_headers.try_recv() else {
            break;
        };
        assert!(
            h.seq > last || last == 0,
            "stale seq {} after {}",
            h.seq,
            last
        );
        last = h.seq;
    }
    // Producer runs at 240fps; the latest delivered seq must keep
    // advancing past the queue cap — freshness proven. Poll with a
    // deadline: slow CI runners produce/deliver slower but the seq
    // must still climb.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let freshest = h.session.latest_seq();
        if freshest >= 100 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "latest_seq {freshest} not advancing"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h.server_task.abort();
}

/// C5: heartbeat + input acks — server-side measurement mode round-trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_acks_and_heartbeat_roundtrip() {
    let input = TestInput::default();
    let calls = input.calls.clone();
    let mut h = harness_with_input(30, 1024, 60, None, true, input).await;

    let seq = h
        .session
        .send_input(rds_core::InputKind::PointerMotion { dx: 3.0, dy: -2.0 })
        .await
        .unwrap();
    h.session.heartbeat().await.unwrap();

    let mut got_ack = false;
    let mut got_hb = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !(got_ack && got_hb) {
        match tokio::time::timeout(Duration::from_secs(2), h.session.events.recv()).await {
            Ok(Some(DesktopEvent::InputAck { seq: s, .. })) => {
                assert_eq!(s, seq);
                got_ack = true;
            }
            Ok(Some(DesktopEvent::Heartbeat { .. })) => got_hb = true,
            _ => {}
        }
    }
    assert!(got_ack, "no input ack received");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(got_hb, "no heartbeat echo received");
    assert!(h.session.control_rtt().is_some(), "heartbeat rtt measured");
    h.server_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn view_only_and_failed_injection_never_ack_but_keep_control_alive() {
    for impair in [None, Some(Impairment::clean())] {
        for view_only in [true, false] {
            let input = TestInput {
                view_only,
                fail: !view_only,
                ..Default::default()
            };
            let calls = input.calls.clone();
            let mut h = harness_with_input(30, 1024, 60, impair, true, input).await;
            for _ in 0..2 {
                h.session
                    .send_input(rds_core::InputKind::PointerMotion { dx: 3.0, dy: -2.0 })
                    .await
                    .unwrap();
            }
            h.session.heartbeat().await.unwrap();
            // The reliable control stream processes both inputs before this
            // heartbeat. Any false success ACK would arrive first.
            let event = tokio::time::timeout(Duration::from_secs(5), h.session.events.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(event, DesktopEvent::Heartbeat { .. }),
                "unexpected {event:?}"
            );
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                if view_only { 0 } else { 2 }
            );
            h.server_task.abort();
            let _ = h.server_task.await;
            h._client_ep.close().await;
            h._server_ep.close().await;
        }
    }
}

/// C5 impairment + G5 latency gate: 5% loss + 30 ms jitter on a 50 ms
/// base — queue stays bounded and control RTT is unaffected by the video
/// backlog. Header-arrival tail bounds below include retransmission; this is
/// not the clean-link 150 ms gate or an input-to-visible measurement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn impaired_link_latency_gate() {
    // 60 fps of ~1 KB frames ≈ one datagram per frame — enough samples
    // to make percentiles meaningful without saturating the lossy link.
    let mut h = harness(60, 1024, 60, Some(Impairment::lossy()), true).await;

    let mut latencies = Vec::new();
    let mut rtts = Vec::new();
    let mut queue_ms = Vec::new();
    let mut wire_ms = Vec::new();
    let mut last_seq: Option<u64> = None;
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        h.session.heartbeat().await.ok();
        tokio::select! {
            hd = h.session.frame_headers.recv() => {
                if let Some(hd) = hd {
                    if let Some(prev) = last_seq {
                        assert!(hd.seq > prev, "stale seq {} delivered after {}", hd.seq, prev);
                    }
                    last_seq = Some(hd.seq);
                    // Shared clock: capture→complete is true latency,
                    // split into queue-wait (capture→send) and wire
                    // (send→deliver) halves.
                    let now = h.clock.now_ms();
                    queue_ms.push(hd.send_ts_ms.saturating_sub(hd.capture_ts_ms));
                    wire_ms.push(now.saturating_sub(hd.send_ts_ms));
                    latencies.push(now.saturating_sub(hd.capture_ts_ms));
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        if let Some(rtt) = h.session.control_rtt() {
            rtts.push(rtt.as_millis() as u64);
        }
    }

    let (s_stats, c_stats) = h.impair.as_ref().unwrap();
    let (s, c) = (s_stats.get(), c_stats.get());
    let lat_p95 = p95(latencies.clone());
    let rtt_p95 = p95(rtts.clone());
    eprintln!(
        "impaired: frames={} lat_p95={}ms lat_p99={}ms queue_p95={}ms wire_p95={}ms rtt_p95={}ms server_out={:?} client_out={:?}",
        latencies.len(),
        lat_p95,
        p99(latencies.clone()),
        p95(queue_ms.clone()),
        p95(wire_ms),
        rtt_p95,
        s,
        c
    );
    // Proof the link itself was impaired, underneath QUIC: both
    // directions show real seeded drops and sustained media traffic.
    assert!(s.dropped > 0, "server-outbound loss never engaged: {s:?}");
    assert!(c.dropped > 0, "client-outbound loss never engaged: {c:?}");
    assert!(
        s.forwarded > 200,
        "media direction barely crossed the impaired socket: {s:?}"
    );
    assert!(
        latencies.len() > 50,
        "too few frames arrived: {}",
        latencies.len()
    );
    // Protocol queues stay bounded: capture→send wait is the bounded
    // channel and pacing time — must stay near zero even under loss.
    let queue_p95 = p95(queue_ms);
    let lat_p50 = p50(latencies.clone());
    assert!(
        queue_p95 <= 100,
        "queue wait p95 {queue_p95}ms — protocol queueing under impairment"
    );
    // The typical frame crosses near path speed: median ≈ base delay
    // + jitter + retransmit odds. Scheduler contention under parallel
    // tests stretches it, so the bound is generous — a real queue
    // backlog pushes the median into seconds, not hundreds of ms.
    assert!(
        lat_p50 <= 500,
        "latency p50 {lat_p50}ms — median frame not at path speed"
    );
    // Tail is retransmit physics: on 5% loss + 30 ms jitter a dropped
    // datagram repays ~1 PTO (~1 s). p95/p99 bounded proves the tail
    // is loss-driven, not an unbounded queue (which would run to 10s+).
    assert!(
        lat_p95 <= 2000 && p99(latencies) <= 3000,
        "latency tail unbounded under impairment"
    );
    // Control unaffected by video backlog: heartbeat RTT ≈ path RTT.
    assert!(
        rtt_p95 <= 400,
        "control rtt p95 {rtt_p95}ms — backlog leaked"
    );
    h.server_task.abort();
}

/// C5 soak: synthetic 60 fps stream. Default 20 s smoke; the checkpoint
/// runs the full `RDS_SOAK_SECS=1800` pass — steady RSS, bounded frame
/// age, zero unbounded-queue events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_60fps() {
    let secs: u64 = std::env::var("RDS_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let mut h = harness(60, 4096, 120, None, false).await;

    let mut ages = Vec::new();
    let mut rss_peak = 0u64;
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if let Ok(Some(hd)) =
            tokio::time::timeout(Duration::from_millis(500), h.session.frame_headers.recv()).await
        {
            ages.push(h.clock.now_ms().saturating_sub(hd.capture_ts_ms));
        }
        if let Some(rss) = self_rss_kb() {
            rss_peak = rss_peak.max(rss);
        }
    }
    let p95_age = p95(ages.clone());
    let p99_age = p99(ages.clone());
    eprintln!(
        "soak {secs}s: frames={} age_p95={}ms age_p99={}ms rss_peak={}KiB",
        ages.len(),
        p95_age,
        p99_age,
        rss_peak
    );
    // Starvation check: bounded admission may legitimately shed frames under
    // CPU contention, so the floor is sustained flow, not offered rate —
    // a stalled pipeline delivers ~0.
    assert!(ages.len() >= secs as usize * 10, "starved: {}", ages.len());
    // G5 synthetic header-arrival age ≤150 ms p95 on a clean loopback link.
    assert!(
        p95_age <= 150,
        "frame age p95 {p95_age}ms exceeds G5 budget"
    );
    assert!(p99_age <= 250, "frame age p99 {p99_age}ms unbounded");
    h.server_task.abort();
}

/// v3 uni demux: a live desktop session and a sync pull share ONE
/// connection. Both consume uni streams — Desktop-tagged frame streams
/// and Sync-tagged chunk streams — which the connection demux routes
/// to their own consumer. Pre-v3, two `accept_uni` callers raced and
/// each could swallow the other's streams.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desktop_and_sync_share_one_connection() {
    let clock = SessionClock::default();
    let (server_ep, client_ep, _imp, target) = endpoints(None).await;

    // Sync root with a multi-chunk file to pull while frames stream.
    let sync_root = std::env::temp_dir().join(format!(
        "rds-coexist-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&sync_root).unwrap();
    let data: Vec<u8> = (0..400_000u32).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(sync_root.join("media.bin"), &data).unwrap();

    // Server: dispatch each control stream — Desktop session or Sync —
    // like the agent does.
    let server_task = tokio::spawn({
        let sync_root = sync_root.clone();
        let clock = clock.clone();
        async move {
            let conn = server_ep.accept().await.unwrap().await.unwrap();
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                let conn = conn.clone();
                let dir = sync_root.clone();
                let clock = clock.clone();
                tokio::spawn(async move {
                    let greeting = read_frame::<_, StreamHello>(&mut recv).await;
                    let frame_route = match &greeting {
                        Ok(StreamHello::DesktopV2 { session, .. }) => {
                            Some(rds_core::UniHello::DesktopFrames { id: *session })
                        }
                        _ => None,
                    };
                    match greeting {
                        Ok(StreamHello::Desktop(hello))
                        | Ok(StreamHello::DesktopV2 { hello, .. }) => {
                            rds_net::write_frame(
                                &mut send,
                                &HelloAck::Desktop(rds_core::DesktopCaps {
                                    displays: vec![],
                                    codecs: vec![Codec::H264],
                                }),
                            )
                            .await
                            .unwrap();
                            let _ = serve_desktop_with(
                                conn,
                                send,
                                recv,
                                hello,
                                SessionConfig {
                                    producer: Some(Box::new(
                                        SyntheticProducer::new(90, 320, 240, 1500)
                                            .keyframe_every(30),
                                    )),
                                    clock: Some(clock),
                                    frame_route,
                                    ..Default::default()
                                },
                            )
                            .await;
                        }
                        Ok(StreamHello::Sync) => {
                            let _ = rds_sync::engine::serve(conn, send, recv, dir).await;
                        }
                        _ => {}
                    }
                });
            }
        }
    });

    let conn = client_ep.connect(target, rds_core::ALPN).await.unwrap();
    let mut session = DesktopSession::connect_opts(
        &conn,
        DesktopHello {
            display: 0,
            max_fps: 90,
            codec: Codec::H264,
            input_acks: false,
        },
        SessionOpts {
            clock: Some(clock.clone()),
            session: Some(next_session_id()),
        },
    )
    .await
    .unwrap();

    // Pull while frames stream: the sync engine claims `UniHello::Sync`
    // streams; the session owns `UniHello::Desktop`. A misrouted stream
    // either stalls the pull or kills the frame task — both would fail
    // the asserts below.
    let dest_dir = sync_root.join("dest");
    let conn2 = conn.clone();
    let pull = tokio::spawn(async move {
        let (mut send, recv) = conn2.open_bi().await.unwrap();
        rds_net::write_frame(&mut send, &StreamHello::Sync)
            .await
            .unwrap();
        rds_sync::engine::recv_file(&conn2, "media.bin", &dest_dir, send, recv).await
    });

    // Consume frames while the pull runs.
    let mut frames = 0usize;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut pull_done = false;
    while Instant::now() < deadline && !(pull_done && frames >= 30) {
        tokio::select! {
            h = session.frame_headers.recv() => if h.is_some() { frames += 1; },
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                pull_done |= pull.is_finished();
            }
        }
    }
    let (dest, stats) = pull.await.unwrap().unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data, "pull bytes differ");
    assert!(stats.fetched > 0);
    assert!(
        frames >= 30,
        "desktop starved by concurrent sync: {frames} frames"
    );
    server_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_and_foreign_frame_routes_never_reach_the_session_inbox() {
    let clock = SessionClock::default();
    let (server_ep, client_ep, _imp, target) = endpoints(None).await;
    let session_id = next_session_id();
    let server_task = tokio::spawn({
        let clock = clock.clone();
        async move {
            let conn = server_ep.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let greeting = read_frame::<_, StreamHello>(&mut recv).await.unwrap();
            let (hello, frame_route) = match greeting {
                StreamHello::DesktopV2 { session, hello } => {
                    assert_eq!(session, session_id);
                    (hello, rds_core::UniHello::DesktopFrames { id: session })
                }
                other => panic!("expected DesktopV2 hello, got {other:?}"),
            };
            rds_net::write_frame(
                &mut send,
                &HelloAck::Desktop(rds_core::DesktopCaps {
                    displays: vec![],
                    codecs: vec![Codec::H264],
                }),
            )
            .await
            .unwrap();
            // Forge two stale streams the way a torn previous session
            // could leave them: one tagged with a session ID this
            // connection never served, one on the legacy shared route.
            // Neither can reach the live session's claimed inbox.
            for tag in [
                rds_core::UniHello::DesktopFrames { id: [0xEE; 16] },
                rds_core::UniHello::Desktop,
            ] {
                let mut forged = conn.open_uni().await.unwrap();
                rds_net::write_frame(&mut forged, &tag).await.unwrap();
                rds_net::write_frame(
                    &mut forged,
                    &rds_core::FrameHeader {
                        seq: u64::MAX,
                        capture_ts_ms: 0,
                        encode_done_ts_ms: 0,
                        send_ts_ms: 0,
                        keyframe: true,
                        codec: Codec::H264,
                        width: 1,
                        height: 1,
                    },
                )
                .await
                .unwrap();
                forged.finish().unwrap();
            }
            serve_desktop_with(
                conn,
                send,
                recv,
                hello,
                SessionConfig {
                    producer: Some(Box::new(
                        SyntheticProducer::new(60, 640, 480, 1500).keyframe_every(10),
                    )),
                    clock: Some(clock),
                    frame_route: Some(frame_route),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
    });
    let conn = client_ep.connect(target, rds_core::ALPN).await.unwrap();
    let mut session = DesktopSession::connect_opts(
        &conn,
        DesktopHello {
            display: 0,
            max_fps: 60,
            codec: Codec::H264,
            input_acks: false,
        },
        SessionOpts {
            clock: Some(clock.clone()),
            session: Some(session_id),
        },
    )
    .await
    .unwrap();
    // Every header reaching the inbox must be this session's own frame
    // shape; a forged stream would surface a bogus header or break the
    // decoder chain.
    for _ in 0..20 {
        let header = tokio::time::timeout(Duration::from_secs(10), session.frame_headers.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((header.width, header.height), (640, 480));
    }
    server_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_shared_route_still_serves_v1_clients() {
    let clock = SessionClock::default();
    let (server_ep, client_ep, _imp, target) = endpoints(None).await;
    let server_task = tokio::spawn({
        let clock = clock.clone();
        async move {
            let conn = server_ep.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let StreamHello::Desktop(hello) =
                read_frame::<_, StreamHello>(&mut recv).await.unwrap()
            else {
                panic!("expected legacy Desktop hello");
            };
            rds_net::write_frame(
                &mut send,
                &HelloAck::Desktop(rds_core::DesktopCaps {
                    displays: vec![],
                    codecs: vec![Codec::H264],
                }),
            )
            .await
            .unwrap();
            serve_desktop_with(
                conn,
                send,
                recv,
                hello,
                SessionConfig {
                    producer: Some(Box::new(
                        SyntheticProducer::new(60, 640, 480, 1500).keyframe_every(10),
                    )),
                    clock: Some(clock),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
    });
    let conn = client_ep.connect(target, rds_core::ALPN).await.unwrap();
    let mut session = DesktopSession::connect_opts(
        &conn,
        DesktopHello {
            display: 0,
            max_fps: 60,
            codec: Codec::H264,
            input_acks: false,
        },
        SessionOpts {
            clock: Some(clock.clone()),
            session: None,
        },
    )
    .await
    .unwrap();
    for _ in 0..10 {
        tokio::time::timeout(Duration::from_secs(10), session.frame_headers.recv())
            .await
            .unwrap()
            .unwrap();
    }
    server_task.abort();
}

#[cfg(target_os = "linux")]
fn self_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmRSS"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
}

#[cfg(not(target_os = "linux"))]
fn self_rss_kb() -> Option<u64> {
    None
}
