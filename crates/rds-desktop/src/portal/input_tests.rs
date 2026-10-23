//! A real EI socket pair; no compositor or ambient desktop receives input.
use super::input::Group;
use rds_core::{InputEvent, InputKind};
use reis::{PendingRequestResult, event::DeviceCapability};
use reis::{
    eis,
    handshake::EisHandshaker,
    request::{EisRequest, EisRequestConverter},
};
use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Key(u32, bool),
    Move(f32, f32),
}

#[derive(Default)]
struct SyncControl {
    hold_next: AtomicBool,
    pause_next: AtomicBool,
    blocked: AtomicBool,
    release: AtomicBool,
}

fn server(
    socket: UnixStream,
    seen: Arc<Mutex<Vec<Seen>>>,
    pause: mpsc::Receiver<()>,
    stop: Arc<AtomicBool>,
    control: Option<Arc<SyncControl>>,
) {
    let context = eis::Context::new(socket).unwrap();
    let mut handshake = EisHandshaker::new(&context, 1);
    let mut converter = None;
    let mut keyboard = None;
    let mut advertised = false;
    let mut held_sync = None;
    let deadline = Instant::now() + Duration::from_secs(12);
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        if context.read().is_err() {
            return;
        }
        while let Some(request) = context.pending_request() {
            let PendingRequestResult::Request(request) = request else {
                panic!("invalid fixture request");
            };
            if let Some(converter) = converter.as_mut() {
                if matches!(
                    &request,
                    eis::Request::Connection(_, eis::connection::Request::Sync { .. })
                ) && let Some(control) = &control
                    && control.hold_next.swap(false, Ordering::AcqRel)
                {
                    assert!(held_sync.is_none());
                    held_sync = Some(request);
                    if control.pause_next.swap(false, Ordering::AcqRel) {
                        let keyboard: &reis::request::Device = keyboard.as_ref().unwrap();
                        keyboard.paused();
                        keyboard.resumed();
                    }
                    control.blocked.store(true, Ordering::Release);
                    continue;
                }
                EisRequestConverter::handle_request(converter, request).unwrap();
            } else if let Some(response) = handshake.handle_request(request).unwrap() {
                let ready = EisRequestConverter::new(&context, response, 1);
                let _ = ready.handle().add_seat(
                    Some("fixture"),
                    DeviceCapability::Keyboard
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Button
                        | DeviceCapability::Scroll,
                );
                converter = Some(ready);
            }
        }
        if let Some(converter) = converter.as_mut() {
            if let Some(control) = &control
                && control.release.load(Ordering::Acquire)
                && let Some(request) = held_sync.take()
            {
                converter.handle_request(request).unwrap();
                control.blocked.store(false, Ordering::Release);
            }
            while let Some(request) = converter.next_request() {
                match request {
                    EisRequest::Bind(bind) if !advertised => {
                        advertised = true;
                        let key = bind.seat.add_device(
                            Some("keyboard"),
                            eis::device::DeviceType::Virtual,
                            DeviceCapability::Keyboard.into(),
                            |_| {},
                        );
                        key.resumed();
                        keyboard = Some(key);
                        let pointer = bind.seat.add_device(
                            Some("pointer"),
                            eis::device::DeviceType::Virtual,
                            DeviceCapability::PointerAbsolute
                                | DeviceCapability::Button
                                | DeviceCapability::Scroll,
                            |device| {
                                device.device().region_mapping_id("4k-region");
                                device.device().region(1920, 0, 2880, 1620, 4.0 / 3.0);
                                device.device().region_mapping_id("fhd-region");
                                device.device().region(0, 540, 1920, 1080, 1.0);
                            },
                        );
                        pointer.resumed();
                    }
                    EisRequest::KeyboardKey(event) => seen.lock().unwrap().push(Seen::Key(
                        event.key,
                        event.state == eis::keyboard::KeyState::Press,
                    )),
                    EisRequest::PointerMotionAbsolute(event) => seen
                        .lock()
                        .unwrap()
                        .push(Seen::Move(event.dx_absolute, event.dy_absolute)),
                    EisRequest::Disconnect => return,
                    _ => {}
                }
            }
            if pause.try_recv().is_ok() {
                let keyboard = keyboard.as_ref().unwrap();
                keyboard.paused();
                keyboard.resumed();
            }
        }
        context.flush().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
fn event(display_id: u32, kind: InputKind) -> InputEvent {
    InputEvent {
        display_id,
        seq: 1,
        kind,
        event_ts_ms: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_monitor_regions_shared_key_holds_and_pause_invalidate_old_leases() {
    let (client, socket) = UnixStream::pair().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (pause, commands) = mpsc::sync_channel(1);
    let (recording, stopping) = (seen.clone(), stop.clone());
    let fixture =
        tokio::task::spawn_blocking(move || server(socket, recording, commands, stopping, None));
    let mappings = BTreeMap::from([
        (
            10,
            (
                "4k-region".into(),
                super::capture::Slot::fixture(10, (3840, 2160)),
            ),
        ),
        (
            20,
            (
                "fhd-region".into(),
                super::capture::Slot::fixture(20, (1920, 1080)),
            ),
        ),
    ]);
    let group = Group::start(client.into(), mappings).unwrap();
    until(|| group.ready.load(Ordering::Acquire)).await;
    let mut first = group.sink(10, (3840, 2160)).unwrap();
    let mut second = group.sink(20, (1920, 1080)).unwrap();
    first
        .inject(&event(
            10,
            InputKind::PointerMove {
                x: 1920.0,
                y: 1080.0,
            },
        ))
        .unwrap();
    second
        .inject(&event(20, InputKind::PointerMove { x: 960.0, y: 540.0 }))
        .unwrap();
    assert!(
        first
            .inject(&event(20, InputKind::KeyDown { code: 30 }))
            .is_err()
    );
    assert!(
        first
            .inject(&event(
                10,
                InputKind::PointerMove {
                    x: f64::NAN,
                    y: 0.0
                }
            ))
            .is_err()
    );
    // A fresh, unowned key release must send only native sync, never alter a
    // physical/foreign application's key state. The fixture records all EI keys.
    first
        .inject(&event(10, InputKind::KeyUp { code: 28 }))
        .unwrap();
    second
        .inject(&event(20, InputKind::KeyUp { code: 28 }))
        .unwrap();
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|seen| matches!(seen, Seen::Key(..)))
    );
    first
        .inject(&event(10, InputKind::KeyDown { code: 30 }))
        .unwrap();
    second
        .inject(&event(20, InputKind::KeyDown { code: 30 }))
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            Seen::Move(3360.0, 810.0),
            Seen::Move(960.0, 1080.0),
            Seen::Key(30, true)
        ]
    );
    drop(first);
    second
        .inject(&event(20, InputKind::KeyUp { code: 30 }))
        .unwrap();
    until(|| seen.lock().unwrap().last() == Some(&Seen::Key(30, false))).await;
    second
        .inject(&event(20, InputKind::KeyDown { code: 31 }))
        .unwrap();
    let era = group.era.load(Ordering::Acquire);
    pause.send(()).unwrap();
    until(|| group.era.load(Ordering::Acquire) != era).await;
    assert!(
        second
            .inject(&event(20, InputKind::KeyDown { code: 32 }))
            .is_err()
    );
    drop(second);
    until(|| group.ready.load(Ordering::Acquire)).await;
    let mut new = group.sink(10, (3840, 2160)).unwrap();
    new.inject(&event(10, InputKind::KeyDown { code: 33 }))
        .unwrap();
    drop(new);
    until(|| seen.lock().unwrap().last() == Some(&Seen::Key(33, false))).await;
    assert!(!seen.lock().unwrap().contains(&Seen::Key(32, true)));
    group.close().await.unwrap();
    stop.store(true, Ordering::Release);
    fixture.await.unwrap();
}

