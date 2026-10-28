//! Real Iroh actor over a bounded controllable custom link and UDP fallback.
use iroh::TransportAddr;
use iroh::endpoint::transports::{
    CustomEndpoint, CustomSender, CustomTransport, PathSelection, PathSelectionContext,
    PathSelector, RecvInfo, Transmit,
};
use iroh_base::CustomAddr;
use std::collections::HashMap;
use std::io::{self, IoSliceMut};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Debug)]
struct Packet {
    from: CustomAddr,
    data: Vec<u8>,
}
type Fabric = Arc<Mutex<HashMap<CustomAddr, mpsc::Sender<Packet>>>>;
#[derive(Debug)]
struct Link {
    addr: CustomAddr,
    fabric: Fabric,
    drop_packets: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}
impl CustomTransport for Link {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        let (tx, rx) = mpsc::channel(64);
        self.fabric.lock().unwrap().insert(self.addr.clone(), tx);
        Ok(Box::new(LinkEndpoint {
            sender: Arc::new(LinkSender {
                addr: self.addr.clone(),
                fabric: self.fabric.clone(),
                drop_packets: self.drop_packets.clone(),
                dropped: self.dropped.clone(),
            }),
            rx,
            addresses: n0_watcher::Watchable::new(vec![self.addr.clone()]),
        }))
    }
}
#[derive(Debug)]
struct LinkEndpoint {
    sender: Arc<LinkSender>,
    rx: mpsc::Receiver<Packet>,
    addresses: n0_watcher::Watchable<Vec<CustomAddr>>,
}
impl Drop for LinkEndpoint {
    fn drop(&mut self) {
        self.sender.fabric.lock().unwrap().remove(&self.sender.addr);
    }
}
impl CustomEndpoint for LinkEndpoint {
    fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>> {
        self.addresses.watch()
    }
    fn create_sender(&self) -> Arc<dyn CustomSender> {
        self.sender.clone()
    }
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        metas: &mut [noq::udp::RecvMeta],
        infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(packet)) => {
                assert!(packet.data.len() <= bufs[0].len());
                bufs[0][..packet.data.len()].copy_from_slice(&packet.data);
                let mut meta = noq::udp::RecvMeta::default();
                meta.len = packet.data.len();
                meta.stride = packet.data.len();
                metas[0] = meta;
                infos[0] = RecvInfo::new(packet.from, Some(self.sender.addr.clone()));
                Poll::Ready(Ok(1))
            }
            Poll::Ready(None) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}
#[derive(Debug)]
struct LinkSender {
    addr: CustomAddr,
    fabric: Fabric,
    drop_packets: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}
impl CustomSender for LinkSender {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
        addr.id() == self.addr.id()
    }
    fn poll_send(
        &self,
        _cx: &mut Context<'_>,
        dst: &CustomAddr,
        src: Option<&CustomAddr>,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        if self.drop_packets.load(Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Poll::Ready(Ok(()));
        }
        let sender = self
            .fabric
            .lock()
            .unwrap()
            .get(dst)
            .cloned()
            .ok_or(io::ErrorKind::NotConnected);
        let result = sender.and_then(|sender| {
            sender
                .try_send(Packet {
                    from: src.unwrap_or(&self.addr).clone(),
                    data: transmit.contents.to_vec(),
                })
                .map_err(|_| io::ErrorKind::Other)
        });
        Poll::Ready(result.map_err(Into::into))
    }
}

#[derive(Debug)]
struct SetupSelector(Arc<AtomicBool>);
fn maintain_probe_proof(ctx: &PathSelectionContext<'_>) {
    for path in ctx.paths() {
        if path
            .congestion_state()
            .and_then(crate::ack_progress::snapshot)
            .is_some_and(|proof| proof.needs_probe())
        {
            path.ping();
        }
    }
}
impl PathSelector for SetupSelector {
    fn maintain_standby_paths(&self) -> bool {
        true
    }
    fn refresh_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(1))
    }
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        if self.0.load(Ordering::Acquire) {
            return super::latency::LatencySelector.select(ctx);
        }
        maintain_probe_proof(ctx);
        // Establish the failure precondition deterministically. On a busy CI
        // runner the genuinely faster UDP path may otherwise win before loss
        // is injected, leaving this a test of setup timing rather than recovery.
        let mut choice = PathSelection::none();
        if let Some(path) = ctx.paths().find(|path| {
            matches!(
                path.network_path().remote(),
                iroh::endpoint::transports::Addr::Custom(_)
            )
        }) {
            choice.set(&path);
        }
        choice
    }
}

