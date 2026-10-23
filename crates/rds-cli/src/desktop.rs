//! Native viewer orchestration. The OS event loop owns the main thread; one
//! cancelable async worker owns the remote desktop and bounded decode work.
use clap::Args;
use rds_client::local::Client;
use rds_core::local::SessionId;
#[cfg(feature = "desktop")]
use rds_core::local::{Command, Reply};
#[cfg(feature = "desktop")]
use rds_core::{Codec, DesktopHello};
#[cfg(feature = "desktop")]
use std::time::Duration;
use std::{
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
};

#[derive(clap::ValueEnum, Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Resolution {
    Hd,
    #[default]
    FullHd,
    Native,
}
impl Resolution {
    pub fn height(self) -> u32 {
        match self {
            Self::Hd => 720,
            Self::FullHd => 1080,
            Self::Native => 0,
        }
    }
}

#[derive(Args, Clone)]
pub struct Options {
    /// Use validated frame receipts; requires updated local and remote agents.
    #[arg(long, action=clap::ArgAction::Set, num_args=0..=1, require_equals=true, default_missing_value="true", default_value="false")]
    pub payload_receipts: bool,
    /// Share UTF-8 text clipboard with the focused remote window (V5).
    #[arg(long, action=clap::ArgAction::Set, num_args=0..=1, require_equals=true, default_missing_value="true", default_value="true")]
    pub clipboard: bool,
    /// Video quality profile; preserve source aspect ratio without upscaling.
    #[arg(long, value_enum, default_value = "full-hd")]
    pub resolution: Resolution,
    #[arg(long, default_value = "0")]
    pub display: u32,
    #[arg(long, default_value = "30")]
    pub max_fps: NonZeroU32,
    /// Hide the local pointer over video when the server embeds its cursor.
    #[arg(long)]
    pub remote_cursor_embedded: bool,
    /// Decode/statistics without opening a native window.
    #[arg(long, conflicts_with = "report")]
    pub headless: bool,
    /// End after this many seconds; useful for bounded qualification runs.
    #[arg(long)]
    pub duration: Option<NonZeroU64>,
    /// Write native presentation diagnostics to a new JSON file.
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Private directory for live diagnostic snapshots.
    #[arg(long)]
    pub diagnostics_dir: Option<PathBuf>,
    /// Controlled-marker descriptor for causal input-to-submit diagnostics.
    #[arg(long, conflicts_with = "headless")]
    pub diagnostic_visual_probe: Option<PathBuf>,
    /// Internal, non-owning telemetry observation for the native flight recorder.
    #[arg(skip)]
    pub diagnostic_health: Option<rds_observe::HealthObserver>,
}

/// Shared bounded credential loading for CLI and native application clients.
pub async fn read_grant(
    path: Option<PathBuf>,
) -> anyhow::Result<Option<Box<rds_core::grant::Grant>>> {
    match path {
        Some(path) => Ok(Some(Box::new(
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                use rustix::fs::{Mode, OFlags};
                use std::io::Read;
                let file = std::fs::File::from(rustix::fs::open(
                    &path,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                anyhow::ensure!(file.metadata()?.is_file(), "grant must be a regular file");
                let mut bytes = Vec::new();
                file.take(65537).read_to_end(&mut bytes)?;
                anyhow::ensure!(bytes.len() <= 65536, "grant file exceeds 64 KiB");
                Ok(serde_json::from_slice(&bytes)?)
            })
            .await??,
        ))),
        None => Ok(None),
    }
}

#[cfg(feature = "desktop")]
mod control;

#[cfg(feature = "desktop")]
mod liveness;

#[cfg(feature = "desktop")]
mod diagnostics;

#[cfg(feature = "desktop")]
mod diagnostic_storage;

#[cfg(feature = "desktop")]
pub mod workspace;

