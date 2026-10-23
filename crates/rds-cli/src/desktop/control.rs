//! Managed native control dispatch progresses independently of decode waits.
use rds_core::DesktopControl;
use rds_desktop::render::{InputReceiver, ViewerInput};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(super) trait Input {
    fn recv(&mut self) -> impl Future<Output = Option<ViewerInput>> + Send;
}
impl Input for InputReceiver {
    async fn recv(&mut self) -> Option<ViewerInput> {
        self.recv().await
    }
}
pub(super) trait Sender {
    fn send(&self, message: DesktopControl) -> impl Future<Output = anyhow::Result<()>> + Send;
}
impl Sender for rds_client::local::ManagedControl {
    async fn send(&self, message: DesktopControl) -> anyhow::Result<()> {
        Ok(self.control(message).await?)
    }
}

const VIDEO_STALL_TIMEOUT: Duration = Duration::from_secs(15);
const REPAIR_AGES: [Duration; 2] = [Duration::from_secs(3), Duration::from_secs(8)];

#[derive(Debug, PartialEq, Eq)]
pub(super) enum VideoAction {
    Healthy,
    Repair,
    Reconnect,
}

/// Idle video is expected to refresh about once a second. Request at most
/// two independent images before reopening the desktop; heartbeats alone
/// never reset actual decoded progress. A new frame rearms this policy.
#[derive(Default)]
pub(super) struct VideoWatchdog {
    progress: Option<tokio::time::Instant>,
    repairs: usize,
}

impl VideoWatchdog {
    pub(super) fn observe(&mut self, progress: tokio::time::Instant) -> VideoAction {
        if self.progress != Some(progress) {
            self.progress = Some(progress);
            self.repairs = 0;
        }
        let age = progress.elapsed();
        if age >= VIDEO_STALL_TIMEOUT {
            return VideoAction::Reconnect;
        }
        if REPAIR_AGES
            .get(self.repairs)
            .is_some_and(|threshold| age >= *threshold)
        {
            self.repairs += 1;
            return VideoAction::Repair;
        }
        VideoAction::Healthy
    }
}

async fn send_control(
    sender: &impl Sender,
    message: DesktopControl,
    sent: &impl Fn(&DesktopControl),
) -> anyhow::Result<()> {
    sent(&message);
    // A partially written control is never reused after timeout: the
    // owner ends both legs and drops the channel before reconnecting.
    tokio::time::timeout(Duration::from_secs(2), sender.send(message))
        .await
        .map_err(|_| anyhow::anyhow!("desktop control write stalled"))??;
    Ok(())
}

#[cfg(test)]
async fn pump(
    input: &mut impl Input,
    sender: &impl Sender,
    progress: &watch::Receiver<tokio::time::Instant>,
    started: Instant,
    sent: impl Fn(&DesktopControl),
) -> anyhow::Result<bool> {
    pump_inner(input, sender, progress, None, started, sent).await
}

pub(super) async fn pump_with_liveness(
    input: &mut impl Input,
    sender: &impl Sender,
    progress: &watch::Receiver<tokio::time::Instant>,
    control_progress: &super::liveness::ControlWatchdog,
    started: Instant,
    sent: impl Fn(&DesktopControl),
) -> anyhow::Result<bool> {
    pump_inner(
        input,
        sender,
        progress,
        Some(control_progress),
        started,
        |message| {
            control_progress.sent(message);
            sent(message);
        },
    )
    .await
}

async fn pump_inner(
    input: &mut impl Input,
    sender: &impl Sender,
    progress: &watch::Receiver<tokio::time::Instant>,
    control_progress: Option<&super::liveness::ControlWatchdog>,
    started: Instant,
    sent: impl Fn(&DesktopControl),
) -> anyhow::Result<bool> {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut watchdog = VideoWatchdog::default();
    loop {
        let message = tokio::select! {
            biased;
            _ = tick.tick() => {
                if let Some(monitor) = control_progress { monitor.check()?; }
                let progress = *progress.borrow();
                match watchdog.observe(progress) {
                    VideoAction::Reconnect => {
                        tracing::warn!(last_decoded_age_ms = progress.elapsed().as_millis() as u64,
                            "desktop progress watchdog expired");
                        anyhow::bail!("remote video stopped making progress");
                    },
                    VideoAction::Repair => {
                        tracing::warn!(last_decoded_age_ms = progress.elapsed().as_millis() as u64,
                            "desktop progress repair requested");
                        send_control(sender, DesktopControl::RequestIdr, &sent).await?;
                    },
                    VideoAction::Healthy => {},
                }
                let ts_ms = started.elapsed().as_millis() as u64;
                match control_progress {
                    Some(monitor) => monitor.heartbeat(ts_ms)?,
                    None => DesktopControl::Heartbeat { seq:0, ts_ms },
                }
            },
            message = input.recv() => match message {
                Some(ViewerInput::Control(message)) => message,
                Some(ViewerInput::Close) | None => return Ok(true),
                Some(ViewerInput::InputStalled) => anyhow::bail!("pointer input acknowledgement stalled"),
            },
        };
        send_control(sender, message, &sent).await?;
    }
}

