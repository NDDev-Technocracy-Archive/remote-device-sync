//! v3 uni demux lifecycle: a dead connection must end every claimed
//! `UniStreams` inbox (`recv` → `None`) — `rds-sync::receive` and the
//! desktop frame task both rely on end-of-stream to exit instead of
//! parking forever on a dead peer.

use std::time::Duration;

use rds_net::{EndpointConfig, bind_endpoint};

/// Connected pair over the default (iroh) backend, discovery off —
/// returns both connections and both endpoints.
async fn connected_pair() -> (
    rds_net::Connection,
    rds_net::Connection,
    rds_net::Endpoint,
    rds_net::Endpoint,
) {
    // Router lifecycle is local to this pair. Wildcard binding advertises
    // unrelated network interfaces, making this fixture depend on their routes.
    let cfg = || EndpointConfig {
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    };
    let server = bind_endpoint(cfg()).await.unwrap();
    let client = bind_endpoint(cfg()).await.unwrap();

    let server_ep = server.clone();
    let accept = tokio::spawn(async move { server_ep.accept().await.unwrap().await.unwrap() });

    let conn = client.connect(server.addr(), rds_core::ALPN).await.unwrap();
    let server_conn = accept.await.unwrap();
    (conn, server_conn, server, client)
}

/// `recv` must yield `None` once the connection dies — the demux exits
/// and its registered senders drop, ending the inbox.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uni_recv_ends_when_connection_dies() {
    let (conn, _peer, _server, _client) = connected_pair().await;
    let mut uni = conn.uni_streams(rds_core::UniHello::Sync).unwrap();

    conn.close(0u32.into(), b"done");
    tokio::time::sleep(Duration::from_millis(200)).await;

    match tokio::time::timeout(Duration::from_secs(10), uni.recv()).await {
        Ok(None) => {}
        Ok(Some(_)) => panic!("uni stream arrived after close"),
        Err(_) => panic!("uni.recv() hung after connection death"),
    }
}

/// A claim on an already-dead connection ends immediately — the demux
/// respawns, fails `accept_uni`, and drops the fresh sender.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uni_streams_on_dead_connection_end() {
    let (conn, _peer, _server, _client) = connected_pair().await;
    conn.close(0u32.into(), b"done");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut uni = conn.uni_streams(rds_core::UniHello::Desktop).unwrap();
    match tokio::time::timeout(Duration::from_secs(10), uni.recv()).await {
        Ok(None) => {}
        Ok(Some(_)) => panic!("uni stream arrived on dead connection"),
        Err(_) => panic!("uni.recv() hung on dead connection"),
    }
}

/// A peer that opens a uni stream and never writes the `UniHello` tag
/// must not stall routing of the streams behind it — each accepted
/// stream gets its own tag-read task.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_tag_does_not_block_routing() {
    let (conn, server_conn, _server, _client) = connected_pair().await;

    let mut uni = conn.uni_streams(rds_core::UniHello::Desktop).unwrap();

    // Stream one: opened, held open, tag never written.
    let mut stalled = server_conn.open_uni().await.unwrap();
    stalled.write_all(&[0]).await.unwrap();

    // Stream two: tagged properly — must route despite the stalled
    // predecessor sitting at the head of the accept queue.
    let mut tagged = server_conn.open_uni().await.unwrap();
    rds_net::write_frame(&mut tagged, &rds_core::UniHello::Desktop)
        .await
        .unwrap();

    match tokio::time::timeout(Duration::from_secs(5), uni.recv()).await {
        Ok(Some(_)) => {}
        Ok(None) => panic!("inbox ended while connection is alive"),
        Err(_) => panic!("tagged stream stuck behind a stalled tag"),
    }
}