#[cfg(feature = "desktop")]
mod native {
    use super::*;
    use rds_client::local::ManagedMessage;
    use rds_desktop::{
        client::{DesktopSession, RelayDecoder, RelayOutcome, SessionOpts},
        render::{InputReceiver, Viewer, ViewerHandle, ViewerInput},
    };
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Instant,
    };

    struct Attempt {
        started: Instant,
        healthy: Arc<AtomicBool>,
    }

    fn retry_delay(failures: &mut u32, frames_advanced: bool, healthy: bool) -> Duration {
        if frames_advanced && healthy {
            *failures = 0;
        }
        *failures = failures.saturating_add(1);
        Duration::from_millis((250u64 << (*failures).min(5)).min(8000))
    }
    use tokio_util::sync::CancellationToken;
    use tracing::Instrument;

    pub(super) enum Source {
        Managed {
            client: Client,
            session: Option<SessionId>,
            peer: String,
            grant: Option<Box<rds_core::grant::Grant>>,
            binding: PeerBinding,
            local_addresses: Vec<std::net::SocketAddr>,
        },
        Direct {
            endpoint: rds_net::Endpoint,
            target: rds_net::EndpointAddr,
            grant: Option<rds_core::grant::Grant>,
        },
    }

    pub(super) fn connect_command(
        target: String,
        grant: Option<Box<rds_core::grant::Grant>>,
        local_addresses: Vec<std::net::SocketAddr>,
    ) -> Command {
        if local_addresses.is_empty() {
            Command::Connect { target, grant }
        } else {
            Command::ConnectPreferred {
                target,
                grant,
                local_addresses,
            }
        }
    }

    /// Display settings must not resolve a mutable device name to a different
    /// endpoint. All tabs for one saved computer share its authenticated binding.
    #[derive(Clone, Default)]
    pub(super) struct PeerBinding(Arc<std::sync::Mutex<Option<String>>>);
    impl PeerBinding {
        pub(super) fn target(&self, original: &str) -> String {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or_else(|| original.to_owned())
        }
        fn authenticated(&self, original: &str, peer: String) -> anyhow::Result<String> {
            let mut binding = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let target = reconnect_target(binding.as_deref().unwrap_or(original), peer)?;
            *binding = Some(target.clone());
            Ok(target)
        }
    }
    fn hello(options: &Options) -> DesktopHello {
        DesktopHello {
            display: options.display,
            max_fps: options.max_fps.get().min(240),
            codec: Codec::H264,
            input_acks: true,
        }
    }
    fn extent(
        handle: &ViewerHandle,
        caps: &rds_core::DesktopCaps,
        display: u32,
    ) -> anyhow::Result<()> {
        let screen = caps
            .displays
            .iter()
            .find(|d| d.index == display)
            .ok_or_else(|| anyhow::anyhow!("remote display {display} unavailable"))?;
        handle.display_extent(screen.width, screen.height);
        Ok(())
    }
    pub(super) async fn run(source: Source, options: Options) -> anyhow::Result<()> {
        if options.headless {
            let duration = options.duration;
            return tokio::select! {
                result = headless(source, options) => result,
                _ = async { match duration { Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds.get())).await, None => std::future::pending().await } } => Ok(()),
            };
        }
        let (viewer, handle, mut input) = Viewer::new(options.display)?;
        let device = match &source {
            Source::Managed { peer, .. } => rds_net::parse_target(peer)
                .map(|address| format!("Device {}", &address.id.to_string()[..8]))
                .unwrap_or_else(|_| peer.chars().take(80).collect()),
            Source::Direct { target, .. } => format!("Device {}", &target.id.to_string()[..8]),
        };
        handle.label(format!("RDS · {device} · Display {}", options.display));
        if let Some(path) = &options.diagnostic_visual_probe {
            use std::io::Read;
            let file = std::fs::File::from(rustix::fs::open(
                path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )?);
            anyhow::ensure!(
                file.metadata()?.is_file(),
                "visual probe descriptor must be a regular file"
            );
            let mut bytes = Vec::new();
            file.take(4097).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 4096, "visual probe descriptor exceeds 4 KiB");
            let spec: rds_desktop::render::VisualProbeSpec = serde_json::from_slice(&bytes)?;
            handle.visual_probe(spec)?;
        }
        let stop = CancellationToken::new();
        let mut workers = tokio::task::JoinSet::new();
        let worker_stop = stop.clone();
        let worker_view = handle.clone();
        let worker_options = options.clone();
        let diagnostic_stop = stop.clone();
        let diagnostic_view = handle.clone();
        let diagnostic_dir = options.diagnostics_dir.clone();
        let diagnostic_health = options.diagnostic_health.clone();
        workers.spawn(observe(
            diagnostic_view,
            diagnostic_dir,
            diagnostic_health,
            diagnostic_stop,
            None,
        ));
        workers.spawn(async move {
            let result = network(
                source,
                &worker_options,
                &worker_view,
                &mut input,
                worker_stop,
            )
            .await;
            if let Err(error) = &result {
                worker_view.status(format!("Disconnected: {error}"));
            }
            result
        });
        if let Some(seconds) = options.duration {
            let view = handle.clone();
            let canceled = stop.clone();
            workers.spawn(async move {
                tokio::select! {
                    _ = canceled.cancelled() => {},
                    _ = tokio::time::sleep(Duration::from_secs(seconds.get())) => view.close(),
                }
                Ok(())
            });
        }
        let result = tokio::task::block_in_place(|| viewer.run());
        stop.cancel();
        if tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(joined) = workers.join_next().await {
                if let Ok(Err(error)) = joined {
                    tracing::warn!(%error,"desktop worker ended");
                }
            }
        })
        .await
        .is_err()
        {
            workers.shutdown().await;
        }
        let report = handle.report();
        if let Some(path) = options.report {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            serde_json::to_writer_pretty(&mut file, &report)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        result?;
        anyhow::ensure!(
            report.frames_submitted > 0,
            "desktop ended before a frame was presented"
        );
        Ok(())
    }

    pub(super) async fn observe(
        diagnostic_view: ViewerHandle,
        diagnostic_dir: Option<PathBuf>,
        diagnostic_health: Option<rds_observe::HealthObserver>,
        diagnostic_stop: CancellationToken,
        scope: Option<(u64, u64)>,
    ) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut recorder = diagnostics::Recorder::default();
        let mut storage = match scope {
            Some((tab, revision)) => {
                diagnostic_storage::Storage::scoped(diagnostic_dir, tab, revision)
            }
            None => diagnostic_storage::Storage::new(diagnostic_dir),
        };
        loop {
            tokio::select! {
                _ = diagnostic_stop.cancelled() => break,
                _ = tick.tick() => {
                    diagnostic_view.heartbeat_ui();
                    let snapshot = diagnostic_view.snapshot();
                    let health = diagnostic_health.as_ref().and_then(rds_observe::HealthObserver::snapshot);
                    let (value, incident) = recorder.observe_with_health(&snapshot, health, &storage.health());
                    let json = serde_json::to_string(&value).ok();
                    if let Some(json) = &json {
                        tracing::info!(snapshot=%json, "viewer health");
                    }
                    storage.persist(json.map(String::into_bytes), incident, snapshot.elapsed_ms).await;
                },
            }
        }
        storage
            .persist(
                None,
                recorder.finish(true),
                diagnostic_view.snapshot().elapsed_ms,
            )
            .await;
        let remaining = storage.health().incident_queue_pending;
        if remaining > 0 {
            tracing::warn!(
                remaining,
                "viewer shutdown retains unsaved incident windows in memory only"
            );
        }
        if scope.is_some() {
            storage.remove_snapshot().await;
        }
        Ok(())
    }

    pub(super) async fn network(
        mut source: Source,
        options: &Options,
        view: &ViewerHandle,
        input: &mut InputReceiver,
        stop: CancellationToken,
    ) -> anyhow::Result<()> {
        view.remote_cursor_embedded(options.remote_cursor_embedded);
        let started = Instant::now();
        let mut failures = 0u32;
        loop {
            let span = rds_observe::conn_span(rds_observe::next_session_id());
            view.begin_session(span.clone());
            let received = view.report().frames_received;
            let attempt = Attempt {
                started,
                healthy: Arc::new(AtomicBool::new(false)),
            };
            let result = tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                result = async {
                    match &mut source {
                        Source::Managed { client,session,peer,grant,binding,local_addresses } => {
                            *peer = binding.target(peer);
                            view.stage("checking managed session");
                            if session.is_none() || client.selected(*session).await.is_err() {
                                view.stage("connecting peer");
                                let Reply::Connected(id) = client.request(connect_command(peer.clone(),grant.clone(),local_addresses.clone())).await? else { anyhow::bail!("unexpected local connection response"); };
                                // After the first authenticated connect retain the exact peer
                                // identity, even when the initial target was a registry name.
                                view.stage("verifying peer");
                                let snapshot = client.snapshot().await?;
                                let authenticated = snapshot.sessions.into_iter().find(|s| s.id == id).ok_or_else(|| anyhow::anyhow!("connected session disappeared"))?.peer;
                                *peer = binding.authenticated(peer,authenticated)?;
                                *session = Some(id);
                            }
                            managed_session(client,session.ok_or_else(|| anyhow::anyhow!("no managed session"))?,options,view,input,&stop,&attempt).await
                        },
                        Source::Direct { endpoint,target,grant } => direct_session(endpoint,(target.clone(),grant.as_ref()),options,view,input,&stop,&attempt).await,
                    }
                }.instrument(span) => result,
            };
            if stop.is_cancelled() || matches!(result, Ok(true)) {
                return Ok(());
            }
            let wait = retry_delay(
                &mut failures,
                view.report().frames_received > received,
                attempt.healthy.load(Ordering::Relaxed),
            );
            let snapshot = view.snapshot();
            tracing::warn!(
                error = ?result.as_ref().err(), attempt = failures,
                network_stage = %snapshot.network_stage,
                render_stage = %snapshot.render_stage,
                decoded_frame_age_ms = snapshot.decoded_frame_age_ms,
                submission_age_ms = snapshot.submission_age_ms,
                ui_event_age_ms = snapshot.ui_event_age_ms,
                pending_input_acks = snapshot.report.pending_input_acks,
                oldest_input_ack_age_ms = snapshot.report.oldest_input_ack_age_ms,
                control_rtt_ms = snapshot.report.control_rtt_ms,
                control_echo_age_ms = snapshot.control_echo_age_ms,
                occluded = snapshot.occluded,
                "desktop reconnecting"
            );
            view.status("Reconnecting");
            view.stage("reconnecting");
            if !retry_pause(wait, &stop, input).await {
                return Ok(());
            }
            // Never replay input captured while disconnected onto a replacement.
            while let Some(message) = input.try_recv() {
                if matches!(message, ViewerInput::Close) {
                    return Ok(());
                }
            }
        }
    }

    #[test]
    fn repeated_unhealthy_attempts_back_off_even_when_video_arrives() {
        let mut failures = 0;
        for ms in [500, 1000, 2000, 4000, 8000, 8000] {
            assert_eq!(
                retry_delay(&mut failures, true, false),
                Duration::from_millis(ms)
            );
        }
        assert_eq!(
            retry_delay(&mut failures, false, true),
            Duration::from_secs(8)
        );
        assert_eq!(
            retry_delay(&mut failures, true, true),
            Duration::from_millis(500)
        );
    }

    pub(super) fn reconnect_target(
        original: &str,
        authenticated: String,
    ) -> anyhow::Result<String> {
        if let Ok(pinned) = rds_net::parse_target(original) {
            anyhow::ensure!(
                pinned.id.to_string() == authenticated,
                "connected session differs from pinned endpoint"
            );
            // Keep explicit address hints through agent/directory outages.
            Ok(original.to_owned())
        } else {
            // A verified name freezes to its authenticated identity.
            Ok(authenticated)
        }
    }

    #[test]
    fn tabs_share_authenticated_identity_across_settings_and_reconnects() {
        let binding = PeerBinding::default();
        let first = rds_net::SecretKey::generate().public().to_string();
        let other = rds_net::SecretKey::generate().public().to_string();
        let sibling = binding.clone();
        assert_eq!(binding.target("mutable-name"), "mutable-name");
        assert_eq!(
            binding
                .authenticated("mutable-name", first.clone())
                .unwrap(),
            first
        );
        assert_eq!(sibling.target("mutable-name"), first);
        assert!(sibling.authenticated("mutable-name", other).is_err());
        assert_eq!(binding.target("mutable-name"), first);
        assert_eq!(
            sibling
                .authenticated("mutable-name", first.clone())
                .unwrap(),
            first
        );
    }

    trait RetryInput: Send {
        fn close_requested(&mut self) -> impl Future<Output = bool> + Send;
    }
    impl RetryInput for InputReceiver {
        async fn close_requested(&mut self) -> bool {
            matches!(self.recv().await, Some(ViewerInput::Close) | None)
        }
    }

    async fn retry_pause(
        wait: Duration,
        stop: &CancellationToken,
        input: &mut impl RetryInput,
    ) -> bool {
        // Discarded input must neither complete nor restart the retry timer.
        let deadline = tokio::time::sleep(wait);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => return false,
                _ = &mut deadline => return true,
                close = input.close_requested() => if close { return false; },
            }
        }
    }

    async fn managed_session(
        client: &Client,
        session: SessionId,
        options: &Options,
        view: &ViewerHandle,
        input: &mut InputReceiver,
        stop: &CancellationToken,
        attempt: &Attempt,
    ) -> anyhow::Result<bool> {
        let started = attempt.started;
        view.stage("opening desktop");
        let (mut channel, mut events) = client
            .desktop_features(
                Some(session),
                hello(options),
                options.resolution.height(),
                options.payload_receipts,
                options.clipboard,
            )
            .await?;
        tracing::info!(
            payload_receipts = options.payload_receipts,
            "native desktop delivery receipt mode"
        );
        view.managed_events_separated();
        extent(view, &channel.caps, options.display)?;
        view.status("Waiting for screen");
        let control = channel.control_handle();
        let (progress, last_frame) = tokio::sync::watch::channel(tokio::time::Instant::now());
        let control_progress = liveness::ControlWatchdog::with_health(attempt.healthy.clone());
        // Keep the entire receive/decode future alive while controls progress.
        // Selecting individual recv calls and awaiting decode in their handler
        // prevents input, heartbeat and close from being polled during decode.
        let media = async {
            let mut decoder = RelayDecoder::new();
            loop {
                match channel.recv().await? {
                    None => break Ok(false),
                    Some(ManagedMessage::Event(_)) => {
                        anyhow::bail!("unexpected event on separated video channel")
                    }
                    Some(ManagedMessage::Frame(frame)) => {
                        view.stage("decoding");
                        let received = Instant::now();
                        let frame_seq = frame.header.seq;
                        view.media_timing(&frame.header);
                        let (next, outcome) = tokio::time::timeout(
                            Duration::from_secs(5),
                            decoder.push_bounded(frame.header, frame.payload),
                        )
                        .await
                        .map_err(|_| anyhow::anyhow!("desktop decode stalled"))??;
                        decoder = next;
                        match outcome {
                            RelayOutcome::Frame(raw) => {
                                progress.send_replace(tokio::time::Instant::now());
                                view.frame_with_seq(raw, received, frame_seq);
                            }
                            RelayOutcome::NeedIdr => {
                                tokio::time::timeout(Duration::from_secs(2), control.request_idr())
                                    .await
                                    .map_err(|_| {
                                        anyhow::anyhow!("desktop repair write stalled")
                                    })??
                            }
                            RelayOutcome::Pending => {}
                        }
                        view.stage("receiving");
                    }
                }
            }
        };
        let event_observation = async {
            loop {
                match events.recv().await.transpose()? {
                    Some(rds_core::DesktopEvent::Heartbeat { seq, ts_ms }) => {
                        if control_progress.echoed(seq, ts_ms) {
                            view.control_rtt(
                                (started.elapsed().as_millis() as u64).saturating_sub(ts_ms),
                            );
                        }
                    }
                    Some(rds_core::DesktopEvent::InputAck { seq, .. }) => view.input_ack(seq),
                    Some(rds_core::DesktopEvent::ClipboardReady { id, bytes }) => {
                        control_progress.clipboard_ready(id, bytes);
                        view.clipboard_ack(id, bytes);
                    }
                    Some(
                        event @ (rds_core::DesktopEvent::ClipboardOffer { .. }
                        | rds_core::DesktopEvent::ClipboardChunk { .. }
                        | rds_core::DesktopEvent::ClipboardError { .. }),
                    ) => view.clipboard_event(event),
                    None => {
                        tracing::warn!("managed desktop event channel ended");
                        return Ok(false);
                    }
                }
            }
        };
        let incoming = async {
            tokio::select! {
                result = event_observation => result,
                result = media => result,
            }
        };
        let controls = control::pump_with_liveness(
            input,
            &control,
            &last_frame,
            &control_progress,
            started,
            |message| {
                view.input_sent(message);
            },
        );
        let result = control::run(controls, incoming, stop).await;
        // A winning leg may cancel a partially written control on the other
        // leg. EOF closes the manager's desktop; never append Finished to a
        // potentially incomplete frame. Unrelated manager sessions survive.
        drop(channel);
        result
    }

    async fn direct_session(
        endpoint: &rds_net::Endpoint,
        target: (rds_net::EndpointAddr, Option<&rds_core::grant::Grant>),
        options: &Options,
        view: &ViewerHandle,
        input: &mut InputReceiver,
        stop: &CancellationToken,
        attempt: &Attempt,
    ) -> anyhow::Result<bool> {
        let started = attempt.started;
        let (target, grant) = target;
        let conn = match grant {
            Some(g) => rds_client::connect_authorized(endpoint, target, g).await?,
            None => rds_client::connect(endpoint, target).await?,
        };
        struct Close(rds_net::Connection);
        impl Drop for Close {
            fn drop(&mut self) {
                self.0.close(0u32.into(), b"viewer session ended");
            }
        }
        let _close = Close(conn.clone());
        let mut session = DesktopSession::connect_opts(
            &conn,
            hello(options),
            SessionOpts {
                session: Some(rand_id()),
                output_height: Some(options.resolution.height()),
                payload_receipts: options.payload_receipts,
                reverse_clipboard: options.clipboard && !options.headless,
                ..Default::default()
            },
        )
        .await?;
        extent(view, session.caps(), options.display)?;
        view.status("Waiting for screen");
        let ctrl = session.control_sender();
        let mut last_frame = tokio::time::Instant::now();
        let mut watchdog = control::VideoWatchdog::default();
        let control_progress = liveness::ControlWatchdog::with_health(attempt.healthy.clone());
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let result = loop {
            tokio::select! {
                _ = stop.cancelled() => break Ok(true),
                message = input.recv() => match message {
                    Some(ViewerInput::Control(message)) => {
                        view.input_sent(&message);
                        control_progress.sent(&message);
                        tokio::time::timeout(Duration::from_secs(2),ctrl.send(message)).await.map_err(|_|anyhow::anyhow!("direct desktop control stalled"))??;
                    },
                    Some(ViewerInput::Close)|None => break Ok(true),
                    Some(ViewerInput::InputStalled) => anyhow::bail!("pointer input acknowledgement stalled"),
                },
                _ = tick.tick() => {
                    control_progress.check()?;
                    match watchdog.observe(last_frame) {
                        control::VideoAction::Reconnect => anyhow::bail!("remote video stopped making progress"),
                        control::VideoAction::Repair => {
                            view.video_repair_requested();
                            tokio::time::timeout(Duration::from_secs(2), ctrl.send(rds_core::DesktopControl::RequestIdr)).await.map_err(|_|anyhow::anyhow!("direct desktop repair stalled"))??;
                        },
                        control::VideoAction::Healthy => {},
                    }
                    let heartbeat = control_progress.heartbeat(started.elapsed().as_millis() as u64)?;
                    control_progress.sent(&heartbeat);
                    tokio::time::timeout(Duration::from_secs(2),ctrl.send(heartbeat)).await.map_err(|_|anyhow::anyhow!("direct desktop heartbeat stalled"))??;
                },
                event = session.events.recv() => match event {
                    Some(rds_core::DesktopEvent::Heartbeat { seq, ts_ms }) => {
                        if control_progress.echoed(seq, ts_ms) { view.control_rtt((started.elapsed().as_millis() as u64).saturating_sub(ts_ms)); }
                    },
                    Some(rds_core::DesktopEvent::InputAck { seq,.. }) => view.input_ack(seq),
                    Some(rds_core::DesktopEvent::ClipboardReady { id, bytes }) => {control_progress.clipboard_ready(id, bytes);view.clipboard_ack(id, bytes);},
                    Some(event @ (rds_core::DesktopEvent::ClipboardOffer { .. } | rds_core::DesktopEvent::ClipboardChunk { .. } | rds_core::DesktopEvent::ClipboardError { .. })) => view.clipboard_event(event),
                    None => break Ok(false),
                },
                frame = session.frames.recv() => match frame {
                    Some(raw) => { last_frame = tokio::time::Instant::now(); view.frame(raw,Instant::now()); },
                    None => break Ok(false),
                },
            }
        };
        drop(session);
        conn.close(0u32.into(), b"viewer session ended");
        result
    }
    fn rand_id() -> [u8; 16] {
        rand::random()
    }

    async fn headless(source: Source, options: Options) -> anyhow::Result<()> {
        match source {
            Source::Direct {
                endpoint,
                target,
                grant,
            } => {
                let conn = match grant {
                    Some(g) => rds_client::connect_authorized(&endpoint, target, &g).await?,
                    None => rds_client::connect(&endpoint, target).await?,
                };
                let mut session = DesktopSession::connect_opts(
                    &conn,
                    hello(&options),
                    SessionOpts {
                        session: Some(rand::random()),
                        output_height: Some(options.resolution.height()),
                        payload_receipts: options.payload_receipts,
                        reverse_clipboard: options.clipboard && !options.headless,
                        ..Default::default()
                    },
                )
                .await?;
                println!("desktop caps: {:?}", session.caps());
                let mut count = 0u64;
                while let Some(frame) = session.frames.recv().await {
                    count += 1;
                    if count.is_multiple_of(30) {
                        println!("decoded {count} frames, {}x{}", frame.width, frame.height);
                    }
                }
            }
            Source::Managed {
                client,
                session,
                peer,
                grant,
                binding: _,
                local_addresses,
            } => {
                let session = match session {
                    Some(session) => session,
                    None => match client
                        .request(connect_command(peer, grant, local_addresses))
                        .await?
                    {
                        Reply::Connected(session) => session,
                        _ => anyhow::bail!("unexpected local connection response"),
                    },
                };
                let mut channel = if options.payload_receipts {
                    client
                        .desktop_profile_receipts(
                            Some(session),
                            hello(&options),
                            options.resolution.height(),
                        )
                        .await?
                } else {
                    client
                        .desktop_profile(
                            Some(session),
                            hello(&options),
                            Some(options.resolution.height()),
                        )
                        .await?
                };
                let mut decoder = RelayDecoder::new();
                let mut count = 0u64;
                loop {
                    match channel.recv().await? {
                        Some(ManagedMessage::Frame(frame)) => {
                            let (next, outcome) =
                                decoder.push_bounded(frame.header, frame.payload).await?;
                            decoder = next;
                            match outcome {
                                RelayOutcome::Frame(raw) => {
                                    count += 1;
                                    if count.is_multiple_of(30) {
                                        println!(
                                            "decoded {count} frames, {}x{}",
                                            raw.width, raw.height
                                        );
                                    }
                                }
                                RelayOutcome::NeedIdr => channel.request_idr().await?,
                                RelayOutcome::Pending => {}
                            }
                        }
                        Some(ManagedMessage::Event(_)) => {}
                        None => break,
                    }
                }
            }
        }
        Ok(())
    }
    #[cfg(test)]
    mod retry_tests {
        use super::*;

        #[test]
        fn reconnect_preserves_pinned_hints_and_freezes_resolved_names() {
            let id = rds_net::SecretKey::generate().public();
            let ticket = rds_net::Ticket(
                rds_net::EndpointAddr::new(id)
                    .with_relay_url("https://relay.example.com/".parse().unwrap()),
            )
            .to_string();
            assert_eq!(reconnect_target(&ticket, id.to_string()).unwrap(), ticket);
            assert_eq!(
                reconnect_target("verified-device", id.to_string()).unwrap(),
                id.to_string()
            );
            assert!(
                reconnect_target(&ticket, rds_net::SecretKey::generate().public().to_string())
                    .is_err()
            );
        }

        impl RetryInput for tokio::sync::mpsc::Receiver<bool> {
            async fn close_requested(&mut self) -> bool {
                self.recv().await.unwrap_or(true)
            }
        }

        #[tokio::test(start_paused = true)]
        async fn input_does_not_end_or_extend_retry_delay() {
            let stop = CancellationToken::new();
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let wait = Duration::from_secs(8);
            let started = tokio::time::Instant::now();
            let mut retry = Box::pin(retry_pause(wait, &stop, &mut rx));
            tx.send(false).await.unwrap();
            tokio::select! {
                result = &mut retry => panic!("input bypassed retry delay: {result}"),
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            }
            for _ in 0..6 {
                tx.send(false).await.unwrap();
                tokio::select! {
                    result = &mut retry => panic!("input bypassed retry delay: {result}"),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                }
            }
            assert!(retry.await);
            assert_eq!(started.elapsed(), wait, "input must not restart the timer");
            assert!(
                rx.try_recv().is_err(),
                "disconnected input was not discarded"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn close_and_cancel_interrupt_retry_immediately() {
            let stop = CancellationToken::new();
            let started = tokio::time::Instant::now();
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            tx.send(true).await.unwrap();
            assert!(!retry_pause(Duration::from_secs(8), &stop, &mut rx).await);
            stop.cancel();
            assert!(!retry_pause(Duration::from_secs(8), &stop, &mut rx).await);
            assert_eq!(started.elapsed(), Duration::ZERO);
        }
    }
}

#[cfg(feature = "desktop")]
pub async fn managed(
    client: &Client,
    session: SessionId,
    grant: Option<Box<rds_core::grant::Grant>>,
    options: Options,
) -> anyhow::Result<()> {
    let snapshot = client.snapshot().await?;
    let peer = snapshot
        .sessions
        .into_iter()
        .find(|s| s.id == session)
        .ok_or_else(|| anyhow::anyhow!("session no longer available"))?
        .peer;
    native::run(
        native::Source::Managed {
            client: client.clone(),
            session: Some(session),
            peer,
            grant,
            binding: Default::default(),
            local_addresses: vec![],
        },
        options,
    )
    .await
}

/// Open the application before attempting network/agent access, so startup
/// outages remain visible and bounded recovery can proceed in the window.
#[cfg(feature = "desktop")]
pub async fn managed_target(
    client: &Client,
    target: String,
    grant: Option<Box<rds_core::grant::Grant>>,
    options: Options,
) -> anyhow::Result<()> {
    native::run(
        native::Source::Managed {
            client: client.clone(),
            session: None,
            peer: target,
            grant,
            binding: Default::default(),
            local_addresses: vec![],
        },
        options,
    )
    .await
}
#[cfg(feature = "desktop")]
pub async fn direct(
    endpoint: &rds_net::Endpoint,
    target: rds_net::EndpointAddr,
    grant: Option<rds_core::grant::Grant>,
    options: Options,
) -> anyhow::Result<()> {
    native::run(
        native::Source::Direct {
            endpoint: endpoint.clone(),
            target,
            grant,
        },
        options,
    )
    .await
}
#[cfg(not(feature = "desktop"))]
pub async fn managed(
    _client: &Client,
    _session: SessionId,
    _grant: Option<Box<rds_core::grant::Grant>>,
    _options: Options,
) -> anyhow::Result<()> {
    anyhow::bail!("rds built without desktop support; enable the desktop feature")
}
#[cfg(not(feature = "desktop"))]
pub async fn direct(
    _endpoint: &rds_net::Endpoint,
    _target: rds_net::EndpointAddr,
    _grant: Option<rds_core::grant::Grant>,
    _options: Options,
) -> anyhow::Result<()> {
    anyhow::bail!("rds built without desktop support; enable the desktop feature")
}
