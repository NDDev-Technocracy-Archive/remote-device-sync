//! Fine capabilities enforced by a real agent over loopback QUIC.
use std::{path::PathBuf, sync::Arc, time::Duration};

use ed25519_dalek::SigningKey;
use rds_agent::{Agent, AgentPolicy};
use rds_core::{ServiceKind, grant::Grant};
use rds_net::{Backend, Connection, Endpoint, EndpointConfig, RecvStream, SendStream};
use rds_net::{read_frame, write_frame};
use rds_sync::{engine, proto::SyncMsg};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rds-grant-scopes-{}-{now}-{seq}",
            std::process::id()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Runner(tokio::task::JoinHandle<()>);
impl Drop for Runner {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn endpoint(backend: Backend) -> Endpoint {
    rds_net::bind_endpoint(EndpointConfig {
        backend,
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    })
    .await
    .unwrap()
}

// A peer can see the previous transfer's final message before the serving task
// has dropped its guard. Retry only that explicit transient refusal, bounded.
async fn next_sync(conn: &Connection) -> (SendStream, RecvStream) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match rds_client::open_sync(conn).await {
                Ok(streams) => return streams,
                Err(e) if e.to_string().contains("sync session already active") => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(e) => panic!("unexpected sync admission failure: {e}"),
            }
        }
    })
    .await
    .expect("sync slot was not released")
}

async fn directional_sync(backend: Backend) {
    let root = Scratch::new();
    let local = Scratch::new();
    let payload = b"synthetic scope regression payload";
    std::fs::write(root.0.join("download.bin"), payload).unwrap();
    std::fs::write(local.0.join("upload.bin"), payload).unwrap();
    let client = endpoint(backend).await;
    let server = endpoint(backend).await;
    let issuer = SigningKey::from_bytes(&[73; 32]);
    let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
    policy.allow.insert(client.id());
    policy.issuers.insert(issuer.verifying_key().to_bytes());
    policy.sync_dir = Some(root.0.clone());
    policy.use_local_revocations();
    let agent = Arc::new(Agent::new(server.clone(), policy));
    let runner = agent.clone();
    let mut task = Runner(tokio::spawn(async move {
        runner.run().await.unwrap();
    }));
    for (index, scope) in [
        ServiceKind::SyncRead,
        ServiceKind::SyncWrite,
        ServiceKind::Sync,
    ]
    .into_iter()
    .enumerate()
    {
        let token = Grant::issue(
            &issuer,
            *client.id().as_bytes(),
            *server.id().as_bytes(),
            [index as u8; 16],
            vec![ServiceKind::Ping, scope],
            Duration::from_secs(120),
            Default::default(),
        );
        let conn = rds_client::connect_authorized(&client, server.addr(), &token)
            .await
            .unwrap();
        if scope != ServiceKind::Sync {
            // Check before even parsing a path: no existence/path detail leaks,
            // no manifest request, journal creation, or destination mutation.
            for path in ["download.bin", "missing/sub/file", "../escape"] {
                let (mut send, mut recv) = next_sync(&conn).await;
                let message = if scope == ServiceKind::SyncRead {
                    SyncMsg::Offer {
                        rel_path: path.into(),
                        size: 0,
                        root: [0; 32],
                        chunk_count: 0,
                    }
                } else {
                    SyncMsg::Request {
                        rel_path: path.into(),
                    }
                };
                write_frame(&mut send, &message).await.unwrap();
                let answer = tokio::time::timeout(
                    Duration::from_secs(3),
                    read_frame::<_, SyncMsg>(&mut recv),
                )
                .await
                .unwrap()
                .unwrap();
                let SyncMsg::Refuse { reason } = answer else {
                    panic!("accepted forbidden operation: {answer:?}");
                };
                assert_eq!(
                    reason,
                    if scope == ServiceKind::SyncRead {
                        "sync write not granted"
                    } else {
                        "sync read not granted"
                    }
                );
                assert_eq!(std::fs::read(root.0.join("download.bin")).unwrap(), payload);
                assert!(!root.0.join(".rds-sync").exists());
                assert!(!root.0.join("missing").exists());
            }
        }
        if scope != ServiceKind::SyncWrite {
            let (send, recv) = next_sync(&conn).await;
            let destination = Scratch::new();
            engine::recv_file_with_timeout(
                &conn,
                "download.bin",
                &destination.0,
                send,
                recv,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
            assert_eq!(
                std::fs::read(destination.0.join("download.bin")).unwrap(),
                payload
            );
        }
        if scope != ServiceKind::SyncRead {
            let (send, recv) = next_sync(&conn).await;
            engine::send_file_with_timeout(
                &conn,
                &local.0.join("upload.bin"),
                send,
                recv,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read(root.0.join("upload.bin")).unwrap(), payload);
        }
        // An abandoned stream must release the slot without a connection reset.
        let (mut held_send, mut held_recv) = next_sync(&conn).await;
        for _ in 0..2 {
            assert!(
                rds_client::open_sync(&conn)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("sync session already active")
            );
        }
        held_send.reset(0u32.into()).unwrap();
        held_recv.stop(0u32.into()).unwrap();
        let (mut send, _) = next_sync(&conn).await;
        send.reset(0u32.into()).unwrap();
        rds_client::ping(&conn, 99).await.unwrap();
        conn.close(0u32.into(), b"done");
    }
    // A control modifier by itself cannot open a desktop, or any sync stream.
    let token = Grant::issue(
        &issuer,
        *client.id().as_bytes(),
        *server.id().as_bytes(),
        [9; 16],
        vec![ServiceKind::DesktopControl, ServiceKind::Ping],
        Duration::from_secs(120),
        Default::default(),
    );
    let conn = rds_client::connect_authorized(&client, server.addr(), &token)
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(
        &mut send,
        &rds_core::StreamHello::Desktop(rds_core::DesktopHello {
            display: 0,
            max_fps: 30,
            codec: rds_core::Codec::H264,
            input_acks: true,
        }),
    )
    .await
    .unwrap();
    let ack = read_frame::<_, rds_core::HelloAck>(&mut recv)
        .await
        .unwrap();
    assert!(
        matches!(ack, rds_core::HelloAck::Error { message } if message == "service Desktop not granted")
    );
    // The per-session greeting must not bypass the same scope check.
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(
        &mut send,
        &rds_core::StreamHello::DesktopV2 {
            session: [7; 16],
            hello: rds_core::DesktopHello {
                display: 0,
                max_fps: 30,
                codec: rds_core::Codec::H264,
                input_acks: true,
            },
        },
    )
    .await
    .unwrap();
    let ack = read_frame::<_, rds_core::HelloAck>(&mut recv)
        .await
        .unwrap();
    assert!(
        matches!(ack, rds_core::HelloAck::Error { message } if message == "service Desktop not granted")
    );
    assert!(
        rds_client::open_sync(&conn)
            .await
            .unwrap_err()
            .to_string()
            .contains("service Sync not granted")
    );
    rds_client::ping(&conn, 100).await.unwrap();
    client.close().await;
    server.close().await;
    tokio::time::timeout(Duration::from_secs(5), &mut task.0)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fine_scopes_on_iroh() {
    directional_sync(Backend::Iroh).await;
}

#[cfg(feature = "transport-noq")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fine_scopes_on_owned_transport() {
    directional_sync(Backend::Noq).await;
}
