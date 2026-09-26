//! Hostile/incomplete desktop streams over both real transport backends.
use std::time::Duration;

use rds_core::{
    Codec, DesktopCaps, DesktopControl, DesktopEvent, FrameHeader, HelloAck, StreamHello, UniHello,
    read_frame, write_frame,
};
use rds_desktop::client::DesktopSession;
use rds_net::{Backend, Connection, Endpoint, EndpointConfig, RecvStream, SendStream};

async fn pair(backend: Backend) -> (Endpoint, Endpoint, Connection, Connection) {
    let config = EndpointConfig {
        backend,
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    };
    let client = rds_net::bind_endpoint(config.clone()).await.unwrap();
    let server = rds_net::bind_endpoint(config).await.unwrap();
    let (a, b) = tokio::join!(client.connect(server.addr(), rds_core::ALPN), async {
        server.accept().await.unwrap().await
    });
    (client, server, a.unwrap(), b.unwrap())
}

async fn session(a: &Connection, b: &Connection) -> (DesktopSession, SendStream, RecvStream) {
    let (client, streams) = tokio::join!(DesktopSession::connect(a, 0, 30, Codec::H264), async {
        let (mut send, mut recv) = b.accept_bi().await.unwrap();
        assert!(matches!(
            read_frame(&mut recv).await.unwrap(),
            StreamHello::Desktop(_)
        ));
        write_frame(
            &mut send,
            &HelloAck::Desktop(DesktopCaps {
                displays: vec![],
                codecs: vec![Codec::H264],
            }),
        )
        .await
        .unwrap();
        (send, recv)
    });
    (client.unwrap(), streams.0, streams.1)
}

async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("session resources did not reach the expected state");
}

async fn tagged(b: &Connection) -> SendStream {
    let mut stream = b.open_uni().await.unwrap();
    write_frame(&mut stream, &UniHello::Desktop).await.unwrap();
    stream
}

fn header(seq: u64) -> FrameHeader {
    FrameHeader {
        seq,
        capture_ts_ms: 0,
        encode_done_ts_ms: 0,
        send_ts_ms: 0,
        keyframe: true,
        codec: Codec::H264,
        width: 640,
        height: 480,
    }
}

async fn rejected_header(b: &Connection, h: FrameHeader) {
    let mut stream = tagged(b).await;
    write_frame(&mut stream, &h).await.unwrap();
    // No body or FIN: refusal must happen from the header alone.
    assert!(
        tokio::time::timeout(Duration::from_secs(3), stream.stopped())
            .await
            .expect("invalid header retained its stream")
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readers_are_bounded_and_session_owns_all_streams() {
    for backend in [Backend::Iroh, Backend::Noq] {
        tokio::time::timeout(Duration::from_secs(20), async {
            let (client_ep, server_ep, a, b) = pair(backend).await;
            let (mut client, mut control_send, mut control_recv) = session(&a, &b).await;

            // A duplicate is rejected locally before another hello appears.
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(500),
                    DesktopSession::connect(&a, 0, 30, Codec::H264)
                )
                .await
                .expect("duplicate claim started a second handshake")
                .is_err()
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(30), b.accept_bi())
                    .await
                    .is_err()
            );

            let mut stalled = Vec::new();
            for _ in 0..client.receive_stats().max_in_flight {
                let mut stream = tagged(&b).await;
                stream.write_all(&[0]).await.unwrap(); // incomplete length prefix
                stalled.push(stream);
            }
            until(|| client.receive_stats().in_flight == stalled.len()).await;
            let stats = client.receive_stats();
            assert!(stats.global_in_flight <= stats.global_max_in_flight);
            let mut excess = tagged(&b).await;
            excess.write_all(&[0]).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(3), excess.stopped())
                    .await
                    .expect("over-budget reader was not refused")
                    .unwrap()
                    .is_some()
            );
            assert_eq!(client.receive_stats().in_flight, stalled.len());

            // Video saturation cannot block the independent control leg.
            let seq = client.heartbeat().await.unwrap();
            let DesktopControl::Heartbeat { seq: got, ts_ms } =
                read_frame(&mut control_recv).await.unwrap()
            else {
                panic!("heartbeat lost under video pressure")
            };
            assert_eq!(seq, got);
            write_frame(&mut control_send, &DesktopEvent::Heartbeat { seq, ts_ms })
                .await
                .unwrap();
            assert!(matches!(
                client.events.recv().await,
                Some(DesktopEvent::Heartbeat { .. })
            ));

            drop(client);
            for stream in stalled {
                assert!(
                    tokio::time::timeout(Duration::from_secs(3), stream.stopped())
                        .await
                        .expect("frame reader survived session drop")
                        .unwrap()
                        .is_some()
                );
            }
            assert!(
                read_frame::<_, DesktopControl>(&mut control_recv)
                    .await
                    .is_err()
            );
            assert!(control_send.stopped().await.unwrap().is_some());
            until(|| a.uni_routing_stats().routes == 0).await;
            drop((control_send, control_recv));

            let (mut client, mut control_send, mut control_recv) = session(&a, &b).await;
            rejected_header(&b, header(u64::MAX)).await;
            let mut oversized = header(0);
            oversized.width = u32::MAX;
            rejected_header(&b, oversized).await;
            let mut empty = header(0);
            empty.height = 0;
            rejected_header(&b, empty).await;
            assert!(client.frame_headers.is_empty());
            let mut good = tagged(&b).await;
            write_frame(&mut good, &header(0)).await.unwrap();
            good.write_all(b"synthetic payload for header tap")
                .await
                .unwrap();
            good.finish().unwrap();
            assert_eq!(client.frame_headers.recv().await.unwrap().seq, 0);

            let mut stalled = tagged(&b).await;
            stalled.write_all(&[0]).await.unwrap();
            until(|| client.receive_stats().in_flight == 1).await;
            // Control EOF must end the session while its public handle lives.
            control_send.finish().unwrap();
            until(|| a.uni_routing_stats().routes == 0).await;
            assert!(client.frame_headers.recv().await.is_none());
            assert!(client.heartbeat().await.is_err());
            assert!(stalled.stopped().await.unwrap().is_some());
            while read_frame::<_, DesktopControl>(&mut control_recv)
                .await
                .is_ok()
            {}
            drop(client);

            // Canceling a handshake owns/reset its streams and releases route.
            let connect = async {
                tokio::time::timeout(
                    Duration::from_millis(100),
                    DesktopSession::connect(&a, 0, 30, Codec::H264),
                )
                .await
            };
            let (result, (send, mut recv)) = tokio::join!(connect, async {
                let (send, mut recv) = b.accept_bi().await.unwrap();
                assert!(matches!(
                    read_frame(&mut recv).await.unwrap(),
                    StreamHello::Desktop(_)
                ));
                (send, recv) // deliberately withhold ACK
            });
            assert!(result.is_err());
            assert!(read_frame::<_, DesktopControl>(&mut recv).await.is_err());
            assert!(send.stopped().await.unwrap().is_some());
            until(|| a.uni_routing_stats().routes == 0).await;

            // The transport remains available to unrelated service streams.
            let (mut send, _) = a.open_bi().await.unwrap();
            send.write_all(b"still connected").await.unwrap();
            send.finish().unwrap();
            let (_, mut recv) = b.accept_bi().await.unwrap();
            assert_eq!(recv.read_to_end(32).await.unwrap(), b"still connected");
            client_ep.close().await;
            server_ep.close().await;
        })
        .await
        .unwrap_or_else(|_| panic!("{backend:?} client lifecycle exceeded fixture budget"));
    }
}
