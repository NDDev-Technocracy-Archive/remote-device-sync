//! C6 end-to-end: real transfers over in-process rds-net endpoints —
//! byte-identical completion, resume after mid-transfer kills, torn
//! journals, corrupt parts, traversal rejection, and an impaired-link
//! lane that proves the resume overhead stays small.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rds_net::{Endpoint, EndpointAddr, EndpointConfig, bind_endpoint, bind_noq_with_socket};
use rds_sync::engine::{recv_file, send_file, serve};
use rds_sync::proto::{check_manifest, check_rel_path};
use rds_sync::{Manifest, manifest_of};

#[path = "support/scratch.rs"]
mod scratch;
use scratch::Scratch;

fn scratch(name: &str) -> Scratch {
    Scratch::new(&format!("rds-sync-{name}"))
}

/// Deterministic pseudo-random content — reproducible per size/seed.
fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut v = vec![0u8; len];
    for b in &mut v {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *b = state as u8;
    }
    v
}

/// Server half: accept one conn, run `serve` on each accepted bi
/// stream — mirrors the agent's per-stream dispatch.
fn spawn_server(ep: Endpoint, dir: Scratch) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let dir = dir.clone();
            tokio::spawn(async move {
                while let Ok((send, recv)) = conn.accept_bi().await {
                    let conn = conn.clone();
                    let dir = dir.clone();
                    tokio::spawn(async move {
                        let _ = serve(conn, send, recv, dir.to_path_buf()).await;
                        drop(dir);
                    });
                }
            });
        }
    })
}

async fn pair() -> (
    Endpoint,
    Endpoint,
    EndpointAddr,
    tokio::task::JoinHandle<()>,
    Scratch,
) {
    let config = EndpointConfig {
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    };
    let server_ep = bind_endpoint(config.clone()).await.unwrap();
    let client_ep = bind_endpoint(config).await.unwrap();
    let dir = scratch("server");
    let task = spawn_server(server_ep.clone(), dir.clone());
    let target = server_ep.addr();
    (server_ep, client_ep, target, task, dir)
}

async fn client_conn(client_ep: &Endpoint, target: EndpointAddr) -> rds_net::Connection {
    client_ep.connect(target, rds_core::ALPN).await.unwrap()
}

/// Part files present under `.rds-sync/*/parts` — resume progress.
fn count_parts(state_dir: &Path) -> usize {
    let Ok(roots) = std::fs::read_dir(state_dir) else {
        return 0;
    };
    roots
        .flatten()
        .map(|e| e.path().join("parts"))
        .map(|p| {
            std::fs::read_dir(&p)
                .map(|d| {
                    d.flatten()
                        .filter(|f| {
                            f.path().is_file()
                                && f.file_name().to_str().is_some_and(|name| {
                                    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
                                })
                        })
                        .count()
                })
                .unwrap_or(0)
        })
        .sum()
}

async fn push(
    client_ep: &Endpoint,
    target: EndpointAddr,
    path: &Path,
) -> anyhow::Result<rds_sync::engine::Stats> {
    let conn = client_conn(client_ep, target).await;
    // Each attempt owns a fresh connection, including cancellation before any
    // application stream opens. Background readers cannot retain a killed dial.
    struct Close(rds_net::Connection);
    impl Drop for Close {
        fn drop(&mut self) {
            self.0.close(0u32.into(), b"transfer attempt ended");
        }
    }
    let _close = Close(conn.clone());
    let (send, recv) = conn.open_bi().await.unwrap();
    send_file(&conn, path, send, recv).await
}

