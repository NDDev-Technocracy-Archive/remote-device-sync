#![cfg(unix)]
//! Real local IPC, multiple QUIC peers and TCP bodies; no external services.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rds_agent::{Agent, AgentPolicy};
use rds_client::local::{Capacity, Client, Error, Prepared, Server};
use rds_core::local::{Command, ErrorCode, Reply, SessionId, Status};
use rds_net::{Backend, Endpoint, EndpointConfig, Ticket, bind_endpoint};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket, UnixStream};
use tokio::task::JoinSet;

/// The small manager capacity the capacity test fills; the default is larger.
const CAPACITY: Capacity = Capacity {
    streams: 64,
    workers: 96,
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::path::Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!(
                "rds-local-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(backend: Backend) -> EndpointConfig {
    EndpointConfig {
        backend,
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    }
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

#[tokio::test]
async fn control_reply_requires_commit_eof_and_rejects_trailing_bytes() {
    use rds_core::local::{Request, Response, VERSION};
    use std::os::unix::fs::PermissionsExt;
    for trailing in [false, true] {
        let root = Scratch::new();
        let path = root.0.join("control.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let client = Client::new(&root.0);
        let mut pending = tokio::spawn(async move { client.request(Command::List).await });
        let (mut socket, _) = listener.accept().await.unwrap();
        let _: Request = rds_net::read_frame(&mut socket).await.unwrap();
        rds_net::write_frame(
            &mut socket,
            &Response {
                version: VERSION,
                result: Ok(Reply::Done),
            },
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pending)
                .await
                .is_err()
        );
        if trailing {
            socket.write_all(&[1]).await.unwrap();
        }
        drop(socket);
        let result = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap();
        if trailing {
            assert!(matches!(result, Err(Error::Protocol)));
        } else {
            assert!(matches!(result, Ok(Reply::Done)));
        }
    }
}

async fn connect(client: &Client, target: String) -> SessionId {
    match client
        .request(Command::Connect {
            target,
            grant: None,
        })
        .await
        .unwrap()
    {
        Reply::Connected(id) => id,
        reply => panic!("unexpected {reply:?}"),
    }
}

async fn peer(
    backend: Backend,
    local: &Endpoint,
    marker: u8,
    tasks: &mut JoinSet<()>,
) -> (Endpoint, rds_core::TcpTarget) {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = tcp
        .local_addr()
        .unwrap()
        .to_string()
        .parse::<rds_core::TcpTarget>()
        .unwrap();
    tasks.spawn(async move {
        let mut workers = JoinSet::new();
        loop {
            tokio::select! {
                Some(_) = workers.join_next(), if !workers.is_empty() => {}
                accepted = tcp.accept(), if workers.len() < 64 => {
                    let (mut stream, _) = accepted.unwrap();
                    workers.spawn(async move {
                        stream.write_u8(marker).await.unwrap();
                        let (mut read, mut write) = stream.split();
                        let _ = tokio::io::copy(&mut read, &mut write).await;
                    });
                }
            }
        }
    });
    let endpoint = bind_endpoint(config(backend)).await.unwrap();
    let mut policy = AgentPolicy::ssh_only(target.clone().into_parts());
    policy.allow.insert(local.id());
    let agent = Arc::new(Agent::new(endpoint.clone(), policy));
    tasks.spawn(async move {
        agent.run().await.unwrap();
    });
    (endpoint, target)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_endpoint_switching_preserves_streams_and_explicit_close_is_scoped() {
    for backend in backends() {
        let root = Scratch::new();
        let path = root.0.join("control");
        let local = bind_endpoint(config(backend)).await.unwrap();
        let prepared = Prepared::bind(&path).await.unwrap();
        let mut server = Server::start(Some(prepared), local.clone(), None);
        let observer = server.take_observer().unwrap();
        let client = Client::new(&path);
        let mut tasks = JoinSet::new();
        let (first, first_tcp) = peer(backend, &local, b'A', &mut tasks).await;
        let (second, second_tcp) = peer(backend, &local, b'B', &mut tasks).await;
        let first_id = connect(&client, Ticket::of(&first).to_string()).await;
        let second_id = connect(&client, Ticket::of(&second).to_string()).await;
        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.endpoint, local.id().to_string());
        assert_eq!(snapshot.sessions.len(), 2);
        assert_eq!(snapshot.selected, Some(first_id));
        assert!(
            snapshot
                .sessions
                .iter()
                .all(|s| s.status == Status::Connected)
        );
        let opened = local.metrics().snapshot()["rds_net_connections_opened_total"];
        assert_eq!(
            connect(&client, Ticket::of(&first).to_string()).await,
            first_id
        );
        assert_eq!(
            client.snapshot().await.unwrap().generation,
            snapshot.generation
        );
        assert_eq!(
            local.metrics().snapshot()["rds_net_connections_opened_total"],
            opened
        );
        assert_eq!(observer.snapshot()["rds_agent_local_manager_connected"], 2);
        let mut held = client.open_tcp(first_id, first_tcp.clone()).await.unwrap();
        assert_eq!(held.read_u8().await.unwrap(), b'A');
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        let forwarding = tokio::spawn({
            let client = client.clone();
            let target = first_tcp.clone();
            async move {
                client
                    .forward(
                        first_id,
                        listener,
                        target,
                        std::num::NonZeroU16::new(2).unwrap(),
                    )
                    .await
            }
        });
        client
            .request(Command::Select { session: second_id })
            .await
            .unwrap();
        // Newly accepted sockets on an existing listener must also stay on A.
        let mut local_tcp = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        assert_eq!(local_tcp.read_u8().await.unwrap(), b'A');
        assert!(
            matches!(client.request(Command::Ping { session: None, nonce: 42 }).await.unwrap(), Reply::Pong { session, .. } if session == second_id)
        );
        assert!(
            matches!(client.request(Command::Info { session: Some(first_id) }).await.unwrap(), Reply::Info { session, .. } if session == first_id)
        );
        held.write_all(b"still-A").await.unwrap();
        let mut echo = [0; 7];
        held.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"still-A");
        let mut other = client.open_tcp(second_id, second_tcp).await.unwrap();
        assert_eq!(other.read_u8().await.unwrap(), b'B');
        // A grant-bearing request must not inherit an ungranted connection.
        let conflict = client
            .request(Command::Connect {
                target: Ticket::of(&first).to_string(),
                grant: Some(Box::new(rds_core::grant::Grant {
                    payload: vec![1],
                    signature: vec![],
                })),
            })
            .await;
        assert!(matches!(
            conflict,
            Err(Error::Rejected(ErrorCode::CredentialConflict))
        ));
        client
            .request(Command::Disconnect { session: second_id })
            .await
            .unwrap();
        assert_eq!(client.snapshot().await.unwrap().selected, None);
        assert!(matches!(
            client
                .request(Command::Ping {
                    session: None,
                    nonce: 1
                })
                .await,
            Err(Error::Rejected(ErrorCode::NoSelection))
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), other.read_u8()).await,
            Ok(Err(_))
        ));
        held.write_u8(b'Z').await.unwrap();
        assert_eq!(held.read_u8().await.unwrap(), b'Z');
        // Peer-side closure removes stale selection/session without redial.
        first.close().await;
        assert!(
            tokio::time::timeout(Duration::from_secs(3), forwarding)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !client.snapshot().await.unwrap().sessions.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        server.close().await.unwrap();
        server.close().await.unwrap();
        assert!(!path.join("control.sock").exists());
        assert_eq!(
            observer.snapshot()["rds_agent_local_manager_snapshot_available"],
            0
        );
        let mut restarted = Server::start(
            Some(Prepared::bind(&path).await.unwrap()),
            local.clone(),
            None,
        );
        assert_ne!(client.snapshot().await.unwrap().instance, snapshot.instance);
        assert!(matches!(
            client.request(Command::Select { session: first_id }).await,
            Err(Error::Rejected(ErrorCode::NotFound))
        ));
        restarted.close().await.unwrap();
        // Manager close did not close the shared agent endpoint.
        let conn = rds_client::connect(&local, second.addr()).await.unwrap();
        rds_client::ping(&conn, 7).await.unwrap();
        conn.close(0u32.into(), b"test complete");
        local.close().await;
        second.close().await;
        tasks.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_dial_never_blocks_selection_and_cancel_releases_reservation() {
    for backend in backends() {
        let root = Scratch::new();
        let path = root.0.join("control");
        let local = bind_endpoint(config(backend)).await.unwrap();
        let mut server = Server::start(
            Some(Prepared::bind(&path).await.unwrap()),
            local.clone(),
            None,
        );
        let client = Client::new(&path);
        let blackhole = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = rds_net::EndpointAddr::new(rds_net::SecretKey::generate().public())
            .with_ip_addr(blackhole.local_addr().unwrap());
        let target = Ticket(addr).to_string();
        for explicit in [false, true] {
            let pending = tokio::spawn({
                let client = client.clone();
                let target = target.clone();
                async move {
                    client
                        .request(Command::Connect {
                            target,
                            grant: None,
                        })
                        .await
                }
            });
            let id = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(session) = client.snapshot().await.unwrap().sessions.first() {
                        break session.id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let mut duplicate = tokio::spawn({
                let client = client.clone();
                let target = target.clone();
                async move {
                    client
                        .request(Command::Connect {
                            target,
                            grant: None,
                        })
                        .await
                }
            });
            assert!(
                tokio::time::timeout(Duration::from_millis(30), &mut duplicate)
                    .await
                    .is_err(),
                "same-credential dial should join the pending owner"
            );
            assert_eq!(client.snapshot().await.unwrap().sessions.len(), 1);
            if explicit {
                client
                    .request(Command::Disconnect { session: id })
                    .await
                    .unwrap();
                assert!(matches!(
                    pending.await.unwrap(),
                    Err(Error::Rejected(ErrorCode::NotFound))
                ));
            } else {
                pending.abort();
                assert!(pending.await.unwrap_err().is_cancelled());
            }
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(2), duplicate)
                    .await
                    .unwrap()
                    .unwrap(),
                Err(Error::Rejected(ErrorCode::NotFound))
            ));
            tokio::time::timeout(Duration::from_secs(2), async {
                while !client.snapshot().await.unwrap().sessions.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        server.close().await.unwrap();
        local.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_connect_joins_one_owner_and_canceled_waiter_does_not_cancel_dial() {
    for backend in backends() {
        let root = Scratch::new();
        let path = root.0.join("control");
        let local = bind_endpoint(config(backend)).await.unwrap();
        let remote = bind_endpoint(config(backend)).await.unwrap();
        let mut manager = Server::start(
            Some(Prepared::bind(&path).await.unwrap()),
            local.clone(),
            None,
        );
        let client = Client::new(&path);
        let ticket = Ticket::of(&remote).to_string();
        let pinned = remote.id().to_string();
        let (arrived, held) = tokio::sync::oneshot::channel();
        let (release, gate) = tokio::sync::oneshot::channel();
        let serving = tokio::spawn({
            let remote = remote.clone();
            async move {
                let conn = remote.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = conn.accept_bi().await.unwrap();
                let rds_core::StreamHello::Ping { nonce } =
                    rds_net::read_frame(&mut recv).await.unwrap()
                else {
                    panic!("application readiness ping expected")
                };
                arrived.send(()).unwrap();
                gate.await.unwrap();
                rds_net::write_frame(&mut send, &rds_core::HelloAck::Ok)
                    .await
                    .unwrap();
                send.write_all(&nonce.to_be_bytes()).await.unwrap();
                send.finish().unwrap();
                conn
            }
        });
        let owner = tokio::spawn({
            let client = client.clone();
            let ticket = ticket.clone();
            async move {
                client
                    .request(Command::Connect {
                        target: ticket,
                        grant: None,
                    })
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(3), held)
            .await
            .unwrap()
            .unwrap();
        let mut canceled = tokio::spawn({
            let client = client.clone();
            let pinned = pinned.clone();
            async move {
                client
                    .request(Command::Connect {
                        target: pinned,
                        grant: None,
                    })
                    .await
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut canceled)
                .await
                .is_err()
        );
        canceled.abort();
        assert!(canceled.await.unwrap_err().is_cancelled());
        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].status, Status::Connecting);
        let conflict = client
            .request(Command::Connect {
                target: ticket,
                grant: Some(Box::new(rds_core::grant::Grant {
                    payload: vec![1],
                    signature: vec![],
                })),
            })
            .await;
        assert!(matches!(
            conflict,
            Err(Error::Rejected(ErrorCode::CredentialConflict))
        ));
        let mut joined = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .request(Command::Connect {
                        target: pinned,
                        grant: None,
                    })
                    .await
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut joined)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        let Reply::Connected(id) = owner.await.unwrap().unwrap() else {
            panic!("connected owner expected")
        };
        assert!(matches!(joined.await.unwrap().unwrap(),Reply::Connected(other) if other==id));
        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].status, Status::Connected);
        assert_eq!(
            local.metrics().snapshot()["rds_net_connections_opened_total"],
            1,
            "duplicate transport association opened"
        );
        let retained = serving.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), remote.accept())
                .await
                .is_err()
        );
        drop(retained);
        manager.close().await.unwrap();
        tokio::join!(local.close(), remote.close());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_is_pinned_and_stops_after_disconnect_and_server_drop_closes_sessions() {
    let root = Scratch::new();
    let path = root.0.join("control");
    let local = bind_endpoint(config(Backend::Iroh)).await.unwrap();
    let server = Server::start(
        Some(Prepared::bind(&path).await.unwrap()),
        local.clone(),
        None,
    );
    let client = Client::new(&path);
    let mut tasks = JoinSet::new();
    let (remote, tcp) = peer(Backend::Iroh, &local, b'X', &mut tasks).await;
    let id = connect(&client, Ticket::of(&remote).to_string()).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let forward = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .forward(id, listener, tcp, std::num::NonZeroU16::new(2).unwrap())
                .await
        }
    });
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    assert_eq!(stream.read_u8().await.unwrap(), b'X');
    drop(server);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), forward)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(stream.read_u8().await.is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while path.join("control.sock").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
    local.close().await;
    remote.close().await;
    tasks.shutdown().await;
}

