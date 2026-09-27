//! Canceling an agent runner must end its child sessions.
use rds_agent::{Agent, AgentLimits, AgentPolicy};
use rds_net::{Backend, Endpoint, EndpointConfig, bind_endpoint};
use std::sync::Arc;
use std::time::Duration;

async fn canceled_runner(backend: Backend) {
    let config = || EndpointConfig {
        backend,
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    };
    let client = bind_endpoint(config()).await.unwrap();
    let endpoint = bind_endpoint(config()).await.unwrap();
    let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
    policy.allow.insert(client.id());
    // Retain the Agent/endpoint: cancellation must work independently of
    // accidentally dropping the last endpoint handle in the test fixture.
    let agent = Arc::new(Agent::new(endpoint.clone(), policy));
    let runner = tokio::spawn({
        let agent = agent.clone();
        async move { agent.run().await }
    });
    let conn = rds_cli::connect(&client, endpoint.addr()).await.unwrap();
    rds_cli::ping(&conn, 1).await.unwrap();
    runner.abort();
    assert!(runner.await.unwrap_err().is_cancelled());
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        while !conn.is_closed() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    endpoint.close().await;
    client.close().await;
    assert!(
        closed.is_ok(),
        "{backend:?} runner left a child session alive"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceling_iroh_runner_closes_child_sessions() {
    canceled_runner(Backend::Iroh).await;
}

#[cfg(feature = "transport-noq")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceling_owned_runner_closes_child_sessions() {
    canceled_runner(Backend::Noq).await;
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

async fn fixture(backend: Backend, connections: u16, streams: u16) -> (Arc<Agent>, Vec<Endpoint>) {
    fixture_target(backend, connections, streams, 9).await
}

async fn fixture_target(
    backend: Backend,
    connections: u16,
    streams: u16,
    tcp_port: u16,
) -> (Arc<Agent>, Vec<Endpoint>) {
    let config = || EndpointConfig {
        backend,
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    };
    let clients = vec![
        bind_endpoint(config()).await.unwrap(),
        bind_endpoint(config()).await.unwrap(),
    ];
    let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), tcp_port));
    policy.allow.extend(clients.iter().map(Endpoint::id));
    let agent =
        Agent::new(bind_endpoint(config()).await.unwrap(), policy).with_limits(AgentLimits::new(
            std::num::NonZeroU16::new(connections).unwrap(),
            std::num::NonZeroU16::new(streams).unwrap(),
        ));
    (Arc::new(agent), clients)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceling_runner_closes_the_forwarded_tcp_socket() {
    use tokio::io::AsyncReadExt;
    for backend in backends() {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        let (agent, clients) = fixture_target(backend, 1, 2, port).await;
        let runner = start(&agent);
        let conn = rds_cli::connect(&clients[0], agent.endpoint.addr())
            .await
            .unwrap();
        let (mut send, _recv) = rds_cli::open_tcp(&conn, "127.0.0.1", port).await.unwrap();
        let (mut remote, _) = tcp.accept().await.unwrap();
        send.write_all(b"x").await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), remote.read_u8())
                .await
                .unwrap()
                .unwrap(),
            b'x'
        );
        state(&agent, 1, 1).await;
        runner.abort();
        assert!(runner.await.unwrap_err().is_cancelled());
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), remote.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        state(&agent, 0, 0).await;
        tokio::time::timeout(Duration::from_secs(2), conn.wait_closed())
            .await
            .unwrap();
        agent.endpoint.close().await;
        for client in clients {
            client.close().await;
        }
    }
}

fn start(agent: &Arc<Agent>) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let agent = agent.clone();
    tokio::spawn(async move { agent.run().await })
}

