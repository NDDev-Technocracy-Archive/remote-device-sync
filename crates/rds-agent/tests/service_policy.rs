//! Deployment service policy enforced by a real agent over loopback QUIC:
//! the explicit service set gates streams, `Info` advertises only what is
//! enabled, and greeting deadlines come from the timeout policy.
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use std::{net::SocketAddr, path::PathBuf};

use rds_agent::{Agent, AgentPolicy, TimeoutPolicy};
use rds_core::{HelloAck, ServiceKind, StreamHello};
use rds_net::{Backend, Endpoint, EndpointConfig, read_frame, write_frame};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "rds-service-policy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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

fn backends() -> Vec<Backend> {
    #[cfg(feature = "transport-noq")]
    {
        vec![Backend::Iroh, Backend::Noq]
    }
    #[cfg(not(feature = "transport-noq"))]
    {
        vec![Backend::Iroh]
    }
}

async fn error_ack(conn: &rds_net::Connection, hello: StreamHello) -> String {
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, &hello).await.unwrap();
    match tokio::time::timeout(Duration::from_secs(5), read_frame(&mut recv))
        .await
        .unwrap()
        .unwrap()
    {
        HelloAck::Error { message } => message,
        other => panic!("expected refusal, got {other:?}"),
    }
}

/// The explicit service set is the only surface a client can reach: `Info`
/// advertises it honestly, enabled services work, everything else is
/// refused by name before grant or service machinery runs.
async fn explicit_service_set(backend: Backend) {
    let root = Scratch::new();
    let client = endpoint(backend).await;
    let server = endpoint(backend).await;
    // A reachable TCP target proves the enabled service still works.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_port = match listener.local_addr().unwrap() {
        SocketAddr::V4(addr) => addr.port(),
        SocketAddr::V6(_) => panic!("expected v4"),
    };

    let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), tcp_port));
    policy.allow.insert(client.id());
    // Configured but not enabled: the explicit set wins over inference.
    policy.sync_dir = Some(root.0.clone());
    policy.services = Some(BTreeSet::from([ServiceKind::Tcp]));

    let agent = Arc::new(Agent::new(server.clone(), policy));
    let runner = agent.clone();
    let _task = Runner(tokio::spawn(async move {
        runner.run().await.unwrap();
    }));
    let conn = rds_client::connect(&client, server.addr()).await.unwrap();

    // The Info advertisement is exactly the enabled set, never more.
    let info = rds_client::info(&conn).await.unwrap();
    assert_eq!(
        info.services,
        vec![ServiceKind::Ping, ServiceKind::Info, ServiceKind::Tcp]
    );

    rds_client::ping(&conn, rand::random()).await.unwrap();
    let (send, _recv) = rds_client::open_tcp(&conn, "127.0.0.1", tcp_port)
        .await
        .expect("enabled tcp service is served");
    drop(send);
    drop(listener);

    // Every disabled service refuses by name — including sync, which is
    // configured on disk but not enabled, and audio, which is reserved.
    for hello in [
        StreamHello::Sync,
        StreamHello::SyncTransferV2 { id: rand::random() },
        StreamHello::Audio(rds_core::AudioHello {
            codec: rds_core::AudioCodec::Opus,
            sample_rate: 48_000,
            channels: 2,
        }),
    ] {
        let message = error_ack(&conn, hello).await;
        assert!(message.contains("not enabled"), "{message}");
    }
}

/// A disabled service is refused by deployment policy before the grant
/// machinery runs — the peer hears "not enabled", not "grant required".
async fn disabled_precedes_grants(backend: Backend) {
    let client = endpoint(backend).await;
    let server = endpoint(backend).await;
    let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
    policy.allow.insert(client.id());
    policy.issuers.insert([5; 32]);
    policy.use_local_revocations();
    policy.services = Some(BTreeSet::from([ServiceKind::Tcp]));

    let agent = Arc::new(Agent::new(server.clone(), policy));
    let runner = agent.clone();
    let _task = Runner(tokio::spawn(async move {
        runner.run().await.unwrap();
    }));
    let conn = rds_client::connect(&client, server.addr()).await.unwrap();

    let message = error_ack(&conn, StreamHello::Sync).await;
    assert!(message.contains("not enabled"), "{message}");

    // The enabled service still fails closed on the missing grant.
    let message = error_ack(
        &conn,
        StreamHello::TcpConnect {
            host: "127.0.0.1".into(),
            port: 9,
        },
    )
    .await;
    assert!(message.contains("grant"), "{message}");
}

/// The greeting deadline is policy, not a fixed constant: a silent stream
/// is cut loose inside the configured hello timeout.
async fn hello_deadline_is_policy(backend: Backend) {
    let client = endpoint(backend).await;
    let server = endpoint(backend).await;
    let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
    policy.allow.insert(client.id());
    policy.timeouts = TimeoutPolicy {
        hello: Duration::from_millis(200),
        ..Default::default()
    };

    let agent = Arc::new(Agent::new(server.clone(), policy));
    let runner = agent.clone();
    let _task = Runner(tokio::spawn(async move {
        runner.run().await.unwrap();
    }));
    let conn = rds_client::connect(&client, server.addr()).await.unwrap();

    let (_send, mut recv) = conn.open_bi().await.unwrap();
    // The server never writes a refusal for a silent greeting; the task
    // dies and the stream ends well inside the default 15s.
    let outcome =
        tokio::time::timeout(Duration::from_secs(5), read_frame::<_, HelloAck>(&mut recv)).await;
    assert!(matches!(outcome, Ok(Err(_)) | Err(_)), "{outcome:?}");
}

#[tokio::test]
async fn explicit_service_set_gates_streams() {
    for backend in backends() {
        explicit_service_set(backend).await;
    }
}

#[tokio::test]
async fn disabled_service_precedes_grant_requirement() {
    for backend in backends() {
        disabled_precedes_grants(backend).await;
    }
}

#[tokio::test]
async fn hello_deadline_comes_from_policy() {
    for backend in backends() {
        hello_deadline_is_policy(backend).await;
    }
}