#[tokio::test]
async fn private_socket_preflight_refuses_unsafe_paths_and_recovers_only_a_dead_socket() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = Scratch::new();
    let path = root.0.join("control");
    let prepared = Prepared::bind(&path).await.unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(path.join("control.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(matches!(
        Prepared::bind(&path).await,
        Err(Error::AlreadyRunning)
    ));
    drop(prepared);
    assert!(!path.join("control.sock").exists());
    let alias = root.0.join("alias");
    symlink(&path, &alias).unwrap();
    assert!(Prepared::bind(&alias).await.is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        Prepared::bind(&path).await,
        Err(Error::UnsafeDirectory)
    ));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = path.join("control.sock");
    std::fs::write(&socket, b"unrelated").unwrap();
    assert!(matches!(
        Prepared::bind(&path).await,
        Err(Error::UnsafeSocket)
    ));
    assert_eq!(std::fs::read(&socket).unwrap(), b"unrelated");
    std::fs::remove_file(&socket).unwrap();
    let live = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    assert!(matches!(
        Prepared::bind(&path).await,
        Err(Error::AlreadyRunning)
    ));
    drop(live);
    // Concurrent process-spawn fixtures can briefly retain a listener FD until
    // exec. Observe refusal before asserting recovery of a *dead* socket.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match UnixStream::connect(&socket).await {
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => break,
                Ok(_) => tokio::time::sleep(Duration::from_millis(1)).await,
                Err(error) => panic!("unexpected socket retirement error: {error}"),
            }
        }
    })
    .await
    .unwrap();
    // The dead reserved socket survives a process death; a new owner recovers it.
    let recovered = Prepared::bind(&path).await.unwrap();
    drop(recovered);
    assert!(!socket.exists());
    // Cleanup must not delete a replacement inode.
    let prepared = Prepared::bind(&path).await.unwrap();
    std::fs::remove_file(&socket).unwrap();
    std::fs::write(&socket, b"replacement").unwrap();
    drop(prepared);
    assert_eq!(std::fs::read(&socket).unwrap(), b"replacement");
}

