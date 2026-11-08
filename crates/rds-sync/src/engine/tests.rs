use super::*;

// These two fixtures deliberately exhaust the same process-wide pool. They
// must not reserve permits against each other while waiting for their workers.
static DISK_POOL_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn cancel_join_waits_for_the_running_worker_and_disposes_its_locked_output() {
    let root = Scratch::new();
    let manifest = crate::manifest_of(b"held native store");
    let journal = Journal::open(&root.0, "data.bin", &manifest).unwrap();
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (jobs, receiver) = mpsc::channel(1);
    // Model an already-running uninterruptible filesystem operation. Its
    // returned Journal also holds the root lock until the join output drops.
    let task = tokio::task::spawn_blocking(move || {
        started.send(()).unwrap();
        blocked.recv().unwrap();
        drop(receiver);
        Ok(journal)
    });
    let mut sink = JournalSink {
        cancel: StoreCancellation {
            canceled: Arc::new(AtomicBool::new(false)),
            task: task.abort_handle(),
        },
        jobs: Some(jobs),
        error: Arc::new(std::sync::Mutex::new(None)),
        task: Some(task),
    };
    ready.await.unwrap();
    let mut cleanup = Box::pin(sink.cancel_and_join());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut cleanup)
            .await
            .is_err()
    );
    let error = Journal::open(&root.0, "data.bin", &manifest).err().unwrap();
    assert!(
        matches!(error, crate::SyncError::Io(ref error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), cleanup)
        .await
        .unwrap()
        .unwrap();
    // No polling/retry: completed cleanup itself guarantees release.
    assert!(Journal::open(&root.0, "data.bin", &manifest).is_ok());
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("rds-sink-cancel-{:032x}", rand::random::<u128>()));
        // Claim the directory before this guard owns recursive cleanup.
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn queued_writer(cancel_finish: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let root = Scratch::new();
    let data = vec![17u8; 4096];
    let manifest = crate::manifest_of(&data);
    runtime.block_on(async {
        // Occupy the sole blocking worker. The queued store cannot have started
        // when the async receiver/finish future is canceled.
        let (release, blocked) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            blocked.recv().unwrap();
        });
        ready.await.unwrap();
        let journal = Journal::open(&root.0, "data.bin", &manifest).unwrap();
        let (sink, stopped) = JournalSink::start(journal).await;
        sink.put(0, data).await.unwrap();
        if cancel_finish {
            assert!(
                tokio::time::timeout(Duration::from_millis(10), sink.finish())
                    .await
                    .is_err()
            );
        } else {
            drop(sink);
        }
        release.send(()).unwrap();
        blocker.await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), stopped)
            .await
            .unwrap();
        // The same blocking worker must have returned and dropped its output.
        tokio::task::spawn_blocking(|| ()).await.unwrap();
        let resumed = Journal::open(&root.0, "data.bin", &manifest).unwrap();
        assert!(
            resumed.have_set().is_empty(),
            "canceled queued chunk was still written to disk"
        );
    });
}

#[test]
fn dropping_sink_discards_stores_that_have_not_started() {
    queued_writer(false);
}

#[test]
fn canceling_finish_discards_stores_that_have_not_started() {
    queued_writer(true);
}

#[test]
fn dropping_queued_journal_preparation_does_not_create_receive_state() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let root = Scratch::new();
    let pending = root.0.join("not-created");
    runtime.block_on(async {
        let (release, blocked) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            blocked.recv().unwrap();
        });
        ready.await.unwrap();
        let manifest = crate::manifest_of(b"never admitted");
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                prepare_journal(
                    &pending,
                    "data.bin",
                    &manifest,
                    Arc::new(AtomicBool::new(false)),
                    None
                )
            )
            .await
            .is_err()
        );
        release.send(()).unwrap();
        blocker.await.unwrap();
        // The single blocking worker has finished any previously queued open.
        tokio::task::spawn_blocking(|| ()).await.unwrap();
        assert!(
            !pending.exists(),
            "abandoned queued preparation created state"
        );
        assert!(Journal::open(&root.0, "data.bin", &manifest).is_ok());
    });
}