pub(super) async fn run(
    controls: impl Future<Output = anyhow::Result<bool>>,
    media: impl Future<Output = anyhow::Result<bool>>,
    stop: &CancellationToken,
) -> anyhow::Result<bool> {
    // Neither leg is respawned or reconstructed after the other wakes. On
    // completion all pending async work is dropped with the owning session.
    tokio::select! {
        biased;
        _ = stop.cancelled() => Ok(true),
        result = controls => result,
        result = media => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rds_core::{InputEvent, InputKind, local::DesktopUp};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::{
        io::DuplexStream,
        sync::{Mutex, mpsc, oneshot},
    };

    impl Input for mpsc::Receiver<ViewerInput> {
        async fn recv(&mut self) -> Option<ViewerInput> {
            self.recv().await
        }
    }
    struct Wire(Mutex<DuplexStream>);
    impl Sender for Wire {
        async fn send(&self, message: DesktopControl) -> anyhow::Result<()> {
            Ok(
                rds_net::write_frame(&mut *self.0.lock().await, &DesktopUp::Control(message))
                    .await?,
            )
        }
    }
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fresh_video_cannot_mask_unacknowledged_control_and_cancellation_drops_media() {
        let (wire, mut remote) = tokio::io::duplex(256);
        let (tx, mut input) = mpsc::channel(8);
        let (progress, last_frame) = watch::channel(tokio::time::Instant::now());
        let stop = CancellationToken::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let held = dropped.clone();
        let mut task = tokio::spawn(async move {
            let monitor = super::super::liveness::ControlWatchdog::default();
            let media = async {
                let _drop = Dropped(held);
                std::future::pending().await
            };
            run(
                pump_with_liveness(
                    &mut input,
                    &Wire(Mutex::new(wire)),
                    &last_frame,
                    &monitor,
                    Instant::now(),
                    |_| {},
                ),
                media,
                &stop,
            )
            .await
        });
        tx.send(ViewerInput::Control(DesktopControl::Input(InputEvent {
            seq: 37,
            event_ts_ms: 0,
            display_id: 0,
            kind: InputKind::KeyDown { code: 56 },
        })))
        .await
        .unwrap();
        let started = tokio::time::Instant::now();
        let mut inputs = 0;
        let error = loop {
            tokio::select! {
                result = &mut task => break result.unwrap().unwrap_err(),
                message = rds_net::read_frame::<_,DesktopUp>(&mut remote) => {
                    let message = match message {
                        Ok(DesktopUp::Control(message)) => message,
                        Err(error) => {
                            assert_eq!(error.kind(),std::io::ErrorKind::UnexpectedEof);
                            break (&mut task).await.unwrap().unwrap_err();
                        },
                        _ => panic!("unexpected framing"),
                    };
                    match message {
                        DesktopControl::Heartbeat {..} => { progress.send_replace(tokio::time::Instant::now()); },
                        DesktopControl::Input(event) => { assert_eq!(event.seq,37);inputs+=1; },
                        DesktopControl::RequestIdr => panic!("fresh video does not need repair"),
                        _ => panic!("unexpected control"),
                    }
                }
            }
        };
        assert!(
            error
                .to_string()
                .contains("control stopped making progress")
        );
        assert_eq!(started.elapsed(), Duration::from_secs(8));
        assert_eq!(inputs, 1, "no input replay while stalled");
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn matched_control_echoes_preserve_the_healthy_session_during_continuous_media() {
        let (wire, mut remote) = tokio::io::duplex(256);
        let (tx, mut input) = mpsc::channel(8);
        let (progress, last_frame) = watch::channel(tokio::time::Instant::now());
        let monitor = super::super::liveness::ControlWatchdog::default();
        let sending = monitor.clone();
        let task = tokio::spawn(async move {
            pump_with_liveness(
                &mut input,
                &Wire(Mutex::new(wire)),
                &last_frame,
                &sending,
                Instant::now(),
                |_| {},
            )
            .await
        });
        let started = tokio::time::Instant::now();
        let mut previous = None;
        while started.elapsed() < Duration::from_secs(60) {
            let DesktopUp::Control(DesktopControl::Heartbeat { seq, ts_ms }) =
                rds_net::read_frame(&mut remote).await.unwrap()
            else {
                panic!("unexpected heartbeat framing");
            };
            if let Some(n) = previous {
                assert_eq!(seq, n + 1);
            }
            previous = Some(seq);
            assert!(monitor.echoed(seq, ts_ms));
            progress.send_replace(tokio::time::Instant::now());
            assert!(!task.is_finished());
        }
        tx.send(ViewerInput::Close).await.unwrap();
        assert!(task.await.unwrap().unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn video_watchdog_repairs_before_reconnect_and_rearms_only_on_frames() {
        let first = tokio::time::Instant::now();
        let mut watchdog = VideoWatchdog::default();
        assert_eq!(watchdog.observe(first), VideoAction::Healthy);
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(watchdog.observe(first), VideoAction::Repair);
        assert_eq!(watchdog.observe(first), VideoAction::Healthy);
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(watchdog.observe(first), VideoAction::Repair);
        assert_eq!(watchdog.observe(first), VideoAction::Healthy);
        tokio::time::advance(Duration::from_secs(7)).await;
        assert_eq!(watchdog.observe(first), VideoAction::Reconnect);
        let decoded = tokio::time::Instant::now();
        assert_eq!(watchdog.observe(decoded), VideoAction::Healthy);
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(watchdog.observe(decoded), VideoAction::Repair);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_video_repair_keeps_input_and_heartbeats_on_the_existing_channel() {
        let (wire, mut remote) = tokio::io::duplex(256);
        let (tx, mut input) = mpsc::channel(8);
        let (progress, last_frame) = watch::channel(tokio::time::Instant::now());
        let task = tokio::spawn(async move {
            pump(
                &mut input,
                &Wire(Mutex::new(wire)),
                &last_frame,
                Instant::now(),
                |_| {},
            )
            .await
        });
        let mut repaired = false;
        let mut input_observed = false;
        let started = tokio::time::Instant::now();
        // Consume actual framed controls. A repair causes the simulated
        // decoder to publish new progress on the original channel.
        loop {
            let DesktopUp::Control(message) = rds_net::read_frame(&mut remote).await.unwrap()
            else {
                panic!("unexpected control framing");
            };
            match message {
                DesktopControl::RequestIdr => {
                    assert!(!repaired, "one repair must not become an IDR storm");
                    assert_eq!(started.elapsed(), Duration::from_secs(3));
                    repaired = true;
                    progress.send_replace(tokio::time::Instant::now());
                    tx.send(ViewerInput::Control(DesktopControl::Input(InputEvent {
                        seq: 37,
                        event_ts_ms: 0,
                        display_id: 0,
                        kind: InputKind::PointerButton {
                            button: 0x110,
                            pressed: true,
                        },
                    })))
                    .await
                    .unwrap();
                }
                DesktopControl::Heartbeat { .. } if repaired => {
                    progress.send_replace(tokio::time::Instant::now());
                    if started.elapsed() >= Duration::from_secs(60) {
                        break;
                    }
                }
                DesktopControl::Input(event) => {
                    assert_eq!(event.seq, 37);
                    input_observed = true;
                }
                DesktopControl::Heartbeat { .. } => {}
                _ => panic!("unexpected control"),
            }
        }
        assert!(
            !task.is_finished(),
            "media recovery must retain the existing session"
        );
        assert!(
            input_observed,
            "repair must not consume or delay user input"
        );
        tx.send(ViewerInput::Close).await.unwrap();
        assert!(task.await.unwrap().unwrap());
    }

    // Read the actual framed controls, rather than only observing that an
    // input was dequeued. The media leg remains held until the session ends.
    #[tokio::test(start_paused = true)]
    async fn blocked_decode_does_not_hold_input_or_heartbeat_and_close_drops_it() {
        let (wire, mut remote) = tokio::io::duplex(256);
        let (tx, mut input) = mpsc::channel(8);
        let stop = CancellationToken::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let held = dropped.clone();
        let (decoding, started_decode) = oneshot::channel();
        let (_release, blocked) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let (_progress, last_frame) = watch::channel(tokio::time::Instant::now());
            let media = async {
                let _drop = Dropped(held);
                decoding.send(()).unwrap();
                blocked.await.unwrap();
                Ok(false)
            };
            run(
                pump(
                    &mut input,
                    &Wire(Mutex::new(wire)),
                    &last_frame,
                    Instant::now(),
                    |_| {},
                ),
                media,
                &stop,
            )
            .await
        });
        started_decode.await.unwrap();
        let started = tokio::time::Instant::now();
        for (seq, kind) in [
            InputKind::KeyDown { code: 56 },
            InputKind::KeyDown { code: 105 },
            InputKind::KeyUp { code: 105 },
            InputKind::KeyUp { code: 56 },
        ]
        .into_iter()
        .enumerate()
        {
            tx.send(ViewerInput::Control(DesktopControl::Input(InputEvent {
                seq: seq as u64,
                event_ts_ms: 10,
                display_id: 0,
                kind,
            })))
            .await
            .unwrap();
        }
        let mut observed = Vec::new();
        let mut heartbeats = 0;
        while observed.len() < 4 || heartbeats < 2 {
            let message: DesktopUp = tokio::time::timeout(
                Duration::from_millis(1200),
                rds_net::read_frame(&mut remote),
            )
            .await
            .unwrap()
            .unwrap();
            match message {
                DesktopUp::Control(DesktopControl::Input(event)) => observed.push(event),
                DesktopUp::Control(DesktopControl::Heartbeat { .. }) => heartbeats += 1,
                other => panic!("unexpected control: {other:?}"),
            }
        }
        assert_eq!(
            observed.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert!(started.elapsed() < Duration::from_millis(1200));
        assert!(
            !dropped.load(Ordering::SeqCst),
            "decoder must stay held during dispatch"
        );
        tx.send(ViewerInput::Close).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        );
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_decode_cannot_disable_progress_watchdog() {
        let (wire, mut remote) = tokio::io::duplex(256);
        let (_tx, mut input) = mpsc::channel(1);
        let stop = CancellationToken::new();
        let task = tokio::spawn(async move {
            let (_progress, last_frame) = watch::channel(tokio::time::Instant::now());
            run(
                pump(
                    &mut input,
                    &Wire(Mutex::new(wire)),
                    &last_frame,
                    Instant::now(),
                    |_| {},
                ),
                std::future::pending(),
                &stop,
            )
            .await
        });
        let started = tokio::time::Instant::now();
        for _ in 0..15 {
            let _: DesktopUp = rds_net::read_frame(&mut remote).await.unwrap();
        }
        let error = task.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("video stopped making progress"));
        assert_eq!(started.elapsed(), Duration::from_secs(15));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_control_write_is_bounded_and_cancel_releases_both_legs() {
        for cancel in [false, true] {
            let (wire, _remote) = tokio::io::duplex(1);
            let (tx, mut input) = mpsc::channel(1);
            tx.send(ViewerInput::Control(DesktopControl::RequestIdr))
                .await
                .unwrap();
            let stop = CancellationToken::new();
            let canceled = stop.clone();
            let dropped = Arc::new(AtomicBool::new(false));
            let held = dropped.clone();
            let (decoding, started_decode) = oneshot::channel();
            let task = tokio::spawn(async move {
                let (_progress, last_frame) = watch::channel(tokio::time::Instant::now());
                let media = async {
                    let _drop = Dropped(held);
                    decoding.send(()).unwrap();
                    std::future::pending().await
                };
                run(
                    pump(
                        &mut input,
                        &Wire(Mutex::new(wire)),
                        &last_frame,
                        Instant::now(),
                        |_| {},
                    ),
                    media,
                    &stop,
                )
                .await
            });
            started_decode.await.unwrap();
            let started = tokio::time::Instant::now();
            if cancel {
                canceled.cancel();
            }
            let result = task.await.unwrap();
            if cancel {
                assert!(result.unwrap());
                assert_eq!(started.elapsed(), Duration::ZERO);
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("control write stalled")
                );
                assert_eq!(started.elapsed(), Duration::from_secs(2));
            }
            assert!(dropped.load(Ordering::SeqCst));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fresh_decoded_progress_renews_watchdog_without_restarting_control_leg() {
        let (wire, mut remote) = tokio::io::duplex(256);
        let (tx, mut input) = mpsc::channel(1);
        let (progress, last_frame) = watch::channel(tokio::time::Instant::now());
        let stop = CancellationToken::new();
        let task = tokio::spawn(async move {
            run(
                pump(
                    &mut input,
                    &Wire(Mutex::new(wire)),
                    &last_frame,
                    Instant::now(),
                    |_| {},
                ),
                std::future::pending(),
                &stop,
            )
            .await
        });
        let started = tokio::time::Instant::now();
        for second in 0..31 {
            // Count real heartbeat ticks, not all framed controls: soft
            // repair is additional traffic, not a faster timer.
            loop {
                match rds_net::read_frame::<_, DesktopUp>(&mut remote)
                    .await
                    .unwrap()
                {
                    DesktopUp::Control(DesktopControl::Heartbeat { .. }) => break,
                    DesktopUp::Control(DesktopControl::RequestIdr) => {}
                    other => panic!("unexpected control: {other:?}"),
                }
            }
            if second == 10 || second == 20 {
                progress.send_replace(tokio::time::Instant::now());
            }
        }
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert!(!task.is_finished());
        tx.send(ViewerInput::Close).await.unwrap();
        assert!(task.await.unwrap().unwrap());
    }
}
