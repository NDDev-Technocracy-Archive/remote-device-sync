//! Owned PipeWire I/O on one bounded native worker, with newest shared pixels.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::{properties::properties, spa};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{DesktopError, RawFrame};

const MAX_PIXELS_BYTES: usize = 128 * 1024 * 1024;
const MAX_RETAINED_BYTES: u32 = 256 * 1024 * 1024;

fn copy_interval(value: Option<&str>) -> Result<Option<Duration>, DesktopError> {
    value
        .map(|value| {
            value
                .parse::<u32>()
                .ok()
                .filter(|fps| (1..=240).contains(fps))
                .map(|fps| Duration::from_secs_f64(1.0 / f64::from(fps)))
                .ok_or_else(|| DesktopError::Capture("invalid RDS_PORTAL_CAPTURE_MAX_FPS".into()))
        })
        .transpose()
}

#[derive(Default)]
struct CopyCadence {
    due: Option<Instant>,
}
impl CopyCadence {
    fn ready(&self, now: Instant) -> bool {
        self.due.is_none_or(|due| now >= due)
    }

    fn copied(&mut self, now: Instant, interval: Option<Duration>) {
        self.due = interval.map(|interval| {
            // Keep phase across small callback jitter; after a pause start a
            // fresh interval instead of admitting a burst of catch-up copies.
            self.due
                .map(|due| due + interval)
                .filter(|due| *due > now)
                .unwrap_or(now + interval)
        });
    }
}

pub(super) struct Image {
    pub frame: RawFrame,
    /// Local PipeWire ingress, not a claim about compositor capture time.
    pub received: Instant,
    pub generation: u64,
}

#[derive(Default)]
struct State {
    extent: Option<(u32, u32)>,
    image: Option<Arc<Image>>,
    failed: bool,
    generation: u64,
}

pub(super) struct Slot {
    pub index: u32,
    state: Mutex<State>,
    changed: Condvar,
    readers: AtomicUsize,
}

impl Slot {
    #[cfg(test)]
    pub(super) fn fixture(index: u32, extent: (u32, u32)) -> Arc<Self> {
        Arc::new(Self {
            index,
            state: Mutex::new(State {
                extent: Some(extent),
                ..Default::default()
            }),
            changed: Condvar::new(),
            readers: AtomicUsize::new(0),
        })
    }
    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        self.readers.fetch_add(1, Ordering::AcqRel);
        Subscription(self.clone())
    }
    pub fn extent(&self) -> Result<Option<(u32, u32)>, DesktopError> {
        let state = self.state.lock().map_err(|_| failure())?;
        if state.failed {
            return Err(failure());
        }
        Ok(state.extent)
    }

    pub fn image(&self, timeout: Duration) -> Result<Arc<Image>, DesktopError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().map_err(|_| failure())?;
        while state.image.is_none() && !state.failed {
            let wait = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(failure)?;
            state = self
                .changed
                .wait_timeout(state, wait)
                .map_err(|_| failure())?
                .0;
        }
        if state.failed {
            return Err(failure());
        }
        state.image.clone().ok_or_else(failure)
    }

    fn fail(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.failed = true;
            state.image = None;
        }
        self.changed.notify_all();
    }
}

pub(super) struct Subscription(pub Arc<Slot>);
impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.readers.fetch_sub(1, Ordering::AcqRel);
    }
}

fn failure() -> DesktopError {
    DesktopError::Capture("PipeWire monitor unavailable".into())
}

pub(super) struct Group {
    pub slots: Vec<Arc<Slot>>,
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<Result<(), DesktopError>>>,
}