#[tokio::test]
async fn bad_ipc_version_oversized_frame_and_untrusted_socket_do_not_mutate_state() {
    let root = Scratch::new();
    let path = root.0.join("control");
    let local = bind_endpoint(config(Backend::Iroh)).await.unwrap();
    let mut server = Server::start(
        Some(Prepared::bind(&path).await.unwrap()),
        local.clone(),
        None,
    );
    let client = Client::new(&path);
    let before = client.snapshot().await.unwrap();
    let mut stream = UnixStream::connect(path.join("control.sock"))
        .await
        .unwrap();
    rds_net::write_frame(
        &mut stream,
        &rds_core::local::Request {
            version: 999,
            command: Command::List,
        },
    )
    .await
    .unwrap();
    let response: rds_core::local::Response = rds_net::read_frame(&mut stream).await.unwrap();
    assert_eq!(response.result.unwrap_err(), ErrorCode::Version);
    let mut stream = UnixStream::connect(path.join("control.sock"))
        .await
        .unwrap();
    stream
        .write_all(&(rds_core::MAX_MESSAGE_LEN + 1).to_be_bytes())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.read_u8())
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(
        client.snapshot().await.unwrap().generation,
        before.generation
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        path.join("control.sock"),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    assert!(matches!(client.snapshot().await, Err(Error::UnsafeSocket)));
    server.close().await.unwrap();
    local.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_grants_keep_service_restrictions_and_revoke_held_sessions() {
    let root = Scratch::new();
    let local = bind_endpoint(config(Backend::Iroh)).await.unwrap();
    let remote = bind_endpoint(config(Backend::Iroh)).await.unwrap();
    let issuer = ed25519_dalek::SigningKey::from_bytes(&[91; 32]);
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target: rds_core::TcpTarget = tcp.local_addr().unwrap().to_string().parse().unwrap();
    let mut policy = AgentPolicy::ssh_only(target.clone().into_parts());
    policy.allow.insert(local.id());
    policy.issuers.insert(issuer.verifying_key().to_bytes());
    policy.use_local_revocations(); // Separately qualified managed-feed policy is unchanged.
    let grant = rds_core::grant::Grant::issue(
        &issuer,
        *local.id().as_bytes(),
        *remote.id().as_bytes(),
        [1; 16],
        vec![rds_core::ServiceKind::Tcp],
        Duration::from_secs(60),
        Default::default(),
    );
    let agent = Arc::new(Agent::new(remote.clone(), policy.clone()));
    let runner = tokio::spawn(async move {
        agent.run().await.unwrap();
    });
    let path = root.0.join("control");
    let mut server = Server::start(
        Some(Prepared::bind(&path).await.unwrap()),
        local.clone(),
        None,
    );
    let client = Client::new(&path);
    let command = Command::Connect {
        target: Ticket::of(&remote).to_string(),
        grant: Some(Box::new(grant.clone())),
    };
    let Reply::Connected(id) = client.request(command.clone()).await.unwrap() else {
        panic!("connect")
    };
    // Reuse does not replay the grant's Authz stream.
    assert!(
        matches!(client.request(command).await.unwrap(), Reply::Connected(reused) if reused == id)
    );
    assert!(matches!(
        client
            .request(Command::Ping {
                session: Some(id),
                nonce: 1
            })
            .await,
        Err(Error::Rejected(ErrorCode::Remote))
    ));
    let mut stream = client.open_tcp(id, target).await.unwrap();
    let (mut service, _) = tcp.accept().await.unwrap();
    stream.write_u8(7).await.unwrap();
    assert_eq!(service.read_u8().await.unwrap(), 7);
    let mut payload = grant
        .verify(
            &std::collections::HashSet::from([issuer.verifying_key().to_bytes()]),
            local.id().as_bytes(),
            remote.id().as_bytes(),
            Duration::from_secs(300),
            rds_core::grant::now_unix(),
        )
        .unwrap()
        .payload;
    payload.revision += 1;
    payload.expires_at += 60;
    let renewed = rds_core::grant::Grant::issue_at(&issuer, payload);
    assert_eq!(grant.id().unwrap(), renewed.id().unwrap());
    for _ in 0..2 {
        assert!(matches!(
            client
                .request(Command::Renew {
                    session: id,
                    grant: Box::new(renewed.clone())
                })
                .await
                .unwrap(),
            Reply::Done
        ));
    }
    assert_eq!(policy.active_grants.lock().unwrap().len(), 1);
    assert!(matches!(
        client
            .request(Command::Connect {
                target: Ticket::of(&remote).to_string(),
                grant: Some(Box::new(grant.clone()))
            })
            .await,
        Err(Error::Rejected(ErrorCode::CredentialConflict))
    ));
    assert!(
        matches!(client.request(Command::Connect { target: Ticket::of(&remote).to_string(), grant: Some(Box::new(renewed)) }).await.unwrap(), Reply::Connected(reused) if reused == id)
    );
    assert_eq!(client.snapshot().await.unwrap().selected, Some(id));
    stream.write_u8(8).await.unwrap();
    assert_eq!(service.read_u8().await.unwrap(), 8);
    policy.revoke(grant.id().unwrap());
    assert!(
        tokio::time::timeout(Duration::from_secs(3), stream.read_u8())
            .await
            .unwrap()
            .is_err()
    );
    server.close().await.unwrap();
    local.close().await;
    remote.close().await;
    runner.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_agent_control_preflight_and_signal_shutdown_preserve_shared_identity() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = Scratch::new();
    let key = root.0.join("fresh/agent.key");
    let path = rds_client::local::control_dir_for_key(&key).unwrap();
    let invalid = root.0.join("not-a-directory");
    std::fs::write(&invalid, b"unrelated").unwrap();
    let command = || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_rds-agent"));
        command
            .args(["--no-relay", "--bind-address", "127.0.0.1:0", "--key-file"])
            .arg(&key)
            .env("RUST_LOG", "off")
            .env("RDS_LOG_FORMAT", "json");
        command
    };
    let invalid_output = command()
        .arg("--control-dir")
        .arg(&invalid)
        .output()
        .unwrap();
    assert!(!invalid_output.status.success());
    assert!(!key.exists());
    let mut child = Child(
        command()
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = Client::new(&path);
    let snapshot = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(child.0.try_wait().unwrap().is_none());
            if let Ok(snapshot) = client.snapshot().await {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let seed: [u8; 32] = std::fs::read(&key).unwrap().try_into().unwrap();
    assert_eq!(
        snapshot.endpoint,
        rds_net::SecretKey::from_bytes(&seed).public().to_string()
    );
    // A different control directory cannot bypass ownership of the seed.
    for args in [vec!["--control-dir"], vec!["--no-control"]] {
        let mut second = command();
        second.args(&args);
        if args[0] == "--control-dir" {
            second.arg(root.0.join("second"));
        }
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::from(second)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "second agent announced an endpoint"
        );
    }
    assert!(matches!(
        rds_net::acquire_key(&key),
        Err(rds_net::KeyStoreError::InUse)
    ));
    // SIGKILL leaves a stale socket but releases OS key/directory locks.
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(path.join("control.sock").exists());
    child = Child(
        command()
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let restarted = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(child.0.try_wait().unwrap().is_none());
            if let Ok(snapshot) = client.snapshot().await {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_ne!(restarted.instance, snapshot.instance);
    assert_eq!(restarted.endpoint, snapshot.endpoint);
    assert!(restarted.sessions.is_empty());
    let remote = bind_endpoint(config(Backend::Iroh)).await.unwrap();
    let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
    policy.allow.insert(snapshot.endpoint.parse().unwrap());
    let agent = Arc::new(Agent::new(remote.clone(), policy));
    let runner = tokio::spawn(async move {
        agent.run().await.unwrap();
    });
    let session = connect(&client, Ticket::of(&remote).to_string()).await;
    assert!(matches!(
        client
            .request(Command::Ping {
                session: Some(session),
                nonce: 8
            })
            .await
            .unwrap(),
        Reply::Pong { .. }
    ));
    let pid = rustix::process::Pid::from_raw(child.0.id() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(status.success());
    assert!(!path.join("control.sock").exists());
    assert_eq!(std::fs::read(&key).unwrap(), seed);
    assert_eq!(
        rds_net::acquire_key(&key)
            .unwrap()
            .secret_key()
            .public()
            .to_string(),
        snapshot.endpoint
    );
    remote.close().await;
    runner.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_silent_tcp_bodies_release_capacity_but_half_close_preserves_response() {
    let root = Scratch::new();
    let path = root.0.join("control");
    let local = bind_endpoint(config(Backend::Iroh)).await.unwrap();
    let remote = bind_endpoint(config(Backend::Iroh)).await.unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target: rds_core::TcpTarget = tcp.local_addr().unwrap().to_string().parse().unwrap();
    let mut policy = AgentPolicy::ssh_only(target.clone().into_parts());
    policy.allow.insert(local.id());
    let agent = Arc::new(Agent::new(remote.clone(), policy).with_limits(
        rds_agent::AgentLimits::new(
            std::num::NonZeroU16::new(4).unwrap(),
            std::num::NonZeroU16::new(128).unwrap(),
        ),
    ));
    let runner = tokio::spawn(async move {
        agent.run().await.unwrap();
    });
    let mut server = Server::start_with_capacity(
        Some(Prepared::bind(&path).await.unwrap()),
        local.clone(),
        None,
        CAPACITY,
    );
    let client = Client::new(&path);
    let id = connect(&client, Ticket::of(&remote).to_string()).await;
    let mut stream = client.open_tcp(id, target.clone()).await.unwrap();
    let (mut service, _) = tcp.accept().await.unwrap();
    stream.write_all(b"request").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut request = Vec::new();
    service.read_to_end(&mut request).await.unwrap();
    assert_eq!(&request, b"request");
    service.write_all(b"late reply").await.unwrap();
    service.shutdown().await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    assert_eq!(&response, b"late reply");
    drop((stream, service));
    // Opposite direction: a remote FIN must not terminate a later upload.
    let mut stream = client.open_tcp(id, target.clone()).await.unwrap();
    let (mut service, _) = tcp.accept().await.unwrap();
    service.write_all(b"greeting").await.unwrap();
    service.shutdown().await.unwrap();
    let mut greeting = Vec::new();
    stream.read_to_end(&mut greeting).await.unwrap();
    assert_eq!(&greeting, b"greeting");
    stream.write_all(b"late upload").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut upload = Vec::new();
    service.read_to_end(&mut upload).await.unwrap();
    assert_eq!(&upload, b"late upload");
    drop((stream, service));
    // Cross multiple IPC chunks and queue boundaries in both directions.
    let mut stream = client.open_tcp(id, target.clone()).await.unwrap();
    let (mut service, _) = tcp.accept().await.unwrap();
    let payload = vec![0x5a; rds_core::local::TCP_CHUNK * 5 + 137];
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            async {
                stream.write_all(&payload).await.unwrap();
                stream.shutdown().await.unwrap();
                let mut returned = Vec::new();
                stream.read_to_end(&mut returned).await.unwrap();
                assert_eq!(returned, payload);
            },
            async {
                let mut received = Vec::new();
                service.read_to_end(&mut received).await.unwrap();
                assert_eq!(received, payload);
                service.write_all(&received).await.unwrap();
                service.shutdown().await.unwrap();
            }
        );
    })
    .await
    .unwrap();
    drop((stream, service));
    for bytes in [vec![], vec![0; rds_core::local::TCP_CHUNK + 1]] {
        let mut raw = UnixStream::connect(path.join("control.sock"))
            .await
            .unwrap();
        rds_net::write_frame(
            &mut raw,
            &rds_core::local::Request {
                version: rds_core::local::VERSION,
                command: Command::OpenTcp {
                    session: Some(id),
                    target: target.clone(),
                },
            },
        )
        .await
        .unwrap();
        let response: rds_core::local::Response = rds_net::read_frame(&mut raw).await.unwrap();
        assert!(matches!(response.result, Ok(Reply::Opened(_))));
        let (mut service, _) = tcp.accept().await.unwrap();
        rds_net::write_frame(&mut raw, &rds_core::local::TcpFrame::Data(bytes))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), service.read_u8())
                .await
                .unwrap()
                .is_err()
        );
    }
    let mut held = Vec::new();
    let mut services = Vec::new();
    for _ in 0..CAPACITY.streams {
        held.push(client.open_tcp(id, target.clone()).await.unwrap());
        services.push(tcp.accept().await.unwrap().0);
    }
    assert!(matches!(
        client.open_tcp(id, target.clone()).await,
        Err(Error::Rejected(ErrorCode::Capacity))
    ));
    assert_eq!(client.snapshot().await.unwrap().sessions.len(), 1);
    drop(held); // Remote TCP sockets stay silent and open.
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match client.open_tcp(id, target.clone()).await {
                Ok(stream) => break stream,
                Err(Error::Rejected(ErrorCode::Capacity)) => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(error) => panic!("unexpected {error}"),
            }
        }
    })
    .await;
    server.close().await.unwrap();
    local.close().await;
    remote.close().await;
    runner.await.unwrap();
    assert!(
        result.is_ok(),
        "abandoned IPC bodies retained all {} stream slots",
        CAPACITY.streams
    );
}

