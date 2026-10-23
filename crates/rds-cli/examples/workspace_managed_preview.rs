//! Real local-agent/QUIC/H.264 workspace qualification with synthetic displays.
//! No operating-system capture or input injection; all endpoints are loopback.
use clap::Parser;
use rds_cli::desktop::{
    Options, Resolution,
    workspace::{DeviceConfig, WorkspaceConfig},
};
use rds_client::local::{Client, Prepared, Server};
use rds_core::{
    AgentInfo, Codec, DesktopCaps, DisplayInfo, FrameHeader, HelloAck, InputEvent, InputKind,
    ServiceKind, StreamHello, UniHello,
};
use rds_desktop::render::workspace::{TabProfile, TabSpec};
use rds_desktop::{
    DesktopError, Encoder, FrameProducer, H264Encoder, InputSink, Produced, ProducerControls,
    RawFrame, SessionClock, SessionConfig,
};
use rds_net::{Backend, Endpoint, EndpointConfig, read_frame, write_frame};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    report: PathBuf,
    #[arg(long, default_value_t = 180)]
    duration: u64,
    #[arg(long)]
    noq: bool,
}
#[derive(Default, serde::Serialize)]
struct Counts {
    opened: u64,
    closed: u64,
    frames: u64,
    input_events: u64,
    held_keys: BTreeSet<u32>,
    fps: Vec<u32>,
}
type Stats = Arc<Mutex<Vec<Counts>>>;

struct Sink {
    index: usize,
    display: u32,
    stats: Stats,
}
impl InputSink for Sink {
    fn inject(&mut self, event: &InputEvent) -> Result<(), DesktopError> {
        assert_eq!(
            event.display_id, self.display,
            "input crossed display scope"
        );
        let mut stats = self.stats.lock().unwrap();
        let stats = &mut stats[self.index];
        stats.input_events += 1;
        match event.kind {
            InputKind::KeyDown { code } => {
                stats.held_keys.insert(code);
            }
            InputKind::KeyUp { code } => {
                stats.held_keys.remove(&code);
            }
            _ => {}
        }
        Ok(())
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        let mut stats = self.stats.lock().unwrap();
        stats[self.index].closed += 1;
        // Model the input backend's session-scoped release on disconnect. There is
        // deliberately no access to a real keyboard, pointer or display server.
        stats[self.index].held_keys.clear();
    }
}