#[tokio::test]
async fn journal_preparation_observes_peer_and_caller_cancellation() {
    for peer_canceled in [false, true] {
        let root = Scratch::new();
        let pending = root.0.join("not-created");
        let result = prepare_journal(
            &pending,
            "data.bin",
            &crate::manifest_of(b"never admitted"),
            Arc::new(AtomicBool::new(peer_canceled)),
            Some(Arc::new(AtomicBool::new(!peer_canceled))),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Err(crate::SyncError::Io(ref e)) if e.kind() == std::io::ErrorKind::Interrupted)
        );
        assert!(!pending.exists());
    }
}

#[test]
fn dropping_queued_assembly_retains_destination_and_verified_parts() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let root = Scratch::new();
    std::fs::write(root.0.join("data.bin"), b"old destination").unwrap();
    let bytes = b"verified replacement";
    let manifest = crate::manifest_of(bytes);
    let mut journal = Journal::open(&root.0, "data.bin", &manifest).unwrap();
    journal.store(0, bytes).unwrap();
    runtime.block_on(async {
        let (release, blocked) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            blocked.recv().unwrap();
        });
        ready.await.unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                assemble_journal(journal, Arc::new(AtomicBool::new(false)), None,)
            )
            .await
            .is_err()
        );
        release.send(()).unwrap();
        blocker.await.unwrap();
        tokio::task::spawn_blocking(|| ()).await.unwrap();
        assert_eq!(
            std::fs::read(root.0.join("data.bin")).unwrap(),
            b"old destination",
            "abandoned assembly published from the blocking queue"
        );
        let resumed = Journal::open(&root.0, "data.bin", &manifest).unwrap();
        assert!(
            resumed.complete(),
            "abandoned assembly discarded verified resume state"
        );
    });
}

#[tokio::test(start_paused = true)]
async fn quiet_control_during_data_has_no_independent_idle_deadline() {
    let (tx, rx) = mpsc::channel(8);
    // Isolate phase timing from transport idle timers and packet scheduling.
    let mut frames = ControlFrames {
        rx,
        stop: Arc::new(AtomicBool::new(false)),
        cause: Arc::new(std::sync::Mutex::new(None)),
        cause_notify: Arc::new(tokio::sync::Notify::new()),
        quit: tokio_util::sync::CancellationToken::new(),
        reader: None,
    };
    assert!(
        tokio::time::timeout(READ_STALL * 2, frames.during_data())
            .await
            .is_err(),
        "quiet control interrupted an independently progressing data phase"
    );
    tx.send(SyncMsg::Cancel {
        reason: "after quiet data".into(),
    })
    .await
    .unwrap();
    assert!(matches!(
        frames.during_data().await.unwrap(),
        SyncMsg::Cancel { .. }
    ));
    drop(tx);
    assert!(frames.during_data().await.is_err());
}

#[tokio::test]
async fn normal_finish_drains_and_verifies_every_queued_store() {
    let root = Scratch::new();
    let data = vec![19u8; 4096];
    let manifest = crate::manifest_of(&data);
    let journal = Journal::open(&root.0, "data.bin", &manifest).unwrap();
    let (sink, _stopped) = JournalSink::start(journal).await;
    sink.put(0, data.clone()).await.unwrap();
    let journal = sink.finish().await.unwrap();
    assert!(journal.complete());
    let path = journal.assemble().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
}

// ---- W1.9/W2.2 session envelope tests ---------------------------------

fn pair() -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
    tokio::io::duplex(64 * 1024)
}