#[tokio::test]
async fn silent_eis_peer_cannot_hold_handshake_admission_forever() {
    let (client, _silent) = UnixStream::pair().unwrap();
    let group = Group::start(client.into(), BTreeMap::new()).unwrap();
    until(|| group.stopped()).await;
    assert!(!group.ready.load(Ordering::Acquire));
    assert!(group.close().await.is_err());
}

async fn canceled_flushed_sync_preserves_the_native_actor(pause_device: bool) {
    let (client, socket) = UnixStream::pair().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let control = Arc::new(SyncControl::default());
    let (_pause, commands) = mpsc::sync_channel(1);
    let (recording, stopping, controlling) = (seen.clone(), stop.clone(), control.clone());
    let fixture = tokio::task::spawn_blocking(move || {
        server(socket, recording, commands, stopping, Some(controlling))
    });
    let group = Group::start(
        client.into(),
        BTreeMap::from([(
            10,
            (
                "4k-region".into(),
                super::capture::Slot::fixture(10, (3840, 2160)),
            ),
        )]),
    )
    .unwrap();
    until(|| group.ready.load(Ordering::Acquire)).await;
    let mut old = group.sink(10, (3840, 2160)).unwrap();
    control.pause_next.store(pause_device, Ordering::Release);
    control.hold_next.store(true, Ordering::Release);
    let injection = tokio::task::spawn_blocking(move || {
        let result = old.inject(&event(10, InputKind::KeyDown { code: 40 }));
        (old, result)
    });
    // Seeing Sync on the real socket proves the action was already flushed.
    until(|| control.blocked.load(Ordering::Acquire)).await;
    let (mut old, result) = tokio::time::timeout(Duration::from_secs(3), injection)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.is_err(),
        "canceled input must never receive a successful ACK"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !group.stopped(),
        "normal pause/caller deadline must not seal all capture/input"
    );
    assert!(
        old.inject(&event(10, InputKind::KeyDown { code: 41 }))
            .is_err()
    );
    control.release.store(true, Ordering::Release);
    until(|| !control.blocked.load(Ordering::Acquire)).await;
    until(|| group.ready.load(Ordering::Acquire)).await;
    let mut fresh = group.sink(10, (3840, 2160)).unwrap();
    fresh
        .inject(&event(10, InputKind::KeyDown { code: 42 }))
        .unwrap();
    drop(fresh);
    until(|| seen.lock().unwrap().last() == Some(&Seen::Key(42, false))).await;
    let events = seen.lock().unwrap().clone();
    assert_eq!(
        events.iter().filter(|e| **e == Seen::Key(40, true)).count(),
        1,
        "no canceled action replay"
    );
    assert!(
        !events.contains(&Seen::Key(41, true)),
        "invalidated lease cannot target the new device generation"
    );
    if !pause_device {
        assert!(
            events.contains(&Seen::Key(40, false)),
            "caller timeout releases its owned key hold"
        );
    }
    drop(old);
    group.close().await.unwrap();
    stop.store(true, Ordering::Release);
    fixture.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_pause_during_flushed_sync_rejects_old_lease_and_admits_fresh_input() {
    canceled_flushed_sync_preserves_the_native_actor(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_real_sync_after_caller_deadline_does_not_kill_or_replay_native_input() {
    canceled_flushed_sync_preserves_the_native_actor(false).await;
}