impl Group {
    pub fn start(video: OwnedFd, sources: Vec<(u32, u32)>) -> Result<Self, DesktopError> {
        if sources.is_empty() || sources.len() > super::session::MAX_MONITORS {
            return Err(failure());
        }
        // One explicit source-wide cap, read once at preparation. Absent means
        // the existing native cadence; it does not change encoder negotiation.
        let configured = std::env::var("RDS_PORTAL_CAPTURE_MAX_FPS");
        let interval = match configured {
            Ok(value) => copy_interval(Some(&value))?,
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => {
                return Err(DesktopError::Capture(
                    "invalid RDS_PORTAL_CAPTURE_MAX_FPS".into(),
                ));
            }
        };
        static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let permit = WORKERS
            .get_or_init(|| Arc::new(Semaphore::new(4)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| failure())?;
        let slots: Vec<_> = sources
            .iter()
            .map(|(index, _)| {
                Arc::new(Slot {
                    index: *index,
                    state: Mutex::new(State::default()),
                    changed: Condvar::new(),
                    readers: AtomicUsize::new(0),
                })
            })
            .collect();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let writing = slots.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _exit = WorkerExit(&writing);
            let result = run(video, &sources, &writing, &stopping, interval);
            if result.is_err() {
                tracing::warn!("Wayland PipeWire capture worker failed");
            }
            result
        });
        Ok(Self {
            slots,
            stop,
            task: Some(task),
        })
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        for slot in &self.slots {
            slot.fail();
        }
    }

    pub async fn close(&mut self) -> Result<(), DesktopError> {
        self.stop();
        if let Some(task) = self.task.take() {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .map_err(|_| failure())?
                .map_err(|_| failure())??;
        }
        Ok(())
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.stop();
    }
}

struct BudgetedPixels {
    pixels: bytes::Bytes,
    _permit: OwnedSemaphorePermit,
}
impl AsRef<[u8]> for BudgetedPixels {
    fn as_ref(&self) -> &[u8] {
        &self.pixels
    }
}

struct Data {
    format: spa::param::video::VideoInfoRaw,
    slot: Arc<Slot>,
    slots: Vec<Arc<Slot>>,
    budget: Arc<Semaphore>,
    interval: Option<Duration>,
    cadence: CopyCadence,
}

fn stream_format(interval: Option<Duration>) -> Result<Vec<u8>, DesktopError> {
    // Mutter uses variable framerate=0 and negotiates its work through
    // maxFramerate. Capping downstream copies alone still permits 60Hz paint.
    let maximum = interval.map_or(60, |period| {
        let nanos = period.as_nanos().max(1);
        ((1_000_000_000 + nanos / 2) / nanos).clamp(1, 60) as u32
    });
    let mut format = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::BGRx
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction {
                num: maximum.min(30),
                denom: 1
            },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: maximum,
                denom: 1
            }
        )
    );
    if interval.is_some() {
        format.properties.push(spa::pod::property!(
            spa::param::format::FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction {
                num: maximum,
                denom: 1
            },
            spa::utils::Fraction { num: 1, denom: 1 },
            spa::utils::Fraction {
                num: maximum,
                denom: 1
            }
        ));
    }
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(format),
    )
    .map_err(|_| failure())?
    .0
    .into_inner();
    Ok(bytes)
}

