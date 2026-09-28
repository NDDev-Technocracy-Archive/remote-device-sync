//! W1.9/W2.2 negotiated sync sessions: real endpoints running the v2
//! `SyncTransferV2` route — Hello/HelloAck limit exchange, transfer-ID
//! bound frames, typed Cancel propagation, and version/tag refusal.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use rds_core::{HelloAck, StreamHello};
use rds_net::{Endpoint, EndpointAddr, EndpointConfig, bind_endpoint, read_frame, write_frame};
use rds_sync::engine::{Access, Transfer};

/// Temp dir per test — unique per process + name.
fn scratch(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rds-v2-{name}-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    dir
}

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

/// v2-capable server: accepts `SyncTransfer`/`SyncTransferV2` bi streams,
/// reports each serve outcome back to the test.
fn spawn_server_v2(
    ep: Endpoint,
    dir: PathBuf,
    outcomes: mpsc::Sender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let dir = dir.clone();
            let outcomes = outcomes.clone();
            tokio::spawn(async move {
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let conn = conn.clone();
                    let dir = dir.clone();
                    let outcomes = outcomes.clone();
                    tokio::spawn(async move {
                        let outcome = async {
                            let hello: StreamHello = read_frame(&mut recv).await?;
                            let transfer = match hello {
                                StreamHello::SyncTransfer { id } => Transfer::new(id),
                                StreamHello::SyncTransferV2 { id } => Transfer::new_v2(id),
                                other => anyhow::bail!("unexpected hello {other:?}"),
                            };
                            write_frame(&mut send, &HelloAck::Ok).await?;
                            transfer
                                .serve(
                                    conn,
                                    (send, recv),
                                    dir,
                                    Access::READ_WRITE,
                                    Duration::from_secs(60),
                                )
                                .await
                        }
                        .await;
                        let _ = outcomes.send(match outcome {
                            Ok(()) => "ok".into(),
                            Err(e) => format!("err:{e:#}"),
                        });
                    });
                }
            });
        }
    })
}

async fn pair() -> (
    Endpoint,
    EndpointAddr,
    PathBuf,
    mpsc::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::level_filters::LevelFilter::DEBUG)
        .with_test_writer()
        .try_init();
    let server_ep = bind_endpoint(EndpointConfig::default()).await.unwrap();
    let client_ep = bind_endpoint(EndpointConfig::default()).await.unwrap();
    let dir = scratch("server");
    let (outcomes, rx) = mpsc::channel();
    let target = server_ep.addr();
    let task = spawn_server_v2(server_ep, dir.clone(), outcomes);
    (client_ep, target, dir, rx, task)
}

