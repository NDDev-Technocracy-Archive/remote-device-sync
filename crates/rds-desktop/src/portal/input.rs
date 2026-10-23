//! One EIS owner; bounded requests, exact monitor regions, no timed-out replay.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use rds_core::{InputEvent, InputKind};
use reis::{
    PendingRequestResult, ei,
    event::{Device, DeviceCapability, EiEvent, EiEventConverter},
};
use rustix::event::{PollFd, PollFlags};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{DesktopError, InputSink};

const INPUT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_DEVICES: usize = 64;

type Mappings = BTreeMap<u32, (String, Arc<super::capture::Slot>)>;

fn error() -> DesktopError {
    DesktopError::Input("Wayland input unavailable or no longer authorized".into())
}

struct Request {
    input_seq: u64,
    lease: u64,
    live: Arc<AtomicBool>,
    era: u64,
    index: u32,
    extent: (u32, u32),
    action: InputKind,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<(), DesktopError>>,
}

struct Pending {
    applied_at: Instant,
    queue_us: u128,
    request: Request,
    done: Rc<Cell<bool>>,
    flushed: bool,
    canceled_until: Option<Instant>,
}
fn check_pending(
    pending: &mut Option<Pending>,
    era: u64,
    now: Instant,
) -> Result<(), DesktopError> {
    let Some(pending) = pending else {
        return Ok(());
    };
    let reason = if pending.request.era != era {
        Some("device generation changed")
    } else if now >= pending.request.deadline {
        Some("sync deadline elapsed")
    } else if !pending.request.live.load(Ordering::Acquire) {
        Some("caller lease canceled")
    } else {
        None
    };
    if let Some(reason) = reason
        && pending.canceled_until.is_none()
    {
        tracing::warn!(
            reason,
            flushed = pending.flushed,
            "Wayland EIS input request canceled"
        );
        // Unwritten bytes cannot be removed from the protocol buffer. Closing
        // that peer is the only safe way to prevent stale input from flushing.
        if !pending.flushed {
            return Err(error());
        }
        pending.request.live.store(false, Ordering::Release);
        let _ = pending.request.reply.try_send(Err(error()));
        pending.canceled_until = Some(now + INPUT_TIMEOUT);
    }
    if pending.canceled_until.is_some_and(|end| now >= end) && !pending.done.get() {
        tracing::warn!("Wayland EIS canceled sync drain deadline elapsed");
        return Err(error());
    }
    Ok(())
}

pub(super) struct Group {
    sender: mpsc::SyncSender<Request>,
    stop: Arc<AtomicBool>,
    pub ready: Arc<AtomicBool>,
    pub era: Arc<AtomicU64>,
    leases: Arc<Semaphore>,
    next: AtomicU64,
    task: Mutex<Option<tokio::task::JoinHandle<Result<(), DesktopError>>>>,
}