#[test]
fn session_limits_negotiate_pairwise_min_and_refuse_zero() {
    let local = SessionLimits::LOCAL;
    let peer = SessionLimits {
        max_chunk: local.max_chunk / 2,
        max_chunks: local.max_chunks * 2,
        fetch_streams: 1,
    };
    let n = SessionLimits::negotiate(local, peer).unwrap();
    assert_eq!(n.max_chunk, local.max_chunk / 2);
    assert_eq!(n.max_chunks, local.max_chunks);
    assert_eq!(n.fetch_streams, 1);
    // Peer declarations above the wire ceiling are clamped, not trusted.
    let over = SessionLimits {
        max_chunk: u32::MAX,
        max_chunks: u32::MAX,
        fetch_streams: u8::MAX,
    };
    let n = SessionLimits::negotiate(local, over).unwrap();
    assert_eq!(n, local);
    for field in [
        SessionLimits {
            max_chunk: 0,
            ..local
        },
        SessionLimits {
            max_chunks: 0,
            ..local
        },
        SessionLimits {
            fetch_streams: 0,
            ..local
        },
    ] {
        assert!(SessionLimits::negotiate(local, field).is_err());
    }
}

#[test]
fn to_session_maps_every_transfer_variant_and_never_nests() {
    use crate::proto::*;
    let msgs = [
        SyncMsg::Offer {
            rel_path: "a".into(),
            size: 1,
            root: [0; 32],
            chunk_count: 0,
        },
        SyncMsg::Request {
            rel_path: "a".into(),
        },
        SyncMsg::Refuse { reason: "x".into() },
        SyncMsg::Cancel { reason: "x".into() },
        SyncMsg::ManifestPart { chunks: vec![] },
        SyncMsg::Need { bits: vec![] },
        SyncMsg::Done { root: [0; 32] },
        SyncMsg::ChunkSet { indices: vec![] },
        SyncMsg::ChunkHdr {
            index: 0,
            hash: [0; 32],
            len: 0,
        },
        SyncMsg::SetDone,
    ];
    for msg in msgs {
        assert!(to_session(&msg).is_some(), "{msg:?} must translate");
    }
    assert!(
        to_session(&SyncMsg::Session {
            transfer_id: [0; 16],
            msg: SessionMsg::SetDone,
        })
        .is_none()
    );
}

#[tokio::test]
async fn v2_envelope_asserts_transfer_id() {
    let (mut a, mut b) = pair();
    let wire = Wire::v2([7; 16], SessionLimits::LOCAL);
    tokio::spawn(async move {
        write_frame(
            &mut b,
            &SyncMsg::Session {
                transfer_id: [9; 16],
                msg: SessionMsg::Request {
                    rel_path: "x".into(),
                },
            },
        )
        .await
        .unwrap();
    });
    let err = wire.recv(&mut a).await.unwrap_err();
    assert!(err.to_string().contains("different transfer"));
}

#[tokio::test]
async fn v2_repeated_greeting_mid_transfer_fails() {
    let (mut a, mut b) = pair();
    let wire = Wire::v2([7; 16], SessionLimits::LOCAL);
    tokio::spawn(async move {
        write_frame(
            &mut b,
            &SyncMsg::Session {
                transfer_id: [7; 16],
                msg: SessionMsg::Hello {
                    version: SESSION_VERSION,
                    limits: SessionLimits::LOCAL,
                },
            },
        )
        .await
        .unwrap();
    });
    let err = wire.recv(&mut a).await.unwrap_err();
    assert!(err.to_string().contains("repeated"));
}

#[tokio::test]
async fn v1_wire_maps_cancel_to_refuse_and_roundtrips() {
    let (mut a, mut b) = pair();
    tokio::spawn(async move {
        Wire::V1
            .send(
                &mut b,
                &SyncMsg::Cancel {
                    reason: "stop".into(),
                },
            )
            .await
            .unwrap();
        Wire::V1
            .send(&mut b, &SyncMsg::Need { bits: vec![3] })
            .await
            .unwrap();
    });
    match Wire::V1.recv(&mut a).await.unwrap() {
        SyncMsg::Refuse { reason } => assert!(reason.contains("stop")),
        other => panic!("v1 Cancel must arrive as Refuse, got {other:?}"),
    }
    assert!(matches!(
        Wire::V1.recv(&mut a).await.unwrap(),
        SyncMsg::Need { .. }
    ));
}