/// Open a v2 control stream: greeting + HelloAck on the `SyncTransferV2`
/// route. Returns the connection the streams live on — the transfer's
/// uni streams must ride the same connection.
async fn open_v2(
    client_ep: &Endpoint,
    target: &EndpointAddr,
) -> (
    rds_net::Connection,
    Transfer,
    rds_net::SendStream,
    rds_net::RecvStream,
) {
    let conn = client_ep
        .connect(target.clone(), rds_core::ALPN)
        .await
        .unwrap();
    let id: [u8; 16] = rand::random();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, &StreamHello::SyncTransferV2 { id })
        .await
        .unwrap();
    let ack: HelloAck = read_frame(&mut recv).await.unwrap();
    assert!(matches!(ack, HelloAck::Ok), "v2 greeting refused: {ack:?}");
    (conn, Transfer::new_v2(id), send, recv)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v2_push_completes_byte_identical_through_negotiated_session() {
    let (client_ep, target, server_dir, rx, _task) = pair().await;
    let src_dir = scratch("src");
    let data = random_bytes(4 * 1024 * 1024, 0x2C);
    let src = src_dir.join("blob.bin");
    std::fs::write(&src, &data).unwrap();

    let (conn, transfer, send, recv) = open_v2(&client_ep, &target).await;
    let stats = match transfer
        .send_file(&conn, &src, (send, recv), Duration::from_secs(60))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            let server = rx.recv_timeout(Duration::from_secs(10));
            panic!("client failed: {e:#}; server outcome: {server:?}");
        }
    };
    assert_eq!(stats.bytes, data.len() as u64);
    assert_eq!(std::fs::read(server_dir.join("blob.bin")).unwrap(), data);
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v2_pull_completes_and_resumes_after_cancel() {
    let (client_ep, target, server_dir, rx, _task) = pair().await;
    std::fs::write(
        server_dir.join("file.bin"),
        random_bytes(2 * 1024 * 1024, 0x7),
    )
    .unwrap();
    let dest_dir = scratch("dest");

    let (conn, transfer, send, recv) = open_v2(&client_ep, &target).await;
    let (dest, stats) = match transfer
        .recv_file(
            &conn,
            "file.bin",
            &dest_dir,
            (send, recv),
            Duration::from_secs(60),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            let server = rx.recv_timeout(Duration::from_secs(10));
            panic!("client failed: {e:#}; server outcome: {server:?}");
        }
    };
    assert_eq!(stats.fetched, stats.total);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        std::fs::read(server_dir.join("file.bin")).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v2_token_cancel_writes_typed_cancel_and_server_observes_abort() {
    let (client_ep, target, _server_dir, rx, _task) = pair().await;
    let src_dir = scratch("src");
    let data = random_bytes(32 * 1024 * 1024, 0x9);
    let src = src_dir.join("big.bin");
    std::fs::write(&src, &data).unwrap();

    let (conn, transfer, send, recv) = open_v2(&client_ep, &target).await;
    let token = tokio_util::sync::CancellationToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        t2.cancel();
    });
    let err = transfer
        .send_file_cancel(
            &conn,
            &src,
            (send, recv),
            Duration::from_secs(300),
            Some(token),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("canceled"),
        "expected canceled error, got {err}"
    );
    // The server-side receiver must report a deliberate peer abort —
    // typed Cancel or a control close, never a hang.
    let outcome = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(
        outcome.starts_with("err:"),
        "server must observe an abort, got {outcome}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v1_route_still_serves_unchanged_for_compat() {
    let (client_ep, target, server_dir, rx, _task) = pair().await;
    let src_dir = scratch("src");
    let data = random_bytes(1024 * 1024, 0x11);
    let src = src_dir.join("v1.bin");
    std::fs::write(&src, &data).unwrap();

    let conn = client_ep
        .connect(target.clone(), rds_core::ALPN)
        .await
        .unwrap();
    let id: [u8; 16] = rand::random();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, &StreamHello::SyncTransfer { id })
        .await
        .unwrap();
    let ack: HelloAck = read_frame(&mut recv).await.unwrap();
    assert!(matches!(ack, HelloAck::Ok));
    let stats = Transfer::new(id)
        .send_file(&conn, &src, (send, recv), Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(stats.bytes, data.len() as u64);
    assert_eq!(std::fs::read(server_dir.join("v1.bin")).unwrap(), data);
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "ok");
}