fn run(
    video: OwnedFd,
    sources: &[(u32, u32)],
    slots: &[Arc<Slot>],
    stop: &AtomicBool,
    interval: Option<Duration>,
) -> Result<(), DesktopError> {
    // Rc-backed PipeWire objects are created, used and destroyed on this worker.
    // No unsafe Send wrapper, foreign pointer or loop handle crosses threads.
    pw::init();
    let loop_ = pw::main_loop::MainLoopBox::new(None).map_err(|_| failure())?;
    let context = pw::context::ContextBox::new(loop_.loop_(), None).map_err(|_| failure())?;
    let core = context.connect_fd(video, None).map_err(|_| failure())?;
    let budget = Arc::new(Semaphore::new(MAX_RETAINED_BYTES as usize));
    let mut streams = Vec::with_capacity(sources.len());
    let mut listeners = Vec::with_capacity(sources.len());
    for ((_, node), slot) in sources.iter().zip(slots) {
        let stream = pw::stream::StreamBox::new(
            &core,
            "RDS monitor",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )
        .map_err(|_| failure())?;
        let listener = stream
            .add_local_listener_with_user_data(Data {
                format: Default::default(),
                slot: slot.clone(),
                slots: slots.to_vec(),
                budget: budget.clone(),
                interval,
                cadence: CopyCadence::default(),
            })
            .state_changed(|_, data, old, state| {
                if matches!(state, pw::stream::StreamState::Error(_))
                    || (state == pw::stream::StreamState::Unconnected
                        && old != pw::stream::StreamState::Unconnected)
                {
                    tracing::warn!(
                        unconnected = state == pw::stream::StreamState::Unconnected,
                        "Wayland PipeWire stream failed or disconnected"
                    );
                    data.slot.fail();
                }
            })
            .param_changed(|_, data, id, param| {
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    return;
                };
                if data.format.parse(param).is_err() {
                    data.slot.fail();
                    return;
                }
                let extent = (data.format.size().width, data.format.size().height);
                if crate::frame_bytes(extent.0 as usize, extent.1 as usize).is_none()
                    || !matches!(
                        data.format.format(),
                        spa::param::video::VideoFormat::BGRA | spa::param::video::VideoFormat::BGRx
                    )
                {
                    data.slot.fail();
                    return;
                }
                if let Ok(mut state) = data.slot.state.lock() {
                    if state.extent != Some(extent) {
                        state.image = None;
                        data.cadence = CopyCadence::default();
                    }
                    state.extent = Some(extent);
                }
                let total = data.slots.iter().try_fold(0usize, |total, slot| {
                    slot.extent()
                        .ok()
                        .flatten()
                        .and_then(|(w, h)| crate::frame_bytes(w as usize, h as usize))
                        .unwrap_or(0)
                        .checked_add(total)
                });
                if total.is_none_or(|total| total > MAX_PIXELS_BYTES) {
                    data.slot.fail();
                }
                data.slot.changed.notify_all();
            })
            .process(|stream, data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                // Retain one initial image for inventory/first presentation;
                // inactive monitor tabs must not copy full-resolution pixels
                // continuously. The native buffer still returns to PipeWire.
                if let Ok(state) = data.slot.state.lock()
                    && (state.failed
                        || (data.slot.readers.load(Ordering::Acquire) == 0
                            && state.image.is_some()))
                {
                    return;
                }
                // Dropping this dequeued buffer returns it to PipeWire even
                // when no owned pixels are copied. Gate before allocation.
                let copy_started = Instant::now();
                if !data.cadence.ready(copy_started) {
                    return;
                }
                let extent = (data.format.size().width, data.format.size().height);
                let Some(size) = crate::frame_bytes(extent.0 as usize, extent.1 as usize) else {
                    return;
                };
                let Ok(permit) = data.budget.clone().try_acquire_many_owned(size as u32) else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.len() != 1 {
                    data.slot.fail();
                    return;
                }
                let native = &mut datas[0];
                let (offset, size, stride) = (
                    native.chunk().offset(),
                    native.chunk().size(),
                    native.chunk().stride(),
                );
                // Empty buffers are legal while a stream pauses. They carry
                // no image and must not invalidate a consented monitor.
                if size == 0 {
                    return;
                }
                let Some(memory) = native.data() else {
                    data.slot.fail();
                    return;
                };
                let Ok(mut frame) = super::frame::copy_bgra(
                    memory,
                    offset,
                    size,
                    stride,
                    extent,
                    data.format.format() == spa::param::video::VideoFormat::BGRx,
                ) else {
                    data.slot.fail();
                    return;
                };
                frame.data = bytes::Bytes::from_owner(BudgetedPixels {
                    pixels: frame.data,
                    _permit: permit,
                });
                if let Ok(mut state) = data.slot.state.lock() {
                    if state.failed {
                        return;
                    }
                    data.cadence.copied(copy_started, data.interval);
                    state.generation = state.generation.saturating_add(1);
                    state.image = Some(Arc::new(Image {
                        frame,
                        received: Instant::now(),
                        generation: state.generation,
                    }));
                }
                data.slot.changed.notify_all();
            })
            .register()
            .map_err(|_| failure())?;
        let bytes = stream_format(interval)?;
        let pod = spa::pod::Pod::from_bytes(&bytes).ok_or_else(failure)?;
        stream
            .connect(
                spa::utils::Direction::Input,
                Some(*node),
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::DONT_RECONNECT,
                &mut [pod],
            )
            .map_err(|_| failure())?;
        listeners.push(listener);
        streams.push(stream);
    }
    while !stop.load(Ordering::Acquire) {
        if loop_
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)))
            < 0
        {
            return Err(failure());
        }
    }
    for stream in &streams {
        let _ = stream.disconnect();
    }
    drop(listeners);
    Ok(())
}