/// `Client::desktop` exercises the whole managed path — connect, permit,
/// stream open — and reports a clean refusal when the peer cannot serve
/// desktop. Without the agent `desktop` feature the service gate refuses;
/// with it but headless, capture fails — either way the channel closes
/// and no stream slot leaks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_desktop_reports_remote_refusal_without_leaking() {
    for backend in backends() {
        let root = Scratch::new();
        let path = root.0.join("control");
        let local = bind_endpoint(config(backend)).await.unwrap();
        let prepared = Prepared::bind(&path).await.unwrap();
        let mut server = Server::start(Some(prepared), local.clone(), None);
        let client = Client::new(&path);
        let mut tasks = JoinSet::new();
        let (peer_ep, _tcp) = peer(backend, &local, b'D', &mut tasks).await;
        let session = connect(&client, Ticket::of(&peer_ep).to_string()).await;
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            client.desktop(
                Some(session),
                rds_core::DesktopHello {
                    display: 0,
                    max_fps: 30,
                    codec: rds_core::Codec::H264,
                    input_acks: false,
                },
            ),
        )
        .await
        .expect("managed desktop open hung");
        assert!(
            matches!(result, Err(Error::Rejected(ErrorCode::Remote))),
            "expected clean remote refusal, got {result:?}"
        );
        let separated = tokio::time::timeout(
            Duration::from_secs(10),
            client.desktop_profile_separated(
                Some(session),
                rds_core::DesktopHello {
                    display: 0,
                    max_fps: 30,
                    codec: rds_core::Codec::H264,
                    input_acks: true,
                },
                1080,
            ),
        )
        .await
        .expect("separated desktop refusal hung");
        assert!(matches!(separated, Err(Error::Rejected(ErrorCode::Remote))));
        // The refused open must not park a stream permit: open_tcp still
        // has its full data budget — every slot but the lane reserved
        // for control traffic.
        let mut held = Vec::new();
        for _ in 0..63 {
            held.push(
                client
                    .open_tcp(session, _tcp.clone())
                    .await
                    .expect("stream slots leaked"),
            );
        }
        // One lane stays free of data services: a 64th TCP greeting is
        // refused while that reserved slot keeps Ping reachable.
        let overflow = client.open_tcp(session, _tcp.clone()).await.err();
        assert!(
            matches!(overflow, Some(Error::Rejected(ErrorCode::Remote))),
            "expected capacity refusal past the data budget, got {overflow:?}"
        );
        drop(held);
        server.close().await.unwrap();
        tasks.abort_all();
    }
}