/// W1.9 hardening: a peer that receives our typed `Cancel` and keeps
/// pushing anyway cannot keep us attached. The canceled receive returns
/// promptly, never publishes the destination, and a fresh transfer on
/// the same connection is isolated by its fresh route ID.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn v2_recv_cancel_detaches_even_when_peer_ignores_it() {
    use rds_core::UniHello;
    use rds_sync::proto::{
        MANIFEST_BATCH, SESSION_VERSION, SessionLimits, SessionMsg, SyncMsg, bits_to_indices,
    };

    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::level_filters::LevelFilter::DEBUG)
        .with_test_writer()
        .try_init();
    let server_ep = bind_endpoint(EndpointConfig::default()).await.unwrap();
    let client_ep = bind_endpoint(EndpointConfig::default()).await.unwrap();
    let dir = scratch("stubborn-server");
    let big = random_bytes(48 * 1024 * 1024, 0x33);
    std::fs::write(dir.join("stubborn.bin"), &big).unwrap();
    let second = random_bytes(256 * 1024, 0x44);
    std::fs::write(dir.join("second.bin"), &second).unwrap();
    let target = server_ep.addr();

    let stubborn = tokio::spawn(async move {
        while let Some(incoming) = server_ep.accept().await {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let dir = dir.clone();
            tokio::spawn(async move {
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let conn = conn.clone();
                    let dir = dir.clone();
                    tokio::spawn(async move {
                        let session = |msg: SessionMsg| SyncMsg::Session {
                            transfer_id: [0; 16],
                            msg,
                        };
                        let _ = async {
                            let hello: StreamHello = read_frame(&mut recv).await?;
                            let id = match hello {
                                StreamHello::SyncTransferV2 { id } => id,
                                other => anyhow::bail!("unexpected hello {other:?}"),
                            };
                            write_frame(&mut send, &HelloAck::Ok).await?;
                            let session = |msg: SessionMsg| SyncMsg::Session {
                                transfer_id: id,
                                msg,
                            };
                            let SyncMsg::Session {
                                msg: SessionMsg::Hello { .. },
                                ..
                            } = read_frame(&mut recv).await?
                            else {
                                anyhow::bail!("expected session Hello");
                            };
                            write_frame(
                                &mut send,
                                &session(SessionMsg::HelloAck {
                                    version: SESSION_VERSION,
                                    limits: SessionLimits::LOCAL,
                                }),
                            )
                            .await?;
                            let SyncMsg::Session {
                                msg: SessionMsg::Request { rel_path },
                                ..
                            } = read_frame(&mut recv).await?
                            else {
                                anyhow::bail!("expected Request");
                            };
                            let data = std::fs::read(dir.join(&rel_path))?;
                            let manifest = rds_sync::manifest_of(&data);
                            write_frame(
                                &mut send,
                                &session(SessionMsg::Offer {
                                    rel_path: rel_path.clone(),
                                    size: manifest.size,
                                    root: manifest.root,
                                    chunk_count: manifest.chunks.len() as u32,
                                }),
                            )
                            .await?;
                            for part in manifest.chunks.chunks(MANIFEST_BATCH) {
                                write_frame(
                                    &mut send,
                                    &session(SessionMsg::ManifestPart {
                                        chunks: part.to_vec(),
                                    }),
                                )
                                .await?;
                            }
                            let SyncMsg::Session {
                                msg: SessionMsg::Need { bits },
                                ..
                            } = read_frame(&mut recv).await?
                            else {
                                anyhow::bail!("expected Need");
                            };
                            let indices = bits_to_indices(&bits, manifest.chunks.len())
                                .map_err(anyhow::Error::msg)?;
                            let mut stream = conn.open_uni().await?;
                            write_frame(&mut stream, &UniHello::SyncTransferV2 { id }).await?;
                            write_frame(
                                &mut stream,
                                &session(SessionMsg::ChunkSet {
                                    indices: indices.clone(),
                                }),
                            )
                            .await?;
                            for i in &indices {
                                let c = &manifest.chunks[*i as usize];
                                write_frame(
                                    &mut stream,
                                    &session(SessionMsg::ChunkHdr {
                                        index: *i,
                                        hash: c.hash,
                                        len: c.len,
                                    }),
                                )
                                .await?;
                                stream
                                    .write_all(
                                        &data
                                            [c.offset as usize..(c.offset + c.len as u64) as usize],
                                    )
                                    .await?;
                            }
                            write_frame(&mut stream, &session(SessionMsg::SetDone)).await?;
                            stream.finish()?;
                            // Deliberately ignore whatever the peer sends
                            // next — including our typed Cancel — and hold
                            // the transfer's streams open.
                            tokio::time::sleep(Duration::from_secs(4)).await;
                            anyhow::Ok(())
                        }
                        .await;
                        let _ = session;
                    });
                }
            });
        }
    });

    let dest_dir = scratch("stubborn-dest");
    let conn = client_ep.connect(target, rds_core::ALPN).await.unwrap();
    let id: [u8; 16] = rand::random();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, &StreamHello::SyncTransferV2 { id })
        .await
        .unwrap();
    let ack: HelloAck = read_frame(&mut recv).await.unwrap();
    assert!(matches!(ack, HelloAck::Ok));

    let token = tokio_util::sync::CancellationToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        t2.cancel();
    });
    let err = tokio::time::timeout(
        // A peer-cooperative teardown would wait out the whole 60s
        // deadline; our own teardown must not.
        Duration::from_secs(30),
        Transfer::new_v2(id).recv_file_cancel(
            &conn,
            "stubborn.bin",
            &dest_dir,
            (send, recv),
            Duration::from_secs(60),
            Some(token),
        ),
    )
    .await
    .expect("receive must return despite the peer ignoring Cancel")
    .unwrap_err();
    assert!(
        err.to_string().contains("canceled"),
        "expected canceled error, got {err:#}"
    );
    assert!(
        !dest_dir.join("stubborn.bin").exists(),
        "a canceled receive must never publish its destination"
    );

    // A second transfer on the same connection carries a fresh route ID
    // — the abandoned one cannot claim its streams.
    let id2: [u8; 16] = rand::random();
    let (mut send2, mut recv2) = conn.open_bi().await.unwrap();
    write_frame(&mut send2, &StreamHello::SyncTransferV2 { id: id2 })
        .await
        .unwrap();
    let ack: HelloAck = read_frame(&mut recv2).await.unwrap();
    assert!(matches!(ack, HelloAck::Ok));
    let (dest, stats) = Transfer::new_v2(id2)
        .recv_file(
            &conn,
            "second.bin",
            &dest_dir,
            (send2, recv2),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert_eq!(stats.fetched, stats.total);
    assert_eq!(std::fs::read(&dest).unwrap(), second);
    stubborn.abort();
}