impl Group {
    pub fn start(fd: OwnedFd, mappings: Mappings) -> Result<Self, DesktopError> {
        static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let permit = WORKERS
            .get_or_init(|| Arc::new(Semaphore::new(4)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| error())?;
        let (sender, receiver) = mpsc::sync_channel(64);
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let era = Arc::new(AtomicU64::new(0));
        let (stopping, readiness, generation) = (stop.clone(), ready.clone(), era.clone());
        let task = tokio::task::spawn_blocking(move || {
            // The converter has non-Send callbacks. Construct and retain it on
            // this worker instead of introducing an unsafe Send assertion.
            let _permit = permit;
            let _exit = WorkerExit {
                ready: &readiness,
                stop: &stopping,
            };
            let result = run(fd, mappings, receiver, &stopping, &readiness, &generation);
            if result.is_err() {
                tracing::warn!("Wayland EIS native worker failed");
            }
            result
        });
        Ok(Self {
            sender,
            stop,
            ready,
            era,
            leases: Arc::new(Semaphore::new(64)),
            next: AtomicU64::new(1),
            task: Mutex::new(Some(task)),
        })
    }

    pub fn sink(&self, index: u32, extent: (u32, u32)) -> Result<Box<dyn InputSink>, DesktopError> {
        if self.stop.load(Ordering::Acquire) || !self.ready.load(Ordering::Acquire) {
            return Err(error());
        }
        let permit = self
            .leases
            .clone()
            .try_acquire_owned()
            .map_err(|_| error())?;
        let lease = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| error())?;
        Ok(Box::new(Sink {
            sender: self.sender.clone(),
            stop: self.stop.clone(),
            ready: self.ready.clone(),
            era: self.era.clone(),
            expected_era: self.era.load(Ordering::Acquire),
            index,
            extent,
            lease,
            live: Arc::new(AtomicBool::new(true)),
            _permit: permit,
        }))
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    pub async fn close(&self) -> Result<(), DesktopError> {
        self.stop();
        let task = self.task.lock().map_err(|_| error())?.take();
        if let Some(task) = task {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .map_err(|_| error())?
                .map_err(|_| error())??;
        }
        Ok(())
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        self.stop();
    }
}

struct WorkerExit<'a> {
    ready: &'a AtomicBool,
    stop: &'a AtomicBool,
}
impl Drop for WorkerExit<'_> {
    fn drop(&mut self) {
        self.ready.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
    }
}

