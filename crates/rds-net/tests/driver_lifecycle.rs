//! Policy tasks must terminate on real connection lifetime boundaries.
#![cfg(feature = "transport-noq")]

use std::time::Duration;

use rds_net::EndpointConfig;
use rds_net::backends::noq::{self, Connection, Endpoint, policy};

async fn endpoints() -> (Endpoint, Endpoint) {
    let config = || EndpointConfig {
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    };
    (
        noq::bind_endpoint(config()).await.unwrap(),
        noq::bind_endpoint(config()).await.unwrap(),
    )
}

async fn pair(a: &Endpoint, b: &Endpoint) -> (Connection, Connection) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, server) = tokio::join!(a.connect(b.addr(), rds_core::ALPN), async {
            b.accept().await.unwrap().await
        });
        (client.unwrap(), server.unwrap())
    })
    .await
    .expect("local handshake")
}

async fn driver_terminates(drop_last_handle: bool) {
    let (a, b) = endpoints().await;
    let (client, server) = pair(&a, &b).await;
    let raw = client.inner();
    let mut driver = tokio::spawn(policy::connection_driver(
        raw.weak_handle(),
        raw.nat_traversal_updates(),
        raw.path_events(),
        vec![::noq::PathId::ZERO],
        a.metrics(),
    ));
    tokio::task::yield_now().await;
    let held = if drop_last_handle {
        drop(client);
        None
    } else {
        client.close(0u32.into(), b"test close with handles alive");
        Some(client)
    };
    let result = tokio::time::timeout(Duration::from_secs(2), &mut driver).await;
    driver.abort();
    if result.is_err() {
        let _ = driver.await;
    }
    drop(held);
    server.close(0u32.into(), b"done");
    a.close().await;
    b.close().await;
    assert!(result.is_ok(), "policy task survived the closed connection");
    result.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_connection_driver_finishes_with_live_handles() {
    driver_terminates(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_io_handle_drop_finishes_driver() {
    driver_terminates(true).await;
}

async fn wait_idle(a: &Endpoint, b: &Endpoint) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while a.active_path_drivers() != 0 || b.active_path_drivers() != 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("closed connections must release all policy tasks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_drivers_follow_connection_churn() {
    let (a, b) = endpoints().await;
    let mut closed_handles = Vec::new();
    for round in 0..32 {
        let (client, server) = pair(&a, &b).await;
        assert_eq!(a.active_path_drivers(), 1);
        assert_eq!(b.active_path_drivers(), 1);
        match round % 3 {
            0 => {
                client.close(0u32.into(), b"local close");
                closed_handles.push(client);
            }
            1 => {
                server.close(0u32.into(), b"peer close");
                closed_handles.push(client);
            }
            _ => drop(client),
        }
        closed_handles.push(server);
        wait_idle(&a, &b).await;
    }
    // Closed Connection objects may outlive their policy tasks indefinitely.
    assert!(!closed_handles.is_empty());
    a.close().await;
    b.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_keep_policy_alive_until_last_io_handle_is_dropped() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (a, b) = endpoints().await;
        let (client, server) = pair(&a, &b).await;
        let (mut send, recv) = client.open_bi().await.unwrap();
        send.write_all(b"a").await.unwrap();
        let (_reply, mut request) = server.accept_bi().await.unwrap();
        let mut byte = [0u8; 1];
        request.read_exact(&mut byte).await.unwrap();
        drop(client);
        tokio::task::yield_now().await;
        assert_eq!(a.active_path_drivers(), 1);
        send.write_all(b"b").await.unwrap();
        request.read_exact(&mut byte).await.unwrap();
        assert_eq!(&byte, b"b");
        drop(send);
        drop(recv);
        wait_idle(&a, &b).await;
        a.close().await;
        b.close().await;
    })
    .await
    .expect("stream ownership must not hang");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_close_joins_drivers_and_refuses_late_admission() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (a, b) = endpoints().await;
        let mut held = Vec::new();
        for _ in 0..8 {
            held.push(pair(&a, &b).await);
        }
        assert_eq!(a.active_path_drivers(), 8);
        assert_eq!(b.active_path_drivers(), 8);
        // Concurrent calls on cloned handles share the same shutdown barrier.
        let a_clone = a.clone();
        tokio::join!(a.close(), a_clone.close(), b.close());
        assert_eq!(a.active_path_drivers(), 0);
        assert_eq!(b.active_path_drivers(), 0);
        assert!(a.accept().await.is_none());
        assert!(a.connect(b.addr(), rds_core::ALPN).await.is_err());
        assert_eq!(a.active_path_drivers(), 0);
        assert!(held.iter().all(|(client, server)| {
            client.inner().close_reason().is_some() && server.inner().close_reason().is_some()
        }));
    })
    .await
    .expect("endpoint close must join its policy tasks");
}