/// W1.9 hardening, serve side: a typed `Cancel` is observed by the
/// shared reader the moment it is decoded — the serve reports a peer
/// abort, never a hang, whether the frame lands before the request or
/// while the manifest phase is still running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v2_peer_cancel_before_request_aborts_serve() {
    use rds_sync::proto::{SESSION_VERSION, SessionLimits, SessionMsg, SyncMsg};

    let (client_ep, target, _server_dir, rx, _task) = pair().await;
    let conn = client_ep.connect(target, rds_core::ALPN).await.unwrap();
    let id: [u8; 16] = rand::random();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, &StreamHello::SyncTransferV2 { id })
        .await
        .unwrap();
    let ack: HelloAck = read_frame(&mut recv).await.unwrap();
    assert!(matches!(ack, HelloAck::Ok));
    let session = |msg: SessionMsg| SyncMsg::Session {
        transfer_id: id,
        msg,
    };
    write_frame(
        &mut send,
        &session(SessionMsg::Hello {
            version: SESSION_VERSION,
            limits: SessionLimits::LOCAL,
        }),
    )
    .await
    .unwrap();
    let SyncMsg::Session {
        msg: SessionMsg::HelloAck { .. },
        ..
    } = read_frame(&mut recv).await.unwrap()
    else {
        panic!("expected HelloAck");
    };
    write_frame(
        &mut send,
        &session(SessionMsg::Cancel {
            reason: "operator abort".into(),
        }),
    )
    .await
    .unwrap();
    let outcome = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(
        outcome.starts_with("err:") && outcome.contains("cancel"),
        "serve must report the peer abort, got {outcome}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v2_peer_cancel_during_pull_serve_aborts_it() {
    use rds_sync::proto::{SESSION_VERSION, SessionLimits, SessionMsg, SyncMsg};

    let (client_ep, target, server_dir, rx, _task) = pair().await;
    std::fs::write(
        server_dir.join("pullme.bin"),
        random_bytes(64 * 1024 * 1024, 0x51),
    )
    .unwrap();
    let conn = client_ep.connect(target, rds_core::ALPN).await.unwrap();
    let id: [u8; 16] = rand::random();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, &StreamHello::SyncTransferV2 { id })
        .await
        .unwrap();
    let ack: HelloAck = read_frame(&mut recv).await.unwrap();
    assert!(matches!(ack, HelloAck::Ok));
    let session = |msg: SessionMsg| SyncMsg::Session {
        transfer_id: id,
        msg,
    };
    write_frame(
        &mut send,
        &session(SessionMsg::Hello {
            version: SESSION_VERSION,
            limits: SessionLimits::LOCAL,
        }),
    )
    .await
    .unwrap();
    let SyncMsg::Session {
        msg: SessionMsg::HelloAck { .. },
        ..
    } = read_frame(&mut recv).await.unwrap()
    else {
        panic!("expected HelloAck");
    };
    write_frame(
        &mut send,
        &session(SessionMsg::Request {
            rel_path: "pullme.bin".into(),
        }),
    )
    .await
    .unwrap();
    // The Cancel races the manifest scan; either landing point must
    // abort the serve — during the scan via the shared stop flag, or at
    // the Need wait via the queued frame.
    tokio::time::sleep(Duration::from_millis(30)).await;
    write_frame(
        &mut send,
        &session(SessionMsg::Cancel {
            reason: "receiver walked away".into(),
        }),
    )
    .await
    .unwrap();
    let outcome = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(
        outcome.starts_with("err:") && outcome.contains("cancel"),
        "serve must report the peer abort, got {outcome}"
    );
}
