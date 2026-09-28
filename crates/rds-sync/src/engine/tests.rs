use super::*;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("rds-sink-cancel-{:032x}", rand::random::<u128>())))
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
    use std::sync::atomic::AtomicUsize;
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..(MAX_DISK_JOBS * 2) {
        let (a, p) = (active.clone(), peak.clone());
        set.spawn(disk_job(move || {
            let n = a.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(15));
            a.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    while set.join_next().await.is_some() {}
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert!(peak.load(Ordering::SeqCst) <= MAX_DISK_JOBS);
    assert!(peak.load(Ordering::SeqCst) > 1, "jobs never overlapped");
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
    std::fs::create_dir(&dir.0).unwrap();
    let path = dir.0.join("scan.bin");
    std::fs::write(&path, vec![0x5Au8; 1_000_000]).unwrap();

    let raised = Arc::new(AtomicBool::new(true));
    let file = Arc::new(File::open(&path).unwrap());
    let err = manifest_from_file(file, Some(raised)).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("operation interrupted"),
        "expected interrupted scan, got {err:#}"
    );

    // A canceled scan leaves the shared file position mid-stream — a
    // fresh handle is the honest rescan, matching engine behavior.
    let file = Arc::new(File::open(&path).unwrap());
    let scanned = manifest_from_file(file, None).await.unwrap();
    assert_eq!(
        scanned.root,
        crate::manifest_of(&std::fs::read(&path).unwrap()).root
    );
}