#[tokio::test]
async fn session_open_refuses_wrong_version_with_clear_error() {
    let (a, mut b) = pair();
    tokio::spawn(async move {
        // Read Hello, answer a mismatched version.
        let msg: SyncMsg = read_frame(&mut b).await.unwrap();
        let SyncMsg::Session { transfer_id, .. } = msg else {
            panic!("expected Session hello");
        };
        write_frame(
            &mut b,
            &SyncMsg::Session {
                transfer_id,
                msg: SessionMsg::HelloAck {
                    version: 99,
                    limits: SessionLimits::LOCAL,
                },
            },
        )
        .await
        .unwrap();
    });
    let (mut recv, mut send) = tokio::io::split(a);
    let err = session_open([7; 16], &mut send, &mut recv)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("version 99"));
}

/// W2.5: every `spawn_blocking` in this crate funnels through `disk_job`,
/// so a store/scan storm can never hold more than `MAX_DISK_JOBS`
/// blocking threads at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disk_jobs_share_one_bounded_pool() {
    let _exclusive = DISK_POOL_TEST.lock().await;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Condvar, Mutex};

    // Retain each admitted worker until the whole pool has arrived. A sleep
    // measures scheduler luck and can run serially on a busy machine. Always
    // open the gate on drop, including timeout/panic, so blocking work cannot
    // keep the test runtime alive forever.
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Release(gate.clone());
    let (started, mut starts) = mpsc::channel(MAX_DISK_JOBS * 2);
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..(MAX_DISK_JOBS * 2) {
        let (a, p) = (active.clone(), peak.clone());
        let gate = gate.clone();
        let started = started.clone();
        set.spawn(disk_job(move || {
            let n = a.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(n, Ordering::SeqCst);
            started.try_send(()).unwrap();
            drop(
                gate.1
                    .wait_while(gate.0.lock().unwrap(), |released| !*released)
                    .unwrap(),
            );
            a.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    let filled = tokio::time::timeout(Duration::from_secs(30), async {
        for _ in 0..MAX_DISK_JOBS {
            starts
                .recv()
                .await
                .expect("disk worker announced admission");
        }
    })
    .await;
    if filled.is_ok() {
        assert_eq!(DISK_JOBS.available_permits(), 0);
        assert_eq!(active.load(Ordering::SeqCst), MAX_DISK_JOBS);
    }
    drop(release);
    while let Some(result) = set.join_next().await {
        result.unwrap().unwrap();
    }
    filled.expect("disk pool never filled");
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(peak.load(Ordering::SeqCst), MAX_DISK_JOBS);
}

/// W1.9: the token→flag projection must flip promptly and stay scoped to
/// the one transfer that created it.
#[tokio::test]
async fn cancel_flag_watcher_projects_token_state() {
    let (flag, watcher) = cancel_flag_watcher(&None);
    assert!(flag.is_none() && watcher.is_none());

    let token = tokio_util::sync::CancellationToken::new();
    let (flag, watcher) = cancel_flag_watcher(&Some(token.clone()));
    let flag = flag.unwrap();
    assert!(!flag.load(Ordering::Acquire));
    token.cancel();
    flag_watcher_wait(&flag).await;
    watcher.unwrap().abort();
}

#[tokio::test]
async fn dropping_cancel_projection_stops_its_waiter() {
    let token = tokio_util::sync::CancellationToken::new();
    let (_, watcher) = cancel_flag_watcher(&Some(token));
    let handle = watcher.as_ref().unwrap().0.abort_handle();
    drop(watcher);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceled_disk_waiter_retains_running_work_budget() {
    let _exclusive = DISK_POOL_TEST.lock().await;
    let _other = DISK_JOBS
        .acquire_many((MAX_DISK_JOBS - 1) as u32)
        .await
        .unwrap();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let worker = tokio::spawn(disk_job(move || {
        let _ = entered.send(());
        let _ = released.recv_timeout(Duration::from_secs(5));
    }));
    ready.await.unwrap();
    worker.abort();
    let _ = worker.await;
    assert!(
        DISK_JOBS.try_acquire().is_err(),
        "running work lost its permit"
    );
    release.send(()).unwrap();
    let permit = tokio::time::timeout(Duration::from_secs(2), DISK_JOBS.acquire())
        .await
        .unwrap()
        .unwrap();
    drop(permit);
}

async fn flag_watcher_wait(flag: &Arc<AtomicBool>) {
    for _ in 0..200 {
        if flag.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("cancel flag was never projected");
}

/// W1.9: a flag raised before the scan starts aborts the manifest build;
/// an unset flag leaves the manifest identical to the plain scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manifest_scan_honors_cancel_flag() {
    let dir = Scratch::new();
    let path = dir.0.join("scan.bin");
    std::fs::write(&path, vec![0x5Au8; 1_000_000]).unwrap();

    let raised = Arc::new(AtomicBool::new(true));
    let file = Arc::new(File::open(&path).unwrap());
    let err = manifest_from_file(file, {
        let raised = raised.clone();
        move || raised.load(Ordering::Acquire)
    })
    .await
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("operation interrupted"),
        "expected interrupted scan, got {err:#}"
    );

    // A canceled scan leaves the shared file position mid-stream — a
    // fresh handle is the honest rescan, matching engine behavior.
    let file = Arc::new(File::open(&path).unwrap());
    let scanned = manifest_from_file(file, || false).await.unwrap();
    assert_eq!(
        scanned.root,
        crate::manifest_of(&std::fs::read(&path).unwrap()).root
    );
}

/// A real bi-stream pair plus its peer half on a second endpoint —
/// the fixture the `ControlFrames` tests exercise. The accept half is
/// spawned first so the incoming handshake is driven while `connect`
/// dials, matching how `spawn_server_v2` pairs endpoints.
async fn stream_pair() -> (
    SendStream,
    RecvStream,
    SendStream,
    Connection,
    Connection,
    rds_net::Endpoint,
    rds_net::Endpoint,
) {
    // This fixture tests framed control between two local endpoints. Pin the
    // local route so host LAN/NAT/discovery cannot turn it into a remote dial.
    let config = rds_net::EndpointConfig {
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        discovery: false,
        ..Default::default()
    };
    let a = rds_net::bind_endpoint(config.clone()).await.unwrap();
    let b = rds_net::bind_endpoint(config).await.unwrap();
    let b_addr = b.addr();
    // The accept future must be polled while `connect` dials (iroh drives
    // the incoming handshake through it), and `b` must outlive it —
    // dropping the endpoint closes the accepted connection.
    let (conn_a, conn_b) = tokio::join!(
        async { a.connect(b_addr, rds_core::ALPN).await.unwrap() },
        async { b.accept().await.unwrap().await.unwrap() },
    );
    let (mut send, recv) = conn_a.open_bi().await.unwrap();
    // Stream opens are lazy: the peer's `accept_bi` only yields the
    // stream after the opener writes its first bytes.
    write_frame(
        &mut send,
        &SyncMsg::Session {
            transfer_id: [0; 16],
            msg: SessionMsg::Need { bits: vec![] },
        },
    )
    .await
    .unwrap();
    let (peer_send, _peer_recv) = conn_b.accept_bi().await.unwrap();
    (send, recv, peer_send, conn_a, conn_b, a, b)
}

/// W1.9 hardening: the shared reader flips `stop` the moment a typed
/// `Cancel` is decoded — before any phase consumes it — and the frame
/// still surfaces through `next` in order with its reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_reader_flags_peer_cancel_before_consumption() {
    let id: [u8; 16] = rand::random();
    let (_send, recv, mut peer_send, _ca, _cb, _ea, _eb) = stream_pair().await;
    let mut frames = ControlFrames::open(Wire::v2(id, SessionLimits::LOCAL), recv);
    write_frame(
        &mut peer_send,
        &SyncMsg::Session {
            transfer_id: id,
            msg: SessionMsg::Cancel {
                reason: "peer walked away".into(),
            },
        },
    )
    .await
    .unwrap();
    flag_watcher_wait(&frames.stop_flag()).await;
    match frames.next(Duration::from_secs(10)).await.unwrap() {
        SyncMsg::Cancel { reason } => assert_eq!(reason, "peer walked away"),
        other => panic!("expected Cancel, got {other:?}"),
    }
    frames.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_reader_flags_refusal_before_consumption_on_both_profiles() {
    for wire in [Wire::V1, Wire::v2([3; 16], SessionLimits::LOCAL)] {
        let (_send, recv, mut peer_send, _ca, _cb, _ea, _eb) = stream_pair().await;
        let mut frames = ControlFrames::open(wire, recv);
        wire.send(
            &mut peer_send,
            &SyncMsg::Refuse {
                reason: "do not start queued disk work".into(),
            },
        )
        .await
        .unwrap();
        flag_watcher_wait(&frames.stop_flag()).await;
        assert!(
            frames
                .abort_cause("missing reason")
                .await
                .contains("do not start")
        );
        assert!(matches!(
            frames.during_data().await.unwrap(),
            SyncMsg::Refuse { .. }
        ));
        frames.close().await;
    }
}

/// Ordered phase traffic flows through the queue untouched and `stop`
/// stays clear; the peer finishing its send half ends the reader and
/// reports the stream closed to the next wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_reader_preserves_order_and_reports_peer_fin() {
    let id: [u8; 16] = rand::random();
    let (_send, recv, mut peer_send, _ca, _cb, _ea, _eb) = stream_pair().await;
    let mut frames = ControlFrames::open(Wire::v2(id, SessionLimits::LOCAL), recv);
    let root = [7u8; 32];
    for msg in [
        SessionMsg::Need { bits: vec![0b11] },
        SessionMsg::Done { root },
    ] {
        write_frame(
            &mut peer_send,
            &SyncMsg::Session {
                transfer_id: id,
                msg,
            },
        )
        .await
        .unwrap();
    }
    match frames.next(Duration::from_secs(10)).await.unwrap() {
        SyncMsg::Need { bits } => assert_eq!(bits, vec![0b11]),
        other => panic!("expected Need, got {other:?}"),
    }
    match frames.next(Duration::from_secs(10)).await.unwrap() {
        SyncMsg::Done { root: got } => assert_eq!(got, root),
        other => panic!("expected Done, got {other:?}"),
    }
    assert!(!frames.stop_flag().load(Ordering::Acquire));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), frames.drained())
            .await
            .is_err()
    );
    peer_send.finish().unwrap();
    frames.drained().await.unwrap();
    assert!(frames.stop_flag().load(Ordering::Acquire));
    assert!(frames.next(Duration::from_secs(10)).await.is_err());
    frames.close().await;
    frames.close().await;
}

/// `close` runs the `recv.stop` epilogue on the wire: the peer's send
/// half observes the stop instead of being left open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_reader_close_stops_the_peer_send() {
    let id: [u8; 16] = rand::random();
    let (_send, recv, peer_send, _ca, _cb, _ea, _eb) = stream_pair().await;
    let mut frames = ControlFrames::open(Wire::v2(id, SessionLimits::LOCAL), recv);
    frames.close().await;
    frames.close().await;
    tokio::time::timeout(Duration::from_secs(10), peer_send.stopped())
        .await
        .expect("peer send must observe the control stop")
        .unwrap();
}