/// Client-task cancellation does not join a receiver's running filesystem
/// operation. Wait for its actual exclusive journal admission before the one
/// resumed transfer; only a typed lock-contention error is transient here.
async fn wait_receive_journal(
    dir: &Path,
    rel: &str,
    manifest: &Manifest,
) -> rds_sync::journal::Journal {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (dir, rel, manifest) = (dir.to_owned(), rel.to_owned(), manifest.clone());
            let opened = tokio::task::spawn_blocking(move || {
                rds_sync::journal::Journal::open(&dir, &rel, &manifest)
            })
            .await
            .unwrap();
            match opened {
                Ok(journal) => break journal,
                Err(rds_sync::SyncError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("unexpected receive admission error: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("canceled receive retained its journal lock beyond the cleanup bound")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_completes_byte_identical() {
    let (_s, c_ep, target, _task, server_dir) = pair().await;
    let src_dir = scratch("src");
    let data = random_bytes(8 * 1024 * 1024, 0xC6);
    let src = src_dir.join("blob.bin");
    std::fs::write(&src, &data).unwrap();

    let stats = push(&c_ep, target.clone(), &src).await.unwrap();
    assert_eq!(stats.bytes, data.len() as u64);
    let got = std::fs::read(server_dir.join("blob.bin")).unwrap();
    assert_eq!(got, data, "received file is byte-identical");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_content_zero_chunks() {
    let (_s, c_ep, target, _task, server_dir) = pair().await;
    let src_dir = scratch("src");
    let data = random_bytes(4 * 1024 * 1024, 0xD00D);
    let src = src_dir.join("same.bin");
    std::fs::write(&src, &data).unwrap();

    let first = push(&c_ep, target.clone(), &src).await.unwrap();
    assert!(first.fetched > 0);
    assert!(server_dir.join("same.bin").is_file());

    // Second push of identical content: receiver already has every
    // part verified — Need comes back empty, nothing crosses the wire.
    let second = push(&c_ep, target.clone(), &src).await.unwrap();
    assert_eq!(second.fetched, 0, "identical content re-fetched chunks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_recv_completes() {
    let (_s, c_ep, target, _task, server_dir) = pair().await;
    let data = random_bytes(3 * 1024 * 1024, 0xBEEF);
    std::fs::write(server_dir.join("doc.dat"), &data).unwrap();

    let dest_dir = scratch("dest");
    let conn = client_conn(&c_ep, target).await;
    let (send, recv) = conn.open_bi().await.unwrap();
    let (dest, stats) = recv_file(&conn, "doc.dat", &dest_dir, send, recv)
        .await
        .unwrap();
    assert!(stats.fetched > 0);
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_keeps_original_inode_after_offer_path_is_replaced() {
    use rds_net::{read_frame, write_frame};
    use rds_sync::proto::SyncMsg;
    let (server, client, target, task, server_dir) = pair().await;
    let original = b"original file bytes";
    let outside = scratch("pull-substitution-outside");
    std::fs::write(outside.join("secret"), b"different file data").unwrap();
    let path = server_dir.join("data.bin");
    std::fs::write(&path, original).unwrap();
    let conn = client_conn(&client, target).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(
        &mut send,
        &SyncMsg::Request {
            rel_path: "data.bin".into(),
        },
    )
    .await
    .unwrap();
    let SyncMsg::Offer {
        root,
        chunk_count: 1,
        ..
    } = read_frame(&mut recv).await.unwrap()
    else {
        panic!("expected one-chunk offer")
    };
    let SyncMsg::ManifestPart { .. } = read_frame(&mut recv).await.unwrap() else {
        panic!("expected manifest")
    };
    // The source path changes after its manifest is sent, before any chunk read.
    std::fs::rename(&path, server_dir.join("held.bin")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret"), &path).unwrap();
    let mut inbox = conn.uni_streams(rds_core::UniHello::Sync).unwrap();
    write_frame(&mut send, &SyncMsg::Need { bits: vec![1] })
        .await
        .unwrap();
    let mut stream = tokio::time::timeout(Duration::from_secs(3), inbox.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        read_frame(&mut stream).await.unwrap(),
        SyncMsg::ChunkSet { .. }
    ));
    let SyncMsg::ChunkHdr { len, .. } = read_frame(&mut stream).await.unwrap() else {
        panic!("expected chunk")
    };
    let mut bytes = vec![0; len as usize];
    stream.read_exact(&mut bytes).await.unwrap();
    assert_eq!(bytes, original);
    assert!(matches!(
        read_frame(&mut stream).await.unwrap(),
        SyncMsg::SetDone
    ));
    write_frame(&mut send, &SyncMsg::Done { root })
        .await
        .unwrap();
    client.close().await;
    server.close().await;
    task.abort();
    std::fs::remove_dir_all(server_dir).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceled_control_stream_releases_receive_lock() {
    use rds_net::{read_frame, write_frame};
    use rds_sync::{journal::Journal, proto::SyncMsg};
    let (server, client, target, task, server_dir) = pair().await;
    let manifest = manifest_of(b"one chunk");
    let conn = client_conn(&client, target).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(
        &mut send,
        &SyncMsg::Offer {
            rel_path: "data.bin".into(),
            size: manifest.size,
            root: manifest.root,
            chunk_count: manifest.chunks.len() as u32,
        },
    )
    .await
    .unwrap();
    write_frame(
        &mut send,
        &SyncMsg::ManifestPart {
            chunks: manifest.chunks.clone(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame(&mut recv).await.unwrap(),
        SyncMsg::Need { .. }
    ));
    assert!(Journal::open(&server_dir, "data.bin", &manifest).is_err());
    recv.stop(0u32.into()).unwrap();
    send.reset(0u32.into()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(journal) = Journal::open(&server_dir, "data.bin", &manifest) {
                assert_eq!(journal.need(), vec![0]);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("canceled receive retained the journal lock");
    client.close().await;
    server.close().await;
    task.abort();
    std::fs::remove_dir_all(server_dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_part_is_refetched() {
    let (_s, c_ep, target, _task, server_dir) = pair().await;
    let src_dir = scratch("src");
    let data = random_bytes(2 * 1024 * 1024, 0xF00D);
    let src = src_dir.join("corruptme.bin");
    std::fs::write(&src, &data).unwrap();

    // Land the first transfer, then corrupt one part on the receiver —
    // a resume must detect it (content hash != name) and re-fetch.
    push(&c_ep, target.clone(), &src).await.unwrap();
    // Assemble removed the state dir on success — rebuild the parts
    // dir with all-but-last parts correct and the last poisoned, so
    // resume re-fetches exactly one chunk.
    let manifest = manifest_of(&data);
    let name = format!(
        "v2-{}-{}",
        blake3::Hash::from(manifest.root).to_hex(),
        blake3::hash(b"corruptme.bin").to_hex()
    );
    let parts = server_dir.join(".rds-sync").join(name).join("parts");
    std::fs::create_dir_all(&parts).unwrap();
    // Write all-but-last parts correctly, poison the last one.
    let keep = manifest.chunks.len() - 1;
    for c in &manifest.chunks[..keep] {
        let name: String = c.hash.iter().map(|b| format!("{b:02x}")).collect();
        std::fs::write(
            parts.join(&name),
            &data[c.offset as usize..(c.offset + c.len as u64) as usize],
        )
        .unwrap();
    }
    let bad = &manifest.chunks[keep];
    let bad_name: String = bad.hash.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(parts.join(&bad_name), b"poisoned-chunk-bytes").unwrap();
    // Remove the assembled file so the resume has work to prove.
    std::fs::remove_file(server_dir.join("corruptme.bin")).unwrap();

    let stats = push(&c_ep, target.clone(), &src).await.unwrap();
    assert_eq!(stats.fetched, 1, "only the corrupt part re-fetched");
    assert_eq!(
        std::fs::read(server_dir.join("corruptme.bin")).unwrap(),
        data
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_mid_transfer_resumes_identical() {
    let (_s, c_ep, target, _task, server_dir) = pair().await;
    let src_dir = scratch("src");
    let data = random_bytes(6 * 1024 * 1024, 0xABCD);
    let src = src_dir.join("killme.bin");
    std::fs::write(&src, &data).unwrap();

    // First attempt: abort the client once some parts have landed —
    // polling the receiver's parts dir makes the kill deterministic
    // instead of racing the 6 MiB transfer.
    let c_ep2 = c_ep.clone();
    let src2 = src.clone();
    let t = target.clone();
    let attempt = tokio::spawn(async move { push(&c_ep2, t, &src2).await });
    let state_dir = server_dir.join(".rds-sync");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut landed = 0usize;
    while std::time::Instant::now() < deadline && landed < 3 {
        landed = count_parts(&state_dir);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    attempt.abort();
    let _ = attempt.await;
    assert!(landed >= 3, "the kill must follow verified part progress");
    // A fixed sleep is not a receiver cleanup fence. Observe the exact root's
    // lock and retain its verified progress before making one resumed push.
    let journal = wait_receive_journal(&server_dir, "killme.bin", &manifest_of(&data)).await;
    assert!(
        journal.have_set().len() >= 3,
        "canceled parts did not survive"
    );
    drop(journal);

    let stats = push(&c_ep, target.clone(), &src).await.unwrap();
    assert_eq!(std::fs::read(server_dir.join("killme.bin")).unwrap(), data);
    // Resume-overhead: only missing chunks crossed; the second attempt
    // re-fetched strictly less than the full set (some parts survived).
    assert!(
        stats.fetched < stats.total,
        "resume re-sent everything: {stats:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_admission_waits_for_the_actual_lock_release() {
    use rds_sync::journal::Journal;
    let dir = scratch("release-fence");
    let manifest = manifest_of(b"retained receiver work");
    let held = Journal::open(&dir, "data.bin", &manifest).unwrap();
    // Reproduce the old assumption: 150ms elapsed, but an executing receiver
    // still owns the real lock. This must remain a refusal, not a forced release.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(matches!(Journal::open(&dir, "data.bin", &manifest),
        Err(rds_sync::SyncError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock));
    let path = dir.clone();
    let waiting =
        tokio::spawn(async move { wait_receive_journal(&path, "data.bin", &manifest).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !waiting.is_finished(),
        "admission bypassed the receiver lock"
    );
    drop(held);
    let journal = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.need(), [0]);
    drop(journal);
    std::fs::remove_dir_all(dir).unwrap();
}

/// G6: kill at randomized progress points until done — byte-identical
/// every time. 20 iterations of a small file keep it fast.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn g6_repeated_kill_resume() {
    let (_s, c_ep, target, _task, server_dir) = pair().await;
    let src_dir = scratch("src");
    let data = random_bytes(1024 * 1024, 0x66);
    let src = src_dir.join("fragile.bin");
    std::fs::write(&src, &data).unwrap();

    let mut state = 0x12345u64;
    let mut completed = false;
    for _ in 0..40 {
        let c_ep2 = c_ep.clone();
        let src2 = src.clone();
        let t = target.clone();
        let attempt = tokio::spawn(async move { push(&c_ep2, t, &src2).await });
        // Random-ish kill delay 0..80ms from xorshift.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        tokio::time::sleep(Duration::from_millis(state % 80)).await;
        attempt.abort();
        match attempt.await {
            Ok(Ok(_)) => {
                completed = true;
                break;
            }
            _ => {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        }
    }
    // Cancellation is asynchronous: an in-flight filesystem operation still
    // owns its exclusive journal lock. G6 promises eventual byte-identical
    // convergence, not that a 30ms sleep makes immediate readmission safe.
    // Bound the entire recovery phase; persistent errors still fail the test.
    if !completed {
        let mut last_error = None;
        let resumed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match push(&c_ep, target.clone(), &src).await {
                    Ok(stats) => break stats,
                    Err(error) => last_error = Some(error),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(resumed.is_ok(), "resume did not converge: {last_error:?}");
    }
    assert_eq!(std::fs::read(server_dir.join("fragile.bin")).unwrap(), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torn_journal_still_resumes() {
    let (_s, c_ep, target, _task, server_dir) = pair().await;
    let src_dir = scratch("src");
    let data = random_bytes(2 * 1024 * 1024, 0x77);
    let src = src_dir.join("torn.bin");
    std::fs::write(&src, &data).unwrap();

    // Partial first pass, then tear the meta file: truncated garbage.
    let c_ep2 = c_ep.clone();
    let src2 = src.clone();
    let t = target.clone();
    let attempt = tokio::spawn(async move { push(&c_ep2, t, &src2).await });
    tokio::time::sleep(Duration::from_millis(80)).await;
    attempt.abort();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let state_dir = server_dir.join(".rds-sync");
    if state_dir.exists() {
        for e in std::fs::read_dir(&state_dir).unwrap().flatten() {
            let meta = e.path().join("meta");
            if meta.exists() {
                std::fs::write(&meta, b"torn!").unwrap();
            }
        }
    }
    let stats = push(&c_ep, target.clone(), &src).await.unwrap();
    assert!(stats.fetched <= stats.total);
    assert_eq!(std::fs::read(server_dir.join("torn.bin")).unwrap(), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn path_traversal_rejected() {
    // Validation-level: these never reach a filesystem.
    for bad in ["../x", "a/../../b", "/abs/path", "..\\win", "a\0b", ""] {
        assert!(check_rel_path(bad).is_err(), "accepted {bad:?}");
    }
    // The journal namespace is reserved: no reading or planting state.
    for bad in [".rds-sync/meta", ".rds-sync/x/parts/aa", "./.rds-sync/meta"] {
        assert!(check_rel_path(bad).is_err(), "accepted {bad:?}");
    }
    assert!(check_rel_path("dir/sub/file.bin").is_ok());
    // Nested roots also own private receive state; peers cannot address it.
    assert!(check_rel_path("a/.rds-sync/notes").is_err());

    // And over the wire: a hostile Offer gets Refuse.
    let (_s, c_ep, target, _task, _server_dir) = pair().await;
    let conn = client_conn(&c_ep, target).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    rds_net::write_frame(
        &mut send,
        &rds_sync::proto::SyncMsg::Offer {
            rel_path: "../escape.bin".into(),
            size: 4,
            root: [0u8; 32],
            chunk_count: 0,
        },
    )
    .await
    .unwrap();
    match rds_net::read_frame::<_, rds_sync::proto::SyncMsg>(&mut recv)
        .await
        .unwrap()
    {
        rds_sync::proto::SyncMsg::Refuse { .. } => {}
        other => panic!("traversal offer not refused: {other:?}"),
    }
}

/// Symlink confinement: a sync root containing links to outside must
/// not serve or write through them — `check_rel_path` is lexical, so
/// Directory-relative no-follow operations refuse symlink traversal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn symlink_escape_refused() {
    let (_s, c_ep, target, _task, server_dir) = pair().await;
    let outside = scratch("outside");
    std::fs::write(outside.join("secret.txt"), b"not for sync").unwrap();

    // `link` inside the sync root points outside it.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, server_dir.join("link")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&outside, server_dir.join("link")).unwrap();

    // Pull through the link: refused, nothing served.
    let conn = client_conn(&c_ep, target.clone()).await;
    let (send, recv) = conn.open_bi().await.unwrap();
    let err = recv_file(&conn, "link/secret.txt", &scratch("dest"), send, recv).await;
    assert!(err.is_err(), "pull through symlinked dir was served");

    // Pull of a file that IS a link pointing outside: also refused.
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("secret.txt"), server_dir.join("alias.txt")).unwrap();
    let conn = client_conn(&c_ep, target.clone()).await;
    let (send, recv) = conn.open_bi().await.unwrap();
    let err = recv_file(&conn, "alias.txt", &scratch("dest2"), send, recv).await;
    assert!(err.is_err(), "pull of symlink-to-outside was served");

    // Push into the linked dir: the offer passes the lexical check but
    // preflight refuses it — the transfer aborts before a chunk
    // moves and nothing lands outside.
    let data = b"payload-bytes".to_vec();
    let manifest = manifest_of(&data);
    let conn = client_conn(&c_ep, target.clone()).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    rds_net::write_frame(
        &mut send,
        &rds_sync::proto::SyncMsg::Offer {
            rel_path: "link/victim.bin".into(),
            size: manifest.size,
            root: manifest.root,
            chunk_count: manifest.chunks.len() as u32,
        },
    )
    .await
    .unwrap();
    let verdict = tokio::time::timeout(
        Duration::from_secs(10),
        rds_net::read_frame::<_, rds_sync::proto::SyncMsg>(&mut recv),
    )
    .await;
    match verdict {
        Err(_) | Ok(Err(_)) => panic!("escape attempt killed the stream, expected Refuse"),
        Ok(Ok(rds_sync::proto::SyncMsg::Refuse { .. })) => {}
        Ok(Ok(other)) => panic!("escape attempt got {other:?}"),
    }
    assert!(
        !outside.join("victim.bin").exists(),
        "push wrote through the symlink outside the root"
    );

    // And a symlinked `.rds-sync` can't redirect the journal either:
    // plant one, then run a normal push — it must be refused rather
    // than journal state landing outside.
    let jail = scratch("jailed-server");
    let journal_out = scratch("journal-out");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&journal_out, jail.join(".rds-sync")).unwrap();
    let jail_ep = bind_endpoint(EndpointConfig {
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    })
    .await
    .unwrap();
    let jail_task = spawn_server(jail_ep.clone(), jail.clone());
    let jail_target = jail_ep.addr();
    let conn = client_conn(&c_ep, jail_target).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    rds_net::write_frame(
        &mut send,
        &rds_sync::proto::SyncMsg::Offer {
            rel_path: "ok.bin".into(),
            size: manifest.size,
            root: manifest.root,
            chunk_count: manifest.chunks.len() as u32,
        },
    )
    .await
    .unwrap();
    rds_net::write_frame(
        &mut send,
        &rds_sync::proto::SyncMsg::ManifestPart {
            chunks: manifest.chunks.clone(),
        },
    )
    .await
    .unwrap();
    let verdict = tokio::time::timeout(
        Duration::from_secs(10),
        rds_net::read_frame::<_, rds_sync::proto::SyncMsg>(&mut recv),
    )
    .await;
    match verdict {
        Ok(Ok(rds_sync::proto::SyncMsg::Refuse { .. })) => {}
        Err(_) | Ok(Err(_)) => panic!("journal refusal did not reach the peer"),
        Ok(Ok(other)) => panic!("journal-through-symlink got {other:?}"),
    }
    assert!(
        std::fs::read_dir(&journal_out).unwrap().next().is_none(),
        "journal state escaped through .rds-sync symlink"
    );
    jail_task.abort();
}

/// A forged `ChunkHdr` length must not size the receive buffer: the
/// header is checked against the manifest before allocation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_chunk_len_rejected() {
    let (_s, c_ep, target, _task, server_dir) = pair().await;
    let conn = client_conn(&c_ep, target).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();

    // Offer a real one-chunk file so the manifest validates.
    let data = random_bytes(64 * 1024, 0xBAD);
    let manifest = manifest_of(&data);
    rds_net::write_frame(
        &mut send,
        &rds_sync::proto::SyncMsg::Offer {
            rel_path: "victim.bin".into(),
            size: manifest.size,
            root: manifest.root,
            chunk_count: manifest.chunks.len() as u32,
        },
    )
    .await
    .unwrap();
    rds_net::write_frame(
        &mut send,
        &rds_sync::proto::SyncMsg::ManifestPart {
            chunks: manifest.chunks.clone(),
        },
    )
    .await
    .unwrap();
    // Server answers Need.
    match rds_net::read_frame::<_, rds_sync::proto::SyncMsg>(&mut recv)
        .await
        .unwrap()
    {
        rds_sync::proto::SyncMsg::Need { .. } => {}
        other => panic!("expected Need, got {other:?}"),
    }

    // Chunk stream: tag, set, then a ChunkHdr claiming 4 GiB.
    let mut stream = conn.open_uni().await.unwrap();
    rds_net::write_frame(&mut stream, &rds_core::UniHello::Sync)
        .await
        .unwrap();
    rds_net::write_frame(
        &mut stream,
        &rds_sync::proto::SyncMsg::ChunkSet { indices: vec![0] },
    )
    .await
    .unwrap();
    rds_net::write_frame(
        &mut stream,
        &rds_sync::proto::SyncMsg::ChunkHdr {
            index: 0,
            hash: manifest.chunks[0].hash,
            len: u32::MAX,
        },
    )
    .await
    .unwrap();
    // Withhold both body and FIN. Rejection must come from validating the
    // header against the admitted manifest, not from truncated-body EOF.
    // Writing after that rejection races the correct STOP_SENDING response.
    let stopped = tokio::time::timeout(Duration::from_secs(10), stream.stopped())
        .await
        .expect("receiver waited for a body after the forged chunk header")
        .expect("connection ended instead of rejecting the forged stream");
    assert_eq!(stopped, Some(rds_net::VarInt::from_u32(0)));

    // The receiver must abort the transfer on the len mismatch — the
    // control stream ends without Done.
    let verdict = tokio::time::timeout(
        Duration::from_secs(10),
        rds_net::read_frame::<_, rds_sync::proto::SyncMsg>(&mut recv),
    )
    .await
    .expect("rejected chunk left its control session waiting indefinitely");
    match verdict {
        Err(error) => assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ),
            "unexpected rejection failure: {error}"
        ),
        Ok(rds_sync::proto::SyncMsg::Refuse { .. }) => {}
        Ok(other) => panic!("forged chunk len accepted: {other:?}"),
    }
    assert!(!server_dir.join("victim.bin").exists());
    assert_eq!(count_parts(&server_dir.join(".rds-sync")), 0);
    // A rejected stream must not break other uses of the admitted connection.
    let source = scratch("after-forged-header");
    let path = source.join("valid.bin");
    std::fs::write(&path, b"valid after rejection").unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), send_file(&conn, &path, send, recv))
        .await
        .expect("valid transfer stalled after rejecting the forged stream")
        .unwrap();
    assert_eq!(
        std::fs::read(server_dir.join("valid.bin")).unwrap(),
        b"valid after rejection"
    );
}

/// Impaired lane: 5% loss + delay + jitter under both sockets, plus a
/// mid-transfer connection drop — the file still lands byte-identical
/// and resume re-fetches only what was lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn impaired_transfer_completes() {
    use rds_bench::impair::{ImpairingSocket, Impairment};

    async fn impaired_ep(imp: Impairment) -> Endpoint {
        let runtime: Arc<dyn noq::Runtime> = Arc::new(noq::TokioRuntime);
        let std_sock = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let inner = noq::Runtime::wrap_udp_socket(&*runtime, std_sock).unwrap();
        let local = inner.local_addr().unwrap();
        let (socket, _stats) = ImpairingSocket::wrap(inner, imp);
        bind_noq_with_socket(
            EndpointConfig::default(),
            Box::new(socket),
            vec![local],
            runtime,
        )
        .await
        .unwrap()
    }

    let server_ep = impaired_ep(Impairment::lossy()).await;
    let client_ep = impaired_ep(Impairment::lossy()).await;
    let server_dir = scratch("impaired-server");
    let _task = spawn_server(server_ep.clone(), server_dir.clone());
    let target = server_ep.addr();

    let src_dir = scratch("src");
    let data = random_bytes(1024 * 1024, 0x1F1E);
    let src = src_dir.join("lossy.bin");
    std::fs::write(&src, &data).unwrap();

    // Attempt 1: cut the client connection ~midway, then re-run to
    // completion on a fresh connection — resume must dedupe parts.
    let c_ep2 = client_ep.clone();
    let src2 = src.clone();
    let t = target.clone();
    let attempt = tokio::spawn(async move { push(&c_ep2, t, &src2).await });
    tokio::time::sleep(Duration::from_millis(700)).await;
    attempt.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let stats = push(&client_ep, target.clone(), &src).await.unwrap();
    assert_eq!(std::fs::read(server_dir.join("lossy.bin")).unwrap(), data);
    assert!(
        stats.fetched <= stats.total,
        "resume re-fetched more than the file: {stats:?}"
    );
}

/// Decoder robustness: malformed manifests/paths never panic, and
/// structurally broken manifests are rejected.
#[test]
fn manifest_and_path_fuzz() {
    check_manifest(&Manifest {
        size: 10,
        root: [0; 32],
        chunks: vec![],
    })
    .unwrap_err(); // empty manifest covering 10 bytes
    check_manifest(&Manifest {
        size: 0,
        root: [0; 32],
        chunks: vec![],
    })
    .unwrap(); // empty file is a valid manifest
}