async fn endpoint(link: Link, armed: Arc<AtomicBool>) -> iroh::Endpoint {
    endpoint_with_selector(link, Arc::new(SetupSelector(armed))).await
}

async fn endpoint_with_selector(link: Link, selector: Arc<dyn PathSelector>) -> iroh::Endpoint {
    let mut transport = iroh::endpoint::QuicTransportConfig::builder()
        .congestion_controller_factory(crate::ack_progress::factory(
            crate::CongestionControl::Cubic.factory(),
            Arc::new(|| tokio::time::Instant::now().into_std()),
        ));
    transport = transport
        .initial_mtu(1200)
        .min_mtu(1200)
        .mtu_discovery_config(None)
        .prefer_same_path_acks(true)
        .enable_segmentation_offload(false);
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
        .unwrap()
        .portmapper_config(iroh::endpoint::PortmapperConfig::Disabled)
        .add_custom_transport(Arc::new(link))
        .path_selector(selector)
        .transport_config(transport.build())
        .alpns(vec![rds_core::ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    let sockets = endpoint.bound_sockets();
    assert_eq!(sockets.len(), 1);
    assert!(sockets[0].ip().is_loopback() && sockets[0].port() != 0);
    // Loopback is normally excluded from peer address publication. Both
    // directions of this isolated fixture must advertise their actual socket;
    // seeding only the dialer's target does not configure the accepting peer.
    endpoint.add_external_addr(sockets[0]).await;
    endpoint
}

fn loopback_target(endpoint: &iroh::Endpoint, custom: CustomAddr) -> iroh::EndpointAddr {
    let sockets = endpoint.bound_sockets();
    assert_eq!(sockets.len(), 1, "fixture must bind one loopback socket");
    assert!(sockets[0].ip().is_loopback() && sockets[0].port() != 0);
    // These fixtures require an already configured UDP sibling. Seed its
    // real bound address instead of depending on asynchronous peer-address
    // advertisement after a custom-only bootstrap. The selector still proves
    // that the custom link is selected before blackholing its stream work.
    iroh::EndpointAddr::from_parts(
        endpoint.id(),
        [TransportAddr::Custom(custom), TransportAddr::Ip(sockets[0])],
    )
}

#[derive(Debug)]
struct StandbySelector {
    phase: AtomicU8,
    retired: AtomicBool,
    confirmed: AtomicBool,
    ip_drained: AtomicBool,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_accept_observes_paths_established_before_actor_registration() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let fabric = Fabric::default();
        let link = |label: &[u8]| Link {
            addr: CustomAddr::from_parts(0x7264736c61746561, label),
            fabric: fabric.clone(),
            drop_packets: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
        };
        let a = endpoint(link(b"a"), Arc::new(AtomicBool::new(false))).await;
        let b_link = link(b"b");
        let target_custom = b_link.addr.clone();
        let b = endpoint(b_link, Arc::new(AtomicBool::new(false))).await;
        // Accept the QUIC handshake, but deliberately defer the Iroh future
        // that registers this connection with its path-observation actor.
        #[allow(clippy::async_yields_async)]
        // Returning the unpolled accepting future is the regression trigger.
        let accept_without_registration = async { b.accept().await.unwrap().accept().unwrap() };
        let (client, pending_server) = tokio::join!(
            a.connect(loopback_target(&b, target_custom), rds_core::ALPN),
            accept_without_registration
        );
        let client = client.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let paths = client.paths();
                if paths.iter().any(|p| p.is_ip()) && paths.iter().any(|p| !p.is_ip()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("client did not establish both paths before server registration");
        let server = pending_server.await.unwrap();
        let observed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let paths = server.paths();
                if paths.iter().any(|p| p.is_ip()) && paths.iter().any(|p| !p.is_ip()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let snapshot = server
            .paths()
            .iter()
            .map(|p| (p.id(), p.is_ip()))
            .collect::<Vec<_>>();
        tokio::join!(a.close(), b.close());
        assert!(
            observed.is_ok(),
            "late observer lost established paths: {snapshot:?}"
        );
    })
    .await
    .expect("late path observation fixture stalled");
}

impl PathSelector for StandbySelector {
    fn refresh_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(1))
    }
    fn maintain_standby_paths(&self) -> bool {
        true
    }
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let phase = self.phase.load(Ordering::Acquire);
        if phase == 2 {
            if std::env::var_os("RDS_TEST_STANDBY_TRACE").is_some() {
                for path in ctx.paths() {
                    let proof = path
                        .congestion_state()
                        .and_then(crate::ack_progress::snapshot);
                    let stats = path.stats();
                    eprintln!(
                        "standby selector route={:?} current={} ip={} stream_tx={:?} stream_work={:?} pending_ms={:?} confirmation_ms={:?} stalled={:?} carrier_acks={:?}",
                        path.network_path(),
                        Some(path.network_path()) == ctx.current(),
                        matches!(
                            path.network_path().remote(),
                            iroh::endpoint::transports::Addr::Ip(_)
                        ),
                        stats.map(|s| s.frame_tx.stream),
                        stats.map(|s| s.unacknowledged_stream_frames),
                        proof.and_then(|s| s.pending_age()).map(|d| d.as_millis()),
                        proof
                            .and_then(|s| s.confirmation_age())
                            .map(|d| d.as_millis()),
                        proof.zip(stats).map(|(p, s)| p.stalled(s.rtt)),
                        stats.map(|s| s.frame_rx.path_acks)
                    );
                }
            }
            return super::latency::LatencySelector.select(ctx);
        }
        maintain_probe_proof(ctx);
        let ip = ctx.paths().find(|p| {
            matches!(
                p.network_path().remote(),
                iroh::endpoint::transports::Addr::Ip(_)
            )
        });
        let custom = ctx.paths().find(|p| {
            matches!(
                p.network_path().remote(),
                iroh::endpoint::transports::Addr::Custom(_)
            )
        });
        let mut choice = PathSelection::none();
        if phase == 0 {
            if let Some(path) = custom {
                choice.set(&path);
            }
        } else if let Some(ip) = ip {
            if ip
                .congestion_state()
                .and_then(crate::ack_progress::snapshot)
                .zip(ip.stats())
                .is_some_and(|(_, stats)| !stats.unacknowledged_stream_frames)
            {
                self.ip_drained.store(true, Ordering::Release);
            }
            if let Some(custom) = custom {
                if !self.retired.load(Ordering::Acquire) {
                    if custom.abandon_with_fallback(&ip) {
                        self.retired.store(true, Ordering::Release);
                    }
                } else if custom
                    .congestion_state()
                    .and_then(crate::ack_progress::snapshot)
                    .zip(custom.stats())
                    .is_some_and(|(proof, stats)| proof.confirmed(stats.rtt))
                {
                    self.confirmed.store(true, Ordering::Release);
                } else {
                    custom.ping();
                }
            }
            choice.set(&ip);
        }
        choice
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_iroh_actor_reopens_a_retired_standby_before_the_next_failure() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let fabric = Fabric::default();
        let selector = Arc::new(StandbySelector {
            phase: AtomicU8::new(0),
            retired: AtomicBool::new(false),
            confirmed: AtomicBool::new(false),
            ip_drained: AtomicBool::new(false),
        });
        let dropping = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let a_addr = CustomAddr::from_parts(0x7264737374616e64, b"a");
        let b_addr = CustomAddr::from_parts(0x7264737374616e64, b"b");
        let a = endpoint_with_selector(
            Link {
                addr: a_addr,
                fabric: fabric.clone(),
                drop_packets: Arc::new(AtomicBool::new(false)),
                dropped: Arc::new(AtomicU64::new(0)),
            },
            selector.clone(),
        )
        .await;
        let b = endpoint_with_selector(
            Link {
                addr: b_addr.clone(),
                fabric,
                drop_packets: dropping.clone(),
                dropped: dropped.clone(),
            },
            selector.clone(),
        )
        .await;
        let target = loopback_target(&b, b_addr);
        let (client, server) = tokio::join!(a.connect(target, rds_core::ALPN), async {
            b.accept().await.unwrap().await
        });
        let (client, server) = (client.unwrap(), server.unwrap());
        let (mut request, mut reply) = client.open_bi().await.unwrap();
        request.write_all(b"warm").await.unwrap();
        let (mut send, mut recv) = server.accept_bi().await.unwrap();
        let mut warm = [0; 4];
        recv.read_exact(&mut warm).await.unwrap();
        send.write_all(b"ok").await.unwrap();
        let mut ack = [0; 2];
        reply.read_exact(&mut ack).await.unwrap();
        let original = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let paths = server.paths();
                if paths.iter().any(|p| p.is_ip())
                    && let Some(path) = paths.iter().find(|p| !p.is_ip())
                {
                    break path.id();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if original.is_err() {
            for (side, conn) in [("client", &client), ("server", &server)] {
                for path in conn.paths().iter() {
                    eprintln!(
                        "standby setup {side}: id={} ip={} selected={} rtt={:?}",
                        path.id(),
                        path.is_ip(),
                        path.is_selected(),
                        path.rtt()
                    );
                }
            }
            eprintln!("standby setup client addresses: {:?}", a.addr());
            eprintln!("standby setup server addresses: {:?}", b.addr());
            eprintln!(
                "standby setup client remote: {:?}",
                a.remote_info(b.id()).await
            );
            eprintln!(
                "standby setup server remote: {:?}",
                b.remote_info(a.id()).await
            );
        }
        let original = original.expect("both UDP and custom paths were not established");
        selector.phase.store(1, Ordering::Release);
        let restored = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let paths = server.paths();
                if selector.retired.load(Ordering::Acquire)
                    && paths.iter().any(|p| p.is_selected() && p.is_ip())
                    && let Some(path) = paths.iter().find(|p| !p.is_ip() && p.id() != original)
                {
                    break path.id();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("known standby was not reopened on the existing connection");
        selector.confirmed.store(false, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !selector.confirmed.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
        })
        .await
        .expect("restored standby had no positive ACK proof");
        // The IP is an actual data-bearing former route, not just a probe-only
        // fixture. Complete and acknowledge its work before making it standby.
        request.write_all(b"work").await.unwrap();
        recv.read_exact(&mut warm).await.unwrap();
        assert_eq!(&warm, b"work");
        send.write_all(b"ok").await.unwrap();
        reply.read_exact(&mut ack).await.unwrap();
        selector.ip_drained.store(false, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !selector.ip_drained.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("active IP work did not drain before standby transition");
        selector.phase.store(0, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !server
                .paths()
                .get(restored)
                .is_some_and(|p| p.is_selected())
            {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
        })
        .await
        .expect("restored custom path was not selectable");
        assert!(
            server.paths().iter().any(|path| path.is_ip()),
            "selecting a non-IP path removed every direct standby"
        );
        let before = server
            .paths()
            .get(restored)
            .unwrap()
            .stats()
            .frame_tx
            .stream;
        dropping.store(true, Ordering::Release);
        send.write_all(b"pending reliable bytes").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while dropped.load(Ordering::Relaxed) == 0
                || server
                    .paths()
                    .get(restored)
                    .unwrap()
                    .stats()
                    .frame_tx
                    .stream
                    <= before
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("restored path did not emit/drop pending data");
        selector.phase.store(2, Ordering::Release);
        let mut body = [0; 22];
        // Retirement now waits for `max(8 RTT, 2s)` without ACK progress before recovery.
        let result =
            tokio::time::timeout(Duration::from_secs(5), reply.read_exact(&mut body)).await;
        if result.is_err() {
            for path in server.paths().iter() {
                eprintln!(
                    "standby recovery path {} ip={} selected={} rtt={:?} stream_tx={} rx={}",
                    path.id(),
                    path.is_ip(),
                    path.is_selected(),
                    path.rtt(),
                    path.stats().frame_tx.stream,
                    path.stats().udp_rx.bytes
                );
            }
        }
        tokio::join!(a.close(), b.close());
        result
            .expect("existing stream did not survive the later blackhole")
            .unwrap();
        assert_eq!(&body, b"pending reliable bytes");
    })
    .await
    .expect("bounded standby-restoration fixture did not terminate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_iroh_actor_establishes_a_standby_that_was_blocked_at_startup() {
    tokio::time::timeout(Duration::from_secs(25), async {
        let fabric = Fabric::default();
        let blocked = Arc::new(AtomicBool::new(true));
        let dropped = Arc::new(AtomicU64::new(0));
        let mut endpoints = Vec::new();
        for name in [b"a", b"b"] {
            endpoints.push(
                endpoint_with_selector(
                    Link {
                        addr: CustomAddr::from_parts(0x726473636f6c6431, name),
                        fabric: fabric.clone(),
                        drop_packets: blocked.clone(),
                        dropped: dropped.clone(),
                    },
                    Arc::new(super::latency::LatencySelector),
                )
                .await,
            );
        }
        let a = &endpoints[0];
        let b = &endpoints[1];
        // Include the configured standby even before its local watcher has
        // published the first address snapshot, like a complete bootstrap ticket.
        let mut target = b.addr();
        target
            .addrs
            .insert(TransportAddr::Custom(CustomAddr::from_parts(
                0x726473636f6c6431,
                b"b",
            )));
        let (client, server) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(a.connect(target, rds_core::ALPN), async {
                b.accept().await.unwrap().await
            })
        })
        .await
        .expect("the blocked standby delayed the healthy initial route");
        let (client, server) = (client.unwrap(), server.unwrap());
        let (mut request, mut response) = client.open_bi().await.unwrap();
        request.write_all(b"before").await.unwrap();
        let (mut reply, mut input) = server.accept_bi().await.unwrap();
        let mut body = [0; 6];
        input.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"before");
        reply.write_all(b"ok").await.unwrap();
        let mut ack = [0; 2];
        response.read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack, b"ok");

        // Miss several initial validation probes while the live IP route keeps
        // this same connection and stream usable.
        tokio::time::sleep(Duration::from_secs(8)).await;
        assert!(dropped.load(Ordering::Relaxed) > 0);
        assert!(client.paths().iter().all(|path| path.is_ip()));
        blocked.store(false, Ordering::Release);
        let restored_at = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !client.paths().iter().any(|path| !path.is_ip())
                || !server.paths().iter().any(|path| !path.is_ip())
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the restored initial standby did not validate within five seconds");
        eprintln!(
            "initial standby validation after restore: {} ms",
            restored_at.elapsed().as_millis()
        );
        request.write_all(b"after!").await.unwrap();
        input.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"after!");
        reply.write_all(b"ok").await.unwrap();
        response.read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack, b"ok");
        assert!(client.close_reason().is_none());
        assert!(server.close_reason().is_none());
        tokio::join!(a.close(), b.close());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_iroh_actor_retires_blackholed_preferred_link_and_delivers_pending_stream_bytes() {
    tokio::time::timeout(Duration::from_secs(15),async {
        let fabric=Fabric::default();
        let armed=Arc::new(AtomicBool::new(false));
        let dropping=Arc::new(AtomicBool::new(false));let dropped=Arc::new(AtomicU64::new(0));
        let a_addr=CustomAddr::from_parts(0x72647374657374,b"a");let b_addr=CustomAddr::from_parts(0x72647374657374,b"b");
        let a=endpoint(Link {addr:a_addr,fabric:fabric.clone(),drop_packets:Arc::new(AtomicBool::new(false)),dropped:Arc::new(AtomicU64::new(0))},armed.clone()).await;
        let b=endpoint(Link {addr:b_addr.clone(),fabric,drop_packets:dropping.clone(),dropped:dropped.clone()},armed.clone()).await;
        let target=loopback_target(&b,b_addr);
        let (client,server)=tokio::join!(a.connect(target,rds_core::ALPN),async {b.accept().await.unwrap().await});
        let (client,server)=(client.unwrap(),server.unwrap());
        let (mut request,mut reply)=client.open_bi().await.unwrap();request.write_all(b"warm").await.unwrap();
        let (mut send,mut recv)=server.accept_bi().await.unwrap();let mut warm=[0;4];recv.read_exact(&mut warm).await.unwrap();
        send.write_all(b"ok").await.unwrap();let mut ack=[0;2];reply.read_exact(&mut ack).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5),async {
            loop {
                let paths=server.paths();
                if paths.iter().any(|p|p.is_ip()) && paths.iter().any(|p|p.is_selected() && matches!(p.remote_addr(),TransportAddr::Custom(_))) { break }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("fixture did not establish its UDP standby while the custom link was preferred");
        let selected=server.paths().iter().find(|path|path.is_selected()).unwrap().id();
        let before=server.paths().get(selected).unwrap().stats().frame_tx.stream;
        dropping.store(true,Ordering::Release);let started=Instant::now();send.write_all(b"pending reliable bytes").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1),async {
            while dropped.load(Ordering::Relaxed)==0 || server.paths().get(selected).unwrap().stats().frame_tx.stream<=before {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("fixture did not emit and drop the already-pending stream frame");
        armed.store(true,Ordering::Release);
        // Retirement waits for `max(8 RTT, 2s)` without ACK progress, so recovery follows it.
        let mut body=[0;22];let result=tokio::time::timeout(Duration::from_secs(5),reply.read_exact(&mut body)).await;
        let elapsed=started.elapsed().as_millis();let injected=dropped.load(Ordering::Relaxed);
        tokio::join!(a.close(),b.close());
        assert!(injected>0,"blackhole fixture did not drop a packet");
        result.expect("Iroh left the existing reliable stream stalled after its preferred path blackholed").unwrap();
        assert_eq!(&body,b"pending reliable bytes");assert!(elapsed<5000,"{elapsed} ms");
    }).await.expect("isolated Iroh blackhole fixture did not terminate");
}
