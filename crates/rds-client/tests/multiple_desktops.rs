//! Simultaneous peer and monitor channels retain independent lifetime/routes.
use rds_client::local::{Client, ManagedMessage, Prepared, Server};
use rds_core::{
    Codec, DesktopCaps, DesktopEvent, DesktopHello, HelloAck, InputEvent, InputKind, StreamHello,
    UniHello,
    local::{Command, Reply},
};
use rds_desktop::{DesktopError, InputSink, SessionConfig, SyntheticProducer};
use rds_net::{Backend, Endpoint, EndpointConfig, read_frame, write_frame};
use std::{os::unix::fs::DirBuilderExt, time::Duration};
struct ScopedInput(u32);
impl InputSink for ScopedInput {
    fn inject(&mut self, event: &InputEvent) -> Result<(), DesktopError> {
        assert_eq!(event.display_id, self.0);
        Ok(())
    }
}
async fn endpoint(backend: Backend) -> Endpoint {
    rds_net::bind_endpoint(EndpointConfig {
        backend,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        discovery: false,
        ..Default::default()
    })
    .await
    .unwrap()
}
fn serve(endpoint: Endpoint) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        while let Some(incoming) = endpoint.accept().await {
            let conn = incoming.await.unwrap();
            connections.spawn(async move {
                let mut streams = tokio::task::JoinSet::new();
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let conn = conn.clone();
                    streams.spawn(async move {
                        match read_frame::<_, StreamHello>(&mut recv).await.unwrap() {
                            StreamHello::Ping { nonce } => {
                                write_frame(&mut send, &HelloAck::Ok).await.unwrap();
                                send.write_all(&nonce.to_be_bytes()).await.unwrap();
                                send.finish().unwrap();
                            }
                            StreamHello::DesktopV5 {
                                session,
                                hello,
                                payload_receipts,
                                clipboard,
                                ..
                            } => {
                                assert!(clipboard);
                                let display = hello.display;
                                write_frame(
                                    &mut send,
                                    &HelloAck::DesktopV5(DesktopCaps {
                                        displays: vec![],
                                        codecs: vec![Codec::H264],
                                    }),
                                )
                                .await
                                .unwrap();
                                rds_desktop::serve_desktop_with(
                                    conn,
                                    send,
                                    recv,
                                    hello,
                                    SessionConfig {
                                        frame_route: Some(UniHello::DesktopFrames { id: session }),
                                        payload_receipts,
                                        clipboard: rds_desktop::ServingClipboard::Text {
                                            reverse: true,
                                        },
                                        input_sink: Some(Box::new(ScopedInput(display))),
                                        producer: Some(Box::new(SyntheticProducer::new(
                                            5,
                                            32 + display * 2,
                                            32,
                                            64,
                                        ))),
                                        ..Default::default()
                                    },
                                )
                                .await
                                .unwrap();
                            }
                            _ => panic!("unexpected service"),
                        }
                    });
                }
                streams.shutdown().await;
            });
        }
        connections.shutdown().await;
    })
}
async fn connect(client: &Client, remote: &Endpoint) -> rds_core::local::SessionId {
    let Reply::Connected(session) = client
        .request(Command::Connect {
            target: rds_net::Ticket::of(remote).to_string(),
            grant: None,
        })
        .await
        .unwrap()
    else {
        panic!("missing managed session")
    };
    session
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_one_monitor_preserves_another_monitor_and_another_device() {
    for backend in [
        Backend::Iroh,
        #[cfg(feature = "transport-noq")]
        Backend::Noq,
    ] {
        let local = endpoint(backend).await;
        let device_a = endpoint(backend).await;
        let device_b = endpoint(backend).await;
        let a = serve(device_a.clone());
        let b = serve(device_b.clone());
        let directory = std::path::Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!(
                "rds-multiple-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let mut manager = Server::start(
            Some(Prepared::bind(&directory).await.unwrap()),
            local.clone(),
            None,
        );
        let client = Client::new(&directory);
        let first = connect(&client, &device_a).await;
        let other = connect(&client, &device_b).await;
        let hello = |display| DesktopHello {
            display,
            max_fps: 5,
            codec: Codec::H264,
            input_acks: true,
        };
        let (one, mut events_one) = client
            .desktop_features(Some(first), hello(0), 0, true, true)
            .await
            .unwrap();
        let (mut two, mut events_two) = client
            .desktop_features(Some(first), hello(1), 0, true, true)
            .await
            .unwrap();
        let (mut third, mut events_third) = client
            .desktop_features(Some(other), hello(2), 0, true, true)
            .await
            .unwrap();
        client
            .request(Command::Select { session: other })
            .await
            .unwrap();
        drop(one);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), events_one.recv())
                .await
                .unwrap()
                .is_none()
        );
        for (channel, events, width) in [
            (&mut two, &mut events_two, 34),
            (&mut third, &mut events_third, 36),
        ] {
            assert!(
                matches!(tokio::time::timeout(Duration::from_secs(2), channel.recv()).await.unwrap().unwrap(), Some(ManagedMessage::Frame(frame)) if frame.header.width == width)
            );
            let seq = channel
                .send_input(InputKind::KeyDown { code: 30 })
                .await
                .unwrap();
            assert!(
                matches!(tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().unwrap().unwrap(), DesktopEvent::InputAck { seq: ack, .. } if ack == seq)
            );
            channel.heartbeat().await.unwrap();
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(2), events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                DesktopEvent::Heartbeat { .. }
            ));
        }
        assert_eq!(
            client.snapshot().await.unwrap().endpoint,
            local.id().to_string()
        );
        drop(two);
        drop(third);
        manager.close().await.unwrap();
        local.close().await;
        device_a.close().await;
        device_b.close().await;
        tokio::time::timeout(Duration::from_secs(2), a)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), b)
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