struct Picture {
    encoder: H264Encoder,
    frame: RawFrame,
    interval: Duration,
    next: Instant,
    index: usize,
    stats: Stats,
}
impl Picture {
    fn new(index: usize, fps: u32, stats: Stats) -> Self {
        let color = match index {
            0 => [140, 65, 35],
            1 => [75, 45, 140],
            _ => [65, 120, 35],
        };
        let mut data = vec![0u8; 640 * 360 * 4];
        for (n, pixel) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let shift = ((n / 640 / 40 + n % 640 / 40) % 2) as u8 * 16;
            pixel.copy_from_slice(&[color[0] + shift, color[1] + shift, color[2] + shift, 255]);
        }
        Self {
            encoder: H264Encoder::new(2_000_000, fps as f32).unwrap(),
            frame: RawFrame {
                width: 640,
                height: 360,
                stride: 2560,
                data: data.into(),
            },
            interval: Duration::from_secs_f64(1. / f64::from(fps)),
            next: Instant::now(),
            index,
            stats,
        }
    }
}
impl FrameProducer for Picture {
    fn preserves_reference(&self) -> bool {
        true
    }
    fn produce(
        &mut self,
        seq: u64,
        controls: &ProducerControls,
        clock: &SessionClock,
    ) -> Option<Produced> {
        // FrameProducer is called on the owned blocking capture pool.
        if let Some(wait) = self.next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        self.next = Instant::now() + self.interval;
        if controls.idr.swap(false, Ordering::Relaxed) {
            self.encoder.request_idr();
        }
        self.encoder.set_bitrate(
            controls
                .bitrate
                .load(Ordering::Relaxed)
                .min(u64::from(u32::MAX)) as u32,
        );
        let capture_ts_ms = clock.now_ms();
        let encoded = self.encoder.encode(&self.frame).ok()?;
        if encoded.data.is_empty() {
            return None;
        }
        self.stats.lock().unwrap()[self.index].frames += 1;
        Some(Produced {
            header: FrameHeader {
                seq,
                capture_ts_ms,
                encode_done_ts_ms: clock.now_ms(),
                send_ts_ms: 0,
                keyframe: encoded.keyframe,
                codec: Codec::H264,
                width: 640,
                height: 360,
            },
            payload: encoded.data,
        })
    }
}
fn caps(computer: usize) -> DesktopCaps {
    DesktopCaps {
        displays: (0..if computer == 0 { 2 } else { 1 })
            .map(|index| DisplayInfo {
                index,
                width: 640,
                height: 360,
                primary: index == 0,
            })
            .collect(),
        codecs: vec![Codec::H264],
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
fn serve(endpoint: Endpoint, computer: usize, stats: Stats) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        while let Some(incoming) = endpoint.accept().await {
            let conn = incoming.await.unwrap();
            let stats = stats.clone();
            tasks.spawn(async move {
                let mut streams = tokio::task::JoinSet::new();
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let conn = conn.clone();
                    let stats = stats.clone();
                    streams.spawn(async move {
                        match read_frame::<_, StreamHello>(&mut recv).await.unwrap() {
                            StreamHello::Ping { nonce } => {
                                write_frame(&mut send, &HelloAck::Ok).await.unwrap();
                                send.write_all(&nonce.to_be_bytes()).await.unwrap();
                                send.finish().unwrap();
                            }
                            StreamHello::Info => {
                                write_frame(
                                    &mut send,
                                    &HelloAck::Info(AgentInfo {
                                        protocol: rds_core::PROTOCOL_VERSION,
                                        version: "synthetic-qualification".into(),
                                        hostname: None,
                                        services: vec![
                                            ServiceKind::Ping,
                                            ServiceKind::Info,
                                            ServiceKind::Desktop,
                                        ],
                                        desktop: Some(caps(computer)),
                                    }),
                                )
                                .await
                                .unwrap();
                                send.finish().unwrap();
                            }
                            greeting @ (StreamHello::DesktopV2 { .. }
                            | StreamHello::DesktopV3 { .. }
                            | StreamHello::DesktopV4 { .. }
                            | StreamHello::DesktopV5 { .. }) => {
                                let (session, hello, payload_receipts, clipboard, ack) =
                                    match greeting {
                                        StreamHello::DesktopV5 {
                                            session,
                                            hello,
                                            payload_receipts,
                                            clipboard,
                                            ..
                                        } => (
                                            session,
                                            hello,
                                            payload_receipts,
                                            clipboard,
                                            HelloAck::DesktopV5(caps(computer)),
                                        ),
                                        StreamHello::DesktopV4 { session, hello, .. } => (
                                            session,
                                            hello,
                                            true,
                                            false,
                                            HelloAck::DesktopV4(caps(computer)),
                                        ),
                                        StreamHello::DesktopV2 { session, hello }
                                        | StreamHello::DesktopV3 { session, hello, .. } => (
                                            session,
                                            hello,
                                            false,
                                            false,
                                            HelloAck::Desktop(caps(computer)),
                                        ),
                                        _ => unreachable!("desktop greeting matched above"),
                                    };
                                assert!(
                                    caps(computer)
                                        .displays
                                        .iter()
                                        .any(|d| d.index == hello.display)
                                );
                                let index = computer * 2 + hello.display as usize;
                                {
                                    let mut stats = stats.lock().unwrap();
                                    stats[index].opened += 1;
                                    stats[index].fps.push(hello.max_fps);
                                }
                                write_frame(&mut send, &ack).await.unwrap();
                                let config = SessionConfig {
                                    frame_route: Some(UniHello::DesktopFrames { id: session }),
                                    payload_receipts,
                                    reverse_clipboard: clipboard,
                                    input_sink: Some(Box::new(Sink {
                                        index,
                                        display: hello.display,
                                        stats: stats.clone(),
                                    })),
                                    producer: Some(Box::new(Picture::new(
                                        index,
                                        hello.max_fps,
                                        stats,
                                    ))),
                                    ..Default::default()
                                };
                                let _ = rds_desktop::serve_desktop_with(
                                    conn, send, recv, hello, config,
                                )
                                .await;
                            }
                            _ => panic!("unexpected fixture service"),
                        }
                    });
                }
                streams.shutdown().await;
            });
        }
        tasks.shutdown().await;
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let args = Args::parse();
    anyhow::ensure!(
        (10..=600).contains(&args.duration),
        "duration must be 10..=600 seconds"
    );
    let backend = if args.noq {
        Backend::Noq
    } else {
        Backend::Iroh
    };
    let local = endpoint(backend).await;
    let a = endpoint(backend).await;
    let b = endpoint(backend).await;
    let stats: Stats = Arc::new(Mutex::new((0..3).map(|_| Counts::default()).collect()));
    let tasks = [
        serve(a.clone(), 0, stats.clone()),
        serve(b.clone(), 1, stats.clone()),
    ];
    // The macOS per-user temporary path can already consume most of sun_path.
    // Canonical /tmp keeps this private fixture's IPC names below that bound.
    let root = std::path::Path::new("/tmp").canonicalize()?.join(format!(
        "rds-workspace-managed-{:032x}",
        rand::random::<u128>()
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let mut manager = Server::start(Some(Prepared::bind(&root).await?), local.clone(), None);
    let client = Client::new(&root);
    let devices = [&a, &b]
        .into_iter()
        .enumerate()
        .map(|(index, endpoint)| DeviceConfig {
            key: format!("computer-{index}"),
            target: rds_net::Ticket::of(endpoint).to_string(),
            label: if index == 0 { "Studio PC" } else { "Build PC" }.into(),
            grant_file: None,
            local_addresses: vec![],
        })
        .collect::<Vec<_>>();
    let tabs = devices
        .iter()
        .enumerate()
        .flat_map(|(computer, device)| {
            caps(computer).displays.into_iter().map(|display| TabSpec {
                device: device.key.clone(),
                label: device.label.clone(),
                display: display.index,
                profile: TabProfile {
                    max_fps: 10,
                    payload_receipts: true,
                    clipboard: false,
                    ..Default::default()
                },
            })
        })
        .collect();
    let config = WorkspaceConfig {
        schema_version: 1,
        presentation: Default::default(),
        devices,
        tabs,
    };
    let options = Options {
        remote_cursor_embedded: false,
        payload_receipts: true,
        clipboard: false,
        resolution: Resolution::FullHd,
        display: 0,
        max_fps: 10.try_into()?,
        headless: false,
        duration: Some(args.duration.try_into()?),
        report: None,
        diagnostics_dir: Some(root.clone()),
        diagnostic_visual_probe: None,
        diagnostic_health: None,
    };
    let result = rds_cli::desktop::workspace::run(
        client.clone(),
        config,
        root.join("workspace.json"),
        options,
        false,
    )
    .await;
    let identity_retained = client.snapshot().await?.endpoint == local.id().to_string();
    manager.close().await?;
    local.close().await;
    a.close().await;
    b.close().await;
    for task in tasks {
        tokio::time::timeout(Duration::from_secs(3), task).await??;
    }
    let report = serde_json::json!({"backend":format!("{backend:?}"),"identity_retained":identity_retained,
        "result":result.as_ref().err().map(ToString::to_string),"displays":&*stats.lock().unwrap()});
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&args.report)?;
    serde_json::to_writer_pretty(file, &report)?;
    std::fs::remove_dir_all(&root)?;
    result?;
    anyhow::ensure!(
        identity_retained,
        "workspace replaced the local endpoint identity"
    );
    anyhow::ensure!(
        stats.lock().unwrap().iter().all(|s| s.opened > 0
            && s.frames > 0
            && s.opened == s.closed
            && s.held_keys.is_empty()),
        "not every synthetic display opened, produced and closed cleanly"
    );
    Ok(())
}