struct Sink {
    sender: mpsc::SyncSender<Request>,
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    era: Arc<AtomicU64>,
    expected_era: u64,
    index: u32,
    extent: (u32, u32),
    lease: u64,
    live: Arc<AtomicBool>,
    _permit: OwnedSemaphorePermit,
}
impl InputSink for Sink {
    fn inject(&mut self, event: &InputEvent) -> Result<(), DesktopError> {
        if event.display_id != self.index
            || !self.live.load(Ordering::Acquire)
            || self.stop.load(Ordering::Acquire)
            || !self.ready.load(Ordering::Acquire)
            || self.expected_era != self.era.load(Ordering::Acquire)
        {
            return Err(error());
        }
        let (reply, response) = mpsc::sync_channel(1);
        self.sender
            .try_send(Request {
                input_seq: event.seq,
                lease: self.lease,
                live: self.live.clone(),
                era: self.expected_era,
                index: self.index,
                extent: self.extent,
                action: event.kind.clone(),
                deadline: Instant::now() + INPUT_TIMEOUT,
                reply,
            })
            .map_err(|_| error())?;
        match response.recv_timeout(INPUT_TIMEOUT) {
            Ok(result) => result,
            Err(_) => {
                self.live.store(false, Ordering::Release);
                Err(error())
            }
        }
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

struct NativeDevice {
    device: Device,
    resumed: bool,
}
struct Holds {
    live: Arc<AtomicBool>,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    pointer: Option<Device>,
    scroll: (f64, f64),
}

struct Actor {
    devices: Vec<NativeDevice>,
    holds: BTreeMap<u64, Holds>,
    sequence: u32,
    origin: Instant,
    converter: EiEventConverter,
    context: ei::Context,
    mappings: Mappings,
}

impl Actor {
    fn events(&mut self, era: &AtomicU64) -> Result<(), DesktopError> {
        self.context.read().map_err(|_| error())?;
        while let Some(event) = self.context.pending_event() {
            let PendingRequestResult::Request(event) = event else {
                return Err(error());
            };
            self.converter.handle_event(event).map_err(|_| error())?;
            while let Some(event) = self.converter.next_event() {
                match event {
                    EiEvent::Disconnected(_) => {
                        tracing::warn!("Wayland EIS peer disconnected");
                        return Err(error());
                    }
                    EiEvent::SeatAdded(event) => event.seat.bind_capabilities(
                        DeviceCapability::Keyboard
                            | DeviceCapability::PointerAbsolute
                            | DeviceCapability::Button
                            | DeviceCapability::Scroll,
                    ),
                    EiEvent::DeviceAdded(event) => {
                        if self.devices.len() >= MAX_DEVICES
                            || event.device.regions().len() > super::session::MAX_MONITORS
                        {
                            return Err(error());
                        }
                        self.devices.push(NativeDevice {
                            device: event.device,
                            resumed: false,
                        });
                    }
                    EiEvent::DeviceResumed(event) => {
                        if let Some(device) =
                            self.devices.iter_mut().find(|d| d.device == event.device)
                        {
                            device.resumed = true;
                            self.sequence = self.sequence.checked_add(1).ok_or_else(error)?;
                            device
                                .device
                                .device()
                                .start_emulating(event.serial, self.sequence);
                        }
                    }
                    EiEvent::DevicePaused(event) => {
                        tracing::info!("Wayland EIS device paused; invalidating old input leases");
                        self.invalidate_device(&event.device);
                        if let Some(device) =
                            self.devices.iter_mut().find(|d| d.device == event.device)
                        {
                            device.resumed = false;
                        }
                        era.fetch_add(1, Ordering::AcqRel);
                        self.release_closed()?;
                    }
                    EiEvent::DeviceRemoved(event) => {
                        tracing::info!("Wayland EIS device removed; invalidating old input leases");
                        self.invalidate_device(&event.device);
                        self.devices.retain(|d| d.device != event.device);
                        era.fetch_add(1, Ordering::AcqRel);
                        self.release_closed()?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn keyboard(&self) -> Result<&Device, DesktopError> {
        let mut matches = self
            .devices
            .iter()
            .filter(|d| d.resumed && d.device.has_capability(DeviceCapability::Keyboard));
        let first = matches.next().ok_or_else(error)?;
        if matches.next().is_some() {
            return Err(error());
        }
        Ok(&first.device)
    }

    fn invalidate_device(&mut self, device: &Device) {
        for hold in self.holds.values_mut() {
            hold.live.store(false, Ordering::Release);
            // Pause/removal neutralizes only the affected native device.
            // Other devices still need explicit releases for ended leases.
            if device.has_capability(DeviceCapability::Keyboard) {
                hold.keys.clear();
            }
            if hold.pointer.as_ref() == Some(device) {
                hold.buttons.clear();
            }
        }
    }

    fn pointer(&self, index: u32) -> Result<(&Device, super::geometry::Region), DesktopError> {
        let (mapping, _) = self.mappings.get(&index).ok_or_else(error)?;
        let mut matches = self.devices.iter().filter(|d| d.resumed).flat_map(|d| {
            d.device.regions().iter().filter_map(move |r| {
                (r.mapping_id.as_deref() == Some(mapping.as_str())).then_some((&d.device, r))
            })
        });
        let (device, region) = matches.next().ok_or_else(error)?;
        if matches.next().is_some() || !device.has_capability(DeviceCapability::PointerAbsolute) {
            return Err(error());
        }
        Ok((
            device,
            super::geometry::Region {
                mapping_id: mapping.clone(),
                x: region.x,
                y: region.y,
                width: region.width,
                height: region.height,
            },
        ))
    }

    fn ready(&self) -> bool {
        self.keyboard().is_ok() && self.mappings.keys().all(|i| self.pointer(*i).is_ok())
    }
    fn frame(&self, device: &Device) {
        device.device().frame(
            self.converter.connection().serial(),
            self.origin.elapsed().as_micros().min(u64::MAX as u128) as u64,
        );
    }

    fn apply(&mut self, request: &Request) -> Result<(), DesktopError> {
        let (_, slot) = self.mappings.get(&request.index).ok_or_else(error)?;
        if slot.extent()? != Some(request.extent) {
            return Err(error());
        }
        self.holds.entry(request.lease).or_insert_with(|| Holds {
            live: request.live.clone(),
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            pointer: None,
            scroll: (0.0, 0.0),
        });
        match request.action {
            InputKind::KeyDown { code } | InputKind::KeyUp { code } => {
                if code == 0 || code > 0x2ff {
                    return Err(error());
                }
                let down = matches!(request.action, InputKind::KeyDown { .. });
                let was_global = self.holds.values().any(|h| h.keys.contains(&code));
                let held = self.holds.get_mut(&request.lease).ok_or_else(error)?;
                if down {
                    held.keys.insert(code);
                } else {
                    held.keys.remove(&code);
                }
                let is_global = self.holds.values().any(|h| h.keys.contains(&code));
                // EIS uses physical holds and compositor repeat semantics. Do
                // not generate a second repeat loop for duplicate source down.
                if was_global != is_global {
                    let device = self.keyboard()?;
                    device.interface::<ei::Keyboard>().ok_or_else(error)?.key(
                        code,
                        if is_global {
                            ei::keyboard::KeyState::Press
                        } else {
                            ei::keyboard::KeyState::Released
                        },
                    );
                    self.frame(device);
                }
            }
            InputKind::PointerMove { x, y } => {
                let (device, region) = self.pointer(request.index)?;
                let (x, y) = region.map(&region.mapping_id, request.extent, (x, y))?;
                device
                    .interface::<ei::PointerAbsolute>()
                    .ok_or_else(error)?
                    .motion_absolute(x as f32, y as f32);
                self.frame(device);
            }
            InputKind::PointerMotion { .. } => {
                return Err(DesktopError::Input(
                    "relative pointer input is not scoped by this portal region".into(),
                ));
            }
            InputKind::PointerButton { button, pressed } => {
                let button = u32::try_from(button)
                    .ok()
                    .filter(|b| (0x110..=0x117).contains(b))
                    .ok_or_else(error)?;
                let (device, _) = self.pointer(request.index)?;
                let device = device.clone();
                let was_global = self
                    .holds
                    .values()
                    .any(|h| h.pointer.as_ref() == Some(&device) && h.buttons.contains(&button));
                let held = self.holds.get_mut(&request.lease).ok_or_else(error)?;
                if held.pointer.as_ref().is_some_and(|old| *old != device) {
                    return Err(error());
                }
                held.pointer = Some(device.clone());
                if pressed {
                    held.buttons.insert(button)
                } else {
                    held.buttons.remove(&button)
                };
                let is_global = self
                    .holds
                    .values()
                    .any(|h| h.pointer.as_ref() == Some(&device) && h.buttons.contains(&button));
                if was_global != is_global {
                    device.interface::<ei::Button>().ok_or_else(error)?.button(
                        button,
                        if is_global {
                            ei::button::ButtonState::Press
                        } else {
                            ei::button::ButtonState::Released
                        },
                    );
                    self.frame(&device);
                }
            }
            InputKind::Scroll { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() || dx.abs() > 32.0 || dy.abs() > 32.0 {
                    return Err(error());
                }
                let (device, _) = self.pointer(request.index)?;
                let device = device.clone();
                let held = self.holds.get_mut(&request.lease).ok_or_else(error)?;
                held.scroll.0 -= dx * 120.0;
                held.scroll.1 -= dy * 120.0;
                let (x, y) = (held.scroll.0.trunc() as i32, held.scroll.1.trunc() as i32);
                held.scroll.0 -= f64::from(x);
                held.scroll.1 -= f64::from(y);
                device
                    .interface::<ei::Scroll>()
                    .ok_or_else(error)?
                    .scroll_discrete(x, y);
                self.frame(&device);
            }
        }
        Ok(())
    }

    fn release_closed(&mut self) -> Result<(), DesktopError> {
        let ended: Vec<_> = self
            .holds
            .iter()
            .filter(|(_, h)| !h.live.load(Ordering::Acquire))
            .map(|(id, _)| *id)
            .collect();
        for id in ended {
            let Some(held) = self.holds.remove(&id) else {
                continue;
            };
            for key in held.keys {
                if !self.holds.values().any(|h| h.keys.contains(&key)) {
                    let device = self.keyboard()?;
                    device
                        .interface::<ei::Keyboard>()
                        .ok_or_else(error)?
                        .key(key, ei::keyboard::KeyState::Released);
                    self.frame(device);
                }
            }
            for button in held.buttons {
                let device = held.pointer.as_ref().ok_or_else(error)?;
                if self
                    .holds
                    .values()
                    .any(|h| h.pointer.as_ref() == Some(device) && h.buttons.contains(&button))
                {
                    continue;
                }
                device
                    .interface::<ei::Button>()
                    .ok_or_else(error)?
                    .button(button, ei::button::ButtonState::Released);
                self.frame(device);
            }
        }
        Ok(())
    }
}

fn poll(context: &ei::Context, writable: bool) -> Result<(), DesktopError> {
    let mut fds = [PollFd::new(
        context,
        PollFlags::IN
            | if writable {
                PollFlags::OUT
            } else {
                PollFlags::empty()
            },
    )];
    rustix::event::poll(
        &mut fds,
        Some(&rustix::time::Timespec {
            tv_sec: 0,
            tv_nsec: 10_000_000,
        }),
    )
    .map_err(|_| error())?;
    if fds[0]
        .revents()
        .intersects(PollFlags::ERR | PollFlags::NVAL | PollFlags::HUP)
    {
        return Err(error());
    }
    Ok(())
}

fn flush(context: &ei::Context) -> Result<bool, DesktopError> {
    match context.flush() {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::AGAIN) => Ok(true),
        Err(_) => Err(error()),
    }
}

fn run(
    fd: OwnedFd,
    mappings: Mappings,
    requests: mpsc::Receiver<Request>,
    stop: &AtomicBool,
    ready: &AtomicBool,
    era: &AtomicU64,
) -> Result<(), DesktopError> {
    let context = ei::Context::new(UnixStream::from(fd)).map_err(|_| error())?;
    let mut handshake =
        reis::handshake::EiHandshaker::new("RDS", ei::handshake::ContextType::Sender);
    let end = Instant::now() + INPUT_TIMEOUT;
    let response = loop {
        if stop.load(Ordering::Acquire) || Instant::now() >= end {
            return Err(error());
        }
        poll(&context, flush(&context)?)?;
        context.read().map_err(|_| error())?;
        let mut response = None;
        while let Some(event) = context.pending_event() {
            let PendingRequestResult::Request(event) = event else {
                return Err(error());
            };
            if let Some(done) = handshake.handle_event(event).map_err(|_| error())? {
                response = Some(done);
                break;
            }
        }
        if let Some(response) = response {
            break response;
        }
    };
    let converter = EiEventConverter::new(&context, response);
    let mut actor = Actor {
        context,
        converter,
        mappings,
        devices: Vec::new(),
        holds: BTreeMap::new(),
        sequence: 0,
        origin: Instant::now(),
    };
    let mut pending: Option<Pending> = None;
    // Diagnostic-only target, capped per actor lifetime. No input payloads.
    let mut timing_samples = 0_u32;
    while !stop.load(Ordering::Acquire) {
        check_pending(&mut pending, era.load(Ordering::Acquire), Instant::now())?;
        actor.release_closed()?;
        let writable = flush(&actor.context)?;
        if !writable && let Some(pending) = &mut pending {
            pending.flushed = true;
        }
        poll(&actor.context, writable)?;
        actor.events(era)?;
        let available = actor.ready();
        ready.store(available, Ordering::Release);
        check_pending(&mut pending, era.load(Ordering::Acquire), Instant::now())?;
        actor.release_closed()?;
        if pending.as_ref().is_some_and(|pending| pending.done.get()) {
            let pending = pending.take().ok_or_else(error)?;
            if pending.canceled_until.is_none() {
                if timing_samples < 128
                    && tracing::enabled!(target: "rds_desktop::input_boundary", tracing::Level::DEBUG)
                {
                    timing_samples += 1;
                    tracing::debug!(target: "rds_desktop::input_boundary",
                        input_seq = pending.request.input_seq,
                        lease = pending.request.lease,
                        queue_us = pending.queue_us,
                        apply_sync_us = pending.applied_at.elapsed().as_micros(),
                        "EIS input boundary");
                }
                let _ = pending.request.reply.try_send(Ok(()));
            }
            // A canceled, already-flushed action is never replayed or ACKed.
            // Its actual sync callback has now drained, so fresh leases can run.
        }
        if pending.is_none() {
            match requests.try_recv() {
                Ok(request) => {
                    if !available
                        || !request.live.load(Ordering::Acquire)
                        || request.era != era.load(Ordering::Acquire)
                        || Instant::now() >= request.deadline
                    {
                        let _ = request.reply.try_send(Err(error()));
                        continue;
                    }
                    let applied_at = Instant::now();
                    let queue_us = applied_at
                        .duration_since(request.deadline - INPUT_TIMEOUT)
                        .as_micros();
                    if let Err(error) = actor.apply(&request) {
                        let _ = request.reply.try_send(Err(error));
                        continue;
                    }
                    let done = Rc::new(Cell::new(false));
                    let mark = done.clone();
                    let callback = actor.converter.connection().connection().sync(1);
                    actor
                        .converter
                        .add_callback_handler(callback, move |_| mark.set(true));
                    pending = Some(Pending {
                        applied_at,
                        queue_us,
                        request,
                        done,
                        flushed: false,
                        canceled_until: None,
                    });
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }
    // Closing this EIS peer releases its virtual-device holds in the compositor;
    // it has no ownership over another application's (e.g. Deskflow) devices.
    Ok(())
}

#[cfg(test)]
mod exit_tests {
    use super::*;
    #[test]
    fn canceled_unflushed_input_fails_closed_and_flushed_sync_drain_remains_bounded() {
        let now = Instant::now();
        let request = |reply| Request {
            input_seq: 0,
            lease: 1,
            live: Arc::new(AtomicBool::new(false)),
            era: 0,
            index: 0,
            extent: (100, 100),
            action: InputKind::KeyDown { code: 30 },
            deadline: now + INPUT_TIMEOUT,
            reply,
        };
        let (reply, response) = mpsc::sync_channel(1);
        let mut unflushed = Some(Pending {
            applied_at: now,
            queue_us: 0,
            request: request(reply),
            done: Rc::new(Cell::new(false)),
            flushed: false,
            canceled_until: None,
        });
        assert!(check_pending(&mut unflushed, 0, now).is_err());
        assert!(
            response.try_recv().is_err(),
            "no fabricated success for bytes that cannot be discarded"
        );
        let (reply, response) = mpsc::sync_channel(1);
        let mut flushed = Some(Pending {
            applied_at: now,
            queue_us: 0,
            request: request(reply),
            done: Rc::new(Cell::new(false)),
            flushed: true,
            canceled_until: None,
        });
        assert!(check_pending(&mut flushed, 0, now).is_ok());
        assert!(response.try_recv().unwrap().is_err());
        assert!(
            check_pending(&mut flushed, 0, now + INPUT_TIMEOUT).is_err(),
            "never accumulate unanswered native callbacks indefinitely"
        );
    }

    #[test]
    fn input_worker_unwind_seals_old_leases_and_readiness() {
        let ready = AtomicBool::new(true);
        let stop = AtomicBool::new(false);
        let result = std::panic::catch_unwind(|| {
            let _exit = WorkerExit {
                ready: &ready,
                stop: &stop,
            };
            panic!("fixture input worker fault");
        });
        assert!(result.is_err());
        assert!(!ready.load(Ordering::Acquire));
        assert!(stop.load(Ordering::Acquire));
    }
}