// Publish exit even when a native adapter panics outside a foreign callback.
// The source coordinator must never keep serving the last retained pixels.
struct WorkerExit<'a>(&'a [Arc<Slot>]);
impl Drop for WorkerExit<'_> {
    fn drop(&mut self) {
        for slot in self.0 {
            slot.fail();
        }
    }
}

#[cfg(test)]
mod exit_tests {
    use super::*;
    #[test]
    fn copy_cap_validates_and_preserves_default() {
        assert_eq!(copy_interval(None).unwrap(), None);
        assert_eq!(
            copy_interval(Some("15")).unwrap(),
            Some(Duration::from_secs_f64(1.0 / 15.0))
        );
        for value in ["0", "241", "", "no", "15.5"] {
            assert!(copy_interval(Some(value)).is_err());
        }
    }

    #[test]
    fn configured_cap_constrains_the_native_variable_rate_offer() {
        use spa::param::format::FormatProperties;
        use spa::pod::{ChoiceValue, Value, deserialize::PodDeserializer};
        use spa::utils::{Choice, ChoiceEnum, Fraction};
        for fps in 1..=240 {
            let bytes = stream_format(copy_interval(Some(&fps.to_string())).unwrap()).unwrap();
            let (_, Value::Object(object)) =
                PodDeserializer::deserialize_from::<Value>(&bytes).unwrap()
            else {
                panic!("expected format object")
            };
            for key in [
                FormatProperties::VideoFramerate,
                FormatProperties::VideoMaxFramerate,
            ] {
                let property = object
                    .properties
                    .iter()
                    .find(|p| p.key == key.as_raw())
                    .unwrap();
                let Value::Choice(ChoiceValue::Fraction(Choice(
                    _,
                    ChoiceEnum::Range { default, min, max },
                ))) = &property.value
                else {
                    panic!("expected bounded fraction")
                };
                assert_eq!(
                    *max,
                    Fraction {
                        num: fps.min(60),
                        denom: 1
                    }
                );
                assert!(default.num <= max.num);
                assert!(min.num <= default.num);
            }
            // Native decoding also accepts the actual bytes handed to PipeWire.
            let mut raw = spa::param::video::VideoInfoRaw::default();
            raw.parse(spa::pod::Pod::from_bytes(&bytes).unwrap())
                .unwrap();
        }
    }

    #[test]
    fn absent_cap_preserves_the_native_offer_without_a_new_maximum_property() {
        use spa::pod::{Value, deserialize::PodDeserializer};
        let bytes = stream_format(None).unwrap();
        let (_, Value::Object(object)) =
            PodDeserializer::deserialize_from::<Value>(&bytes).unwrap()
        else {
            panic!("expected format object")
        };
        assert!(
            !object
                .properties
                .iter()
                .any(|p| p.key == spa::param::format::FormatProperties::VideoMaxFramerate.as_raw())
        );
    }

    #[test]
    fn copy_cadence_skips_intermediate_callbacks_and_resumes_without_burst() {
        let start = Instant::now();
        let interval = Duration::from_millis(66);
        let mut cadence = CopyCadence::default();
        assert!(cadence.ready(start));
        cadence.copied(start, Some(interval));
        assert!(!cadence.ready(start + Duration::from_millis(33)));
        // A late callback must not shift every subsequent deadline later.
        let late = start + Duration::from_millis(70);
        assert!(cadence.ready(late));
        cadence.copied(late, Some(interval));
        assert!(cadence.ready(start + interval * 2));
        let resumed = start + Duration::from_secs(2);
        assert!(cadence.ready(resumed));
        cadence.copied(resumed, Some(interval));
        assert!(!cadence.ready(resumed));
        assert!(cadence.ready(resumed + interval));
        // Resize resets the cadence, allowing the first new-geometry image.
        cadence = CopyCadence::default();
        assert!(cadence.ready(resumed));
        cadence.copied(resumed, None);
        assert!(cadence.ready(resumed));
    }

    #[test]
    fn native_worker_unwind_invalidates_retained_monitor_inventory() {
        let slots = vec![Slot::fixture(10, (3840, 2160))];
        let result = std::panic::catch_unwind(|| {
            let _exit = WorkerExit(&slots);
            panic!("fixture native worker fault");
        });
        assert!(result.is_err());
        assert!(slots[0].extent().is_err());
        assert!(slots[0].image(Duration::ZERO).is_err());
    }
}
