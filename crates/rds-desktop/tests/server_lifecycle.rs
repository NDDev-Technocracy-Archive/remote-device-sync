//! Canceling the serving future must release its capture source and workers.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rds_core::{Codec, DesktopCaps, HelloAck, StreamHello};
use rds_desktop::client::DesktopSession;
use rds_desktop::{
    FrameProducer, Produced, ProducerControls, SessionClock, SessionConfig, SyntheticProducer,
    serve_desktop_with,
};
use rds_net::{Backend, EndpointConfig};
use rds_net::{read_frame, write_frame};

struct Source {
    inner: SyntheticProducer,
    stopped: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl FrameProducer for Source {
    fn produce(
        &mut self,
        seq: u64,
        controls: &ProducerControls,
        clock: &SessionClock,
    ) -> Option<Produced> {
        if self.stopped.load(Ordering::SeqCst) {
            None
        } else {
            self.inner.produce(seq, controls, clock)
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_serving_session_releases_capture_without_closing_connection() {
    for backend in [Backend::Iroh, Backend::Noq] {
        let config = EndpointConfig {
            backend,
            discovery: false,
            bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
            ..Default::default()
        };
        let client = rds_net::bind_endpoint(config.clone()).await.unwrap();
        let server = rds_net::bind_endpoint(config).await.unwrap();
        let (a, b) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(client.connect(server.addr(), rds_core::ALPN), async {
                server.accept().await.unwrap().await
            })
        })
        .await
        .unwrap();
        let (a, b) = (a.unwrap(), b.unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let source = Source {
            inner: SyntheticProducer::new(60, 16, 16, 256),
            stopped: stopped.clone(),
            dropped: dropped.clone(),
        };
        let serving_conn = b.clone();
        let serving = tokio::spawn(async move {
            let (mut send, mut recv) = serving_conn.accept_bi().await.unwrap();
            let greeting = read_frame(&mut recv).await.unwrap();
            let frame_route = match &greeting {
                StreamHello::DesktopV2 { session, .. } => {
                    Some(rds_core::UniHello::DesktopFrames { id: *session })
                }
                _ => None,
            };
            let (StreamHello::Desktop(hello) | StreamHello::DesktopV2 { hello, .. }) = greeting
            else {
                panic!("expected desktop hello");
            };
            write_frame(
                &mut send,
                &HelloAck::Desktop(DesktopCaps {
                    displays: vec![],
                    codecs: vec![Codec::H264],
                }),
            )
            .await
            .unwrap();
            serve_desktop_with(
                serving_conn,
                send,
                recv,
                hello,
                SessionConfig {
                    view_only: true,
                    producer: Some(Box::new(source)),
                    frame_route,
                    ..Default::default()
                },
            )
            .await
        });
        let mut session = DesktopSession::connect(&a, 0, 60, Codec::H264)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), session.frame_headers.recv())
            .await
            .unwrap()
            .unwrap();
        serving.abort();
        assert!(serving.await.unwrap_err().is_cancelled());
        let released = tokio::time::timeout(Duration::from_secs(2), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        // Also permits bounded cleanup when running against the old code.
        stopped.store(true, Ordering::SeqCst);
        drop(session);
        let mut next = a.open_uni().await.unwrap();
        next.write_all(b"still usable").await.unwrap();
        next.finish().unwrap();
        let mut recv = tokio::time::timeout(Duration::from_secs(3), b.accept_uni())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recv.read_to_end(12).await.unwrap(), b"still usable");
        a.close(0u32.into(), b"done");
        client.close().await;
        server.close().await;
        assert!(
            released.is_ok(),
            "capture survived cancellation on {backend:?}"
        );
    }
}