async fn state(agent: &Agent, connections: usize, streams: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while agent.active_connections() != connections || agent.active_streams() != streams {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("agent did not release/reach its admission slots");
}

async fn stop(
    agent: &Agent,
    clients: &[Endpoint],
    runner: tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    agent.endpoint.close().await;
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(agent.active_connections(), 0);
    assert_eq!(agent.active_streams(), 0);
    assert_eq!(
        agent.endpoint.metrics().snapshot()["rds_net_active_connections"],
        0
    );
    for client in clients {
        client.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_budget_backpressures_and_releases_a_slot() {
    for backend in backends() {
        let (agent, clients) = fixture(backend, 2, 2).await;
        let runner = start(&agent);
        let conn = rds_cli::connect(&clients[0], agent.endpoint.addr())
            .await
            .unwrap();
        let (mut first, _first_reply) = conn.open_bi().await.unwrap();
        first.write_all(&[0]).await.unwrap();
        let (mut second, _second_reply) = conn.open_bi().await.unwrap();
        second.write_all(&[0]).await.unwrap();
        state(&agent, 1, 2).await;
        let ping = rds_cli::ping(&conn, 9);
        tokio::pin!(ping);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut ping)
                .await
                .is_err()
        );
        assert_eq!(agent.active_streams(), 2);
        first.reset(0u32.into()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), ping)
            .await
            .unwrap()
            .unwrap();
        state(&agent, 1, 1).await;
        // Close while one worker is still blocked on a real partial hello.
        stop(&agent, &clients, runner).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_connection_budget_refuses_then_recovers() {
    for backend in backends() {
        let (agent, clients) = fixture(backend, 1, 2).await;
        let runner = start(&agent);
        let first = rds_cli::connect(&clients[0], agent.endpoint.addr())
            .await
            .unwrap();
        rds_cli::ping(&first, 1).await.unwrap();
        state(&agent, 1, 0).await;
        let rejected = tokio::time::timeout(Duration::from_secs(2), async {
            match rds_cli::connect(&clients[1], agent.endpoint.addr()).await {
                Err(_) => true,
                Ok(conn) => rds_cli::ping(&conn, 2).await.is_err(),
            }
        })
        .await
        .expect("full agent did not promptly refuse admission");
        assert!(rejected);
        assert_eq!(agent.active_connections(), 1);
        first.close(0u32.into(), b"release connection slot");
        state(&agent, 0, 0).await;
        let next = rds_cli::connect(&clients[1], agent.endpoint.addr())
            .await
            .unwrap();
        rds_cli::ping(&next, 3).await.unwrap();
        stop(&agent, &clients, runner).await;
    }
}

async fn manual_pair(
    agent: &Agent,
    client: &Endpoint,
) -> (rds_net::Connection, rds_net::Connection) {
    let (client, server) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            client.connect(agent.endpoint.addr(), rds_core::ALPN),
            async { agent.endpoint.accept().await.unwrap().await }
        )
    })
    .await
    .unwrap();
    (client.unwrap(), server.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_serve_enforces_budget_and_cancellation_releases_it() {
    for backend in backends() {
        let (agent, clients) = fixture(backend, 1, 2).await;
        let (first, server) = manual_pair(&agent, &clients[0]).await;
        let serving = tokio::spawn({
            let agent = agent.clone();
            async move { agent.serve(server).await }
        });
        rds_cli::ping(&first, 1).await.unwrap();
        state(&agent, 1, 0).await;
        let (second, server) = manual_pair(&agent, &clients[1]).await;
        let error = agent.serve(server).await.unwrap_err();
        assert!(error.to_string().contains("connection budget"));
        tokio::time::timeout(Duration::from_secs(2), second.wait_closed())
            .await
            .unwrap();
        serving.abort();
        assert!(serving.await.unwrap_err().is_cancelled());
        state(&agent, 0, 0).await;
        assert_eq!(
            agent.endpoint.metrics().snapshot()["rds_net_active_connections"],
            0
        );
        tokio::time::timeout(Duration::from_secs(2), first.wait_closed())
            .await
            .unwrap();
        let (third, server) = manual_pair(&agent, &clients[1]).await;
        let serving = tokio::spawn({
            let agent = agent.clone();
            async move { agent.serve(server).await }
        });
        rds_cli::ping(&third, 3).await.unwrap();
        third.close(0u32.into(), b"done");
        tokio::time::timeout(Duration::from_secs(2), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        state(&agent, 0, 0).await;
        agent.endpoint.close().await;
        for client in clients {
            client.close().await;
        }
    }
}

/// Fixture variant carrying a process fd ceiling (MiB ceiling unused here).
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn fixture_with_fd_budget(
    backend: Backend,
    connections: u16,
    streams: u16,
    max_fds: u64,
) -> (Arc<Agent>, Vec<Endpoint>) {
    let config = || EndpointConfig {
        backend,
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    };
    let clients = vec![
        bind_endpoint(config()).await.unwrap(),
        bind_endpoint(config()).await.unwrap(),
    ];
    let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
    policy.allow.extend(clients.iter().map(Endpoint::id));
    let agent = Agent::new(bind_endpoint(config()).await.unwrap(), policy).with_limits(
        AgentLimits::new(
            std::num::NonZeroU16::new(connections).unwrap(),
            std::num::NonZeroU16::new(streams).unwrap(),
        )
        .with_process_budget(Some(max_fds), None),
    );
    (Arc::new(agent), clients)
}

/// A ceiling of one descriptor is already exceeded, so the runner refuses
/// the handshake and `serve` refuses an established conn — neither consumes
/// a connection slot.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_fd_budget_refuses_runner_and_serve() {
    for backend in backends() {
        let (agent, clients) = fixture_with_fd_budget(backend, 4, 2, 1).await;
        let runner = start(&agent);
        let refused = tokio::time::timeout(Duration::from_secs(2), async {
            rds_cli::connect(&clients[0], agent.endpoint.addr())
                .await
                .is_err()
        })
        .await
        .expect("over-budget agent left a connection attempt parked");
        assert!(
            refused,
            "{backend:?} over-budget agent admitted a connection"
        );
        state(&agent, 0, 0).await;
        runner.abort();
        assert!(runner.await.unwrap_err().is_cancelled());
        // serve() applies the same gate to an already-established conn.
        let (conn, server) = manual_pair(&agent, &clients[1]).await;
        let error = agent.serve(server).await.unwrap_err();
        assert!(error.to_string().contains("resource budget"));
        tokio::time::timeout(Duration::from_secs(2), conn.wait_closed())
            .await
            .unwrap();
        assert_eq!(agent.active_connections(), 0);
        agent.endpoint.close().await;
        for client in clients {
            client.close().await;
        }
    }
}

/// A ceiling above real usage admits normally — the gate must not disturb
/// the configured-but-untriggered path.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_fd_budget_above_usage_still_serves() {
    for backend in backends() {
        let (agent, clients) = fixture_with_fd_budget(backend, 1, 2, u64::MAX).await;
        let runner = start(&agent);
        let conn = rds_cli::connect(&clients[0], agent.endpoint.addr())
            .await
            .unwrap();
        rds_cli::ping(&conn, 1).await.unwrap();
        stop(&agent, &clients, runner).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_track_real_admission_streams_and_do_not_own_agent_io() {
    for backend in backends() {
        let (agent, clients) = fixture(backend, 2, 2).await;
        let metrics = agent.metrics();
        let runner = start(&agent);
        let conn = rds_cli::connect(&clients[0], agent.endpoint.addr())
            .await
            .unwrap();
        rds_cli::ping(&conn, 42).await.unwrap();
        let (mut stalled, _reply) = conn.open_bi().await.unwrap();
        stalled.write_all(&[0]).await.unwrap();
        state(&agent, 1, 1).await;
        let observed = metrics.snapshot();
        assert_eq!(observed["rds_agent_connections_active"], 1);
        assert_eq!(observed["rds_agent_streams_active"], 1);
        assert_eq!(observed["rds_agent_connections_limit"], 2);
        assert_eq!(observed["rds_net_connections_accepted_total"], 1);
        assert_eq!(observed["rds_net_active_connections"], 1);
        assert_eq!(observed["rds_agent_grants_required"], 0);
        assert!(!observed.contains_key("rds_agent_revocations_fresh"));
        // Busy policy accounting is unknown, never a fabricated zero.
        {
            let _grants = agent.policy.active_grants.lock().unwrap();
            let busy = metrics.snapshot();
            assert_eq!(busy["rds_agent_active_grants_known"], 0);
            assert!(!busy.contains_key("rds_agent_active_grants"));
        }
        stop(&agent, &clients, runner).await;
        assert_eq!(metrics.snapshot()["rds_agent_connections_active"], 0);
        assert_eq!(metrics.snapshot()["rds_agent_streams_active"], 0);
        drop(agent);
        assert_eq!(
            metrics.snapshot(),
            std::collections::BTreeMap::from([("rds_agent_metrics_available", 0)])
        );
    }
}
